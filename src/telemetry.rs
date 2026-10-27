//! Optional, bounded monitoring. Request threads never perform journal I/O.
mod journal;
mod preview;
mod wire;

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub use journal::export;

const MAX_QUERY_BYTES: usize = 64 * 1024;
const MAX_STAGES: usize = 128;
const MAX_LABEL_BYTES: usize = 4096;
const QUEUE_BYTES: u64 = 8 * 1024 * 1024;
static RECORDER: OnceLock<Mutex<Option<Arc<Recorder>>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub waiting: bool,
    pub duration_ms: u64,
}

/// A known cause, suitable for public monitoring exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Diagnostic {
    MaintenanceBusy,
    WorkspaceRetired,
    RefreshRequired,
    RepositoryTransfer,
    RequestCancelled,
    Shutdown,
    Superseded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub kind: String,
    pub repository: Option<String>,
    pub target: Option<String>,
    pub query: Option<String>,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub status: String,
    pub detailed: bool,
    pub stages: Vec<Stage>,
    pub links: Vec<String>,
    pub current_stage: Option<String>,
    pub waiting: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskSample {
    pub timestamp_ms: u64,
    pub total_bytes: u64,
    pub repositories: BTreeMap<String, u64>,
    pub shared: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub day: u64,
    pub kind: String,
    pub repository: Option<String>,
    pub count: u64,
    pub total_duration_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Export {
    pub version: u32,
    pub cursor: String,
    pub more: bool,
    pub reset: bool,
    pub replace_days: Vec<u64>,
    pub operations: Vec<Record>,
    pub active: Vec<Record>,
    pub disk: Vec<DiskSample>,
    pub summaries: Vec<Summary>,
    /// Cumulative number of events omitted because monitoring reached its bounds.
    pub gaps: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Event {
    Operation(Record),
    Disk(DiskSample),
    Summary(Summary),
}

// The bounded queue stores events inline to avoid an allocation on every update.
#[allow(clippy::large_enum_variant)]
enum Message {
    Event(Event, u64),
    Stop,
}

struct Recorder {
    sender: mpsc::SyncSender<Message>,
    queued_bytes: AtomicU64,
    gaps: AtomicU64,
    next_id: AtomicU64,
    run_id: String,
}

impl Recorder {
    fn send(&self, event: Event) {
        let bytes = match &event {
            Event::Operation(record) => {
                256 + record.query.as_ref().map_or(0, String::len)
                    + record.kind.len()
                    + record.repository.as_ref().map_or(0, String::len)
                    + record.target.as_ref().map_or(0, String::len)
                    + record.current_stage.as_ref().map_or(0, String::len)
                    + record.waiting.iter().map(String::len).sum::<usize>()
                    + record
                        .stages
                        .iter()
                        .map(|stage| 48 + stage.name.len())
                        .sum::<usize>()
                    + record.links.iter().map(String::len).sum::<usize>()
            }
            Event::Disk(sample) => {
                64 + sample
                    .repositories
                    .iter()
                    .chain(&sample.shared)
                    .map(|(name, _)| name.len() + 32)
                    .sum::<usize>()
            }
            Event::Summary(summary) => {
                64 + summary.kind.len() + summary.repository.as_ref().map_or(0, String::len)
            }
        } as u64;
        if self
            .queued_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
                queued
                    .checked_add(bytes)
                    .filter(|&total| total <= QUEUE_BYTES)
            })
            .is_err()
        {
            self.gaps.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.sender.try_send(Message::Event(event, bytes)).is_err() {
            self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
            self.gaps.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Keeps the recorder alive and flushes accepted events on orderly shutdown.
pub struct Guard {
    recorder: Arc<Recorder>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(global) = RECORDER.get() {
            let mut global = global.lock().unwrap_or_else(|error| error.into_inner());
            if global
                .as_ref()
                .is_some_and(|value| Arc::ptr_eq(value, &self.recorder))
            {
                *global = None;
            }
        }
        let _ = self.recorder.sender.send(Message::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn init(directory: &Path) -> anyhow::Result<Guard> {
    let mut global = RECORDER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    anyhow::ensure!(global.is_none(), "monitoring is already initialized");
    let writer = journal::Writer::open(directory)?;
    let run_id = writer.run_id().to_owned();
    let (sender, receiver) = mpsc::sync_channel(1024);
    let recorder = Arc::new(Recorder {
        sender,
        queued_bytes: AtomicU64::new(0),
        gaps: AtomicU64::new(writer.gaps()),
        next_id: AtomicU64::new(1),
        run_id,
    });
    let weak = Arc::downgrade(&recorder);
    let worker = std::thread::Builder::new()
        .name("sigla-monitor".into())
        .spawn(move || writer.run(receiver, weak))?;
    *global = Some(recorder.clone());
    Ok(Guard {
        recorder,
        worker: Some(worker),
    })
}

fn recorder() -> Option<Arc<Recorder>> {
    RECORDER
        .get()?
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub fn enabled() -> bool {
    recorder().is_some()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

struct Running {
    record: Record,
    query: Option<String>,
    started: Instant,
    current_stage: Option<(String, u64)>,
    waits: BTreeMap<u64, (String, u64, Instant)>,
    next_wait: u64,
    wait_ms: u64,
    finished: bool,
}

struct OperationInner {
    recorder: Weak<Recorder>,
    state: Mutex<Running>,
}

impl OperationInner {
    fn update(&self, update: impl FnOnce(&mut Running)) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.finished {
            return;
        }
        update(&mut state);
        if let Some(recorder) = self.recorder.upgrade() {
            recorder.send(Event::Operation(state.record.clone()));
        }
    }

    fn finish(&self, status: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.finished {
            return;
        }
        state.finished = true;
        state.record.current_stage = None;
        state.record.waiting.clear();
        let elapsed = state.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let end_ms = state.record.start_ms.saturating_add(elapsed);
        if let Some((name, start_ms)) = state.current_stage.take() {
            push_stage(
                &mut state,
                Stage {
                    name,
                    start_ms,
                    end_ms,
                    waiting: false,
                    duration_ms: end_ms.saturating_sub(start_ms),
                },
                &self.recorder,
            );
        }
        for (_, (name, start_ms, started)) in std::mem::take(&mut state.waits) {
            let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            state.wait_ms = state.wait_ms.saturating_add(duration_ms);
            push_stage(
                &mut state,
                Stage {
                    name,
                    start_ms,
                    end_ms,
                    waiting: true,
                    duration_ms,
                },
                &self.recorder,
            );
        }
        state.record.end_ms = Some(end_ms);
        state.record.status = status.to_owned();
        state.record.detailed =
            elapsed >= 5_000 || state.wait_ms >= 1_000 || !matches!(status, "success" | "noop");
        if state.record.detailed {
            state.record.query = state.query.take();
        } else {
            state.record.stages.clear();
        }
        if let Some(recorder) = self.recorder.upgrade() {
            recorder.send(Event::Operation(state.record.clone()));
        }
    }
}

impl Drop for OperationInner {
    fn drop(&mut self) {
        self.finish("cancelled");
    }
}

/// Clones share one operation. The last uncompleted handle records cancellation.
#[derive(Clone, Default)]
pub struct Operation(Option<Arc<OperationInner>>);

pub fn operation(kind: &str, repository: Option<&str>, query: Option<&str>) -> Operation {
    let Some(recorder) = recorder() else {
        return Operation::default();
    };
    operation_using(recorder, kind, repository, query)
}

fn operation_using(
    recorder: Arc<Recorder>,
    kind: &str,
    repository: Option<&str>,
    query: Option<&str>,
) -> Operation {
    let query = query.map(|query| {
        if query.len() > MAX_QUERY_BYTES {
            recorder.gaps.fetch_add(1, Ordering::Relaxed);
            preview::truncate(query, MAX_QUERY_BYTES)
        } else {
            query.to_owned()
        }
    });
    let mut record = Record {
        id: format!(
            "{}-{}",
            recorder.run_id,
            recorder.next_id.fetch_add(1, Ordering::Relaxed)
        ),
        kind: bounded_label(&recorder, kind),
        repository: repository.map(|value| bounded_label(&recorder, value)),
        target: None,
        query: query.clone(),
        start_ms: now_ms(),
        end_ms: None,
        status: "running".into(),
        detailed: false,
        stages: Vec::new(),
        links: Vec::new(),
        current_stage: None,
        waiting: Vec::new(),
        diagnostic: None,
    };
    recorder.send(Event::Operation(record.clone()));
    record.query = query.as_deref().map(preview::summarize);
    Operation(Some(Arc::new(OperationInner {
        recorder: Arc::downgrade(&recorder),
        state: Mutex::new(Running {
            record,
            query,
            started: Instant::now(),
            current_stage: None,
            waits: BTreeMap::new(),
            next_wait: 0,
            wait_ms: 0,
            finished: false,
        }),
    })))
}

fn bounded_label(recorder: &Recorder, value: &str) -> String {
    if value.len() > MAX_LABEL_BYTES {
        recorder.gaps.fetch_add(1, Ordering::Relaxed);
        preview::truncate(value, MAX_LABEL_BYTES)
    } else {
        value.to_owned()
    }
}

fn push_stage(state: &mut Running, stage: Stage, recorder: &Weak<Recorder>) {
    if stage.waiting
        && let Some(previous) = state
            .record
            .stages
            .iter_mut()
            .find(|previous| previous.waiting && previous.name == stage.name)
    {
        previous.end_ms = previous.end_ms.max(stage.end_ms);
        previous.duration_ms = previous.duration_ms.saturating_add(stage.duration_ms);
        return;
    }
    if state.record.stages.len() < MAX_STAGES {
        state.record.stages.push(stage);
    } else if let Some(recorder) = recorder.upgrade() {
        recorder.gaps.fetch_add(1, Ordering::Relaxed);
    }
}

impl Operation {
    /// Record a verified cause without exposing error messages to monitoring clients.
    pub fn diagnose(&self, diagnostic: Diagnostic) {
        if let Some(inner) = &self.0 {
            inner.update(|state| state.record.diagnostic = Some(diagnostic));
        }
    }

    pub fn repository(&self, value: &str) {
        if let Some(inner) = &self.0
            && let Some(recorder) = inner.recorder.upgrade()
        {
            inner.update(|state| state.record.repository = Some(bounded_label(&recorder, value)));
        }
    }

    pub fn target(&self, value: &str) {
        if let Some(inner) = &self.0
            && let Some(recorder) = inner.recorder.upgrade()
        {
            inner.update(|state| state.record.target = Some(bounded_label(&recorder, value)));
        }
    }

    pub fn id(&self) -> Option<String> {
        self.0.as_ref().map(|inner| {
            inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record
                .id
                .clone()
        })
    }

    pub fn stage(&self, name: &str) {
        if let Some(inner) = &self.0 {
            inner.update(|state| {
                let now = state
                    .record
                    .start_ms
                    .saturating_add(state.started.elapsed().as_millis() as u64);
                if let Some((name, start_ms)) = state.current_stage.take() {
                    push_stage(
                        state,
                        Stage {
                            name,
                            start_ms,
                            end_ms: now,
                            waiting: false,
                            duration_ms: now.saturating_sub(start_ms),
                        },
                        &inner.recorder,
                    );
                }
                state.current_stage = Some((name.to_owned(), now));
                state.record.current_stage = Some(name.to_owned());
            });
        }
    }

    pub fn wait(&self, name: &str) -> StageGuard {
        let mut id = 0;
        if let Some(inner) = &self.0 {
            inner.update(|state| {
                state.next_wait += 1;
                id = state.next_wait;
                if state.record.waiting.len() < MAX_STAGES {
                    state.record.waiting.push(name.to_owned());
                    let now = state
                        .record
                        .start_ms
                        .saturating_add(state.started.elapsed().as_millis() as u64);
                    state
                        .waits
                        .insert(id, (name.to_owned(), now, Instant::now()));
                } else if let Some(recorder) = inner.recorder.upgrade() {
                    recorder.gaps.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        StageGuard {
            inner: self.0.clone(),
            id,
        }
    }

    pub fn wait_ms(&self, name: &str, duration_ms: u64) {
        if let Some(inner) = &self.0 {
            inner.update(|state| {
                let end_ms = state
                    .record
                    .start_ms
                    .saturating_add(state.started.elapsed().as_millis() as u64);
                state.wait_ms = state.wait_ms.saturating_add(duration_ms);
                push_stage(
                    state,
                    Stage {
                        name: name.to_owned(),
                        start_ms: end_ms.saturating_sub(duration_ms),
                        end_ms,
                        waiting: true,
                        duration_ms,
                    },
                    &inner.recorder,
                );
            });
        }
    }

    pub fn link(&self, id: &str) {
        if let Some(inner) = &self.0 {
            inner.update(|state| {
                if state.record.links.len() < MAX_STAGES
                    && !state.record.links.iter().any(|link| link == id)
                {
                    state.record.links.push(id.to_owned());
                }
            });
        }
    }

    pub fn finish(self, status: &str) {
        if let Some(inner) = &self.0 {
            inner.finish(status);
        }
    }
}

pub struct StageGuard {
    inner: Option<Arc<OperationInner>>,
    id: u64,
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(inner) = &self.inner {
            inner.update(|state| {
                let Some((name, start_ms, started)) = state.waits.remove(&self.id) else {
                    return;
                };
                let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                state.wait_ms = state.wait_ms.saturating_add(elapsed);
                if let Some(index) = state
                    .record
                    .waiting
                    .iter()
                    .position(|waiting| waiting == &name)
                {
                    state.record.waiting.remove(index);
                }
                push_stage(
                    state,
                    Stage {
                        name,
                        start_ms,
                        end_ms: start_ms.saturating_add(elapsed),
                        waiting: true,
                        duration_ms: elapsed,
                    },
                    &inner.recorder,
                );
            });
        }
    }
}

pub fn disk_sample(
    total_bytes: u64,
    repositories: BTreeMap<String, u64>,
    shared: BTreeMap<String, u64>,
) {
    disk_sample_at(now_ms(), total_bytes, repositories, shared);
}

pub fn disk_sample_at(
    timestamp_ms: u64,
    total_bytes: u64,
    repositories: BTreeMap<String, u64>,
    shared: BTreeMap<String, u64>,
) {
    if let Some(recorder) = recorder() {
        recorder.send(Event::Disk(DiskSample {
            timestamp_ms,
            total_bytes,
            repositories,
            shared,
        }));
    }
}

pub fn record<T, E>(
    kind: &str,
    repository: Option<&str>,
    query: Option<&str>,
    run: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let operation = operation(kind, repository, query);
    let result = run();
    operation.finish(if result.is_ok() { "success" } else { "failure" });
    result
}

#[cfg(test)]
pub(crate) struct TestRecorder {
    _recorder: Arc<Recorder>,
    receiver: mpsc::Receiver<Message>,
}

#[cfg(test)]
impl TestRecorder {
    pub(crate) fn completed(&self) -> Vec<Record> {
        self.receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Event(Event::Operation(record), _) if record.end_ms.is_some() => {
                    Some(record)
                }
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) fn test_operation() -> (Operation, TestRecorder) {
    let (sender, receiver) = mpsc::sync_channel(32);
    let recorder = Arc::new(Recorder {
        sender,
        queued_bytes: AtomicU64::new(0),
        gaps: AtomicU64::new(0),
        next_id: AtomicU64::new(1),
        run_id: "test".into(),
    });
    let operation = operation_using(recorder.clone(), "search", None, None);
    (
        operation,
        TestRecorder {
            _recorder: recorder,
            receiver,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder(capacity: usize) -> (Arc<Recorder>, mpsc::Receiver<Message>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        (
            Arc::new(Recorder {
                sender,
                queued_bytes: AtomicU64::new(0),
                gaps: AtomicU64::new(0),
                next_id: AtomicU64::new(1),
                run_id: "test".into(),
            }),
            receiver,
        )
    }

    fn completed(receiver: &mpsc::Receiver<Message>) -> Vec<Record> {
        receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Event(Event::Operation(record), _) if record.end_ms.is_some() => {
                    Some(record)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn known_diagnostic_is_exported_without_a_message() {
        let (recorder, receiver) = recorder(32);
        let operation = operation_using(recorder.clone(), "search", None, None);
        operation.diagnose(Diagnostic::MaintenanceBusy);
        operation.finish("failure");
        let records = completed(&receiver);
        let json = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(json["diagnostic"], "maintenance_busy");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn clones_finish_once_and_fast_success_has_a_preview() {
        let (recorder, receiver) = recorder(32);
        let query = format!(
            "calls:Very.Long.Namespace.Parser.Parse({}) path:src/** wait:complete",
            "LongType,".repeat(20)
        );
        let operation = operation_using(recorder.clone(), "search", Some("repo"), Some(&query));
        let clone = operation.clone();
        operation.stage("validation");
        operation.finish("success");
        clone.finish("failure");
        let records = completed(&receiver);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, "success");
        assert!(!records[0].detailed);
        assert!(records[0].stages.is_empty());
        assert!(records[0].query.as_ref().unwrap().len() < query.len());
    }

    #[test]
    fn waits_promote_full_queries_and_accumulate_by_name() {
        let (recorder, receiver) = recorder(32);
        let query = "text:\"some full query\"".repeat(20);
        let operation = operation_using(recorder.clone(), "search", None, Some(&query));
        operation.wait_ms("writer", 600);
        operation.wait_ms("writer", 600);
        operation.finish("success");
        let records = completed(&receiver);
        assert!(records[0].detailed);
        assert_eq!(records[0].query.as_deref(), Some(query.as_str()));
        assert_eq!(records[0].stages.len(), 1);
        assert_eq!(records[0].stages[0].duration_ms, 1200);
    }

    #[test]
    fn cancellation_captures_an_open_wait() {
        let (recorder, receiver) = recorder(32);
        let operation = operation_using(recorder.clone(), "search", None, None);
        let wait = operation.wait("shared preparation");
        operation.finish("cancelled");
        drop(wait);
        let records = completed(&receiver);
        assert_eq!(records.len(), 1);
        assert!(records[0].detailed);
        assert!(
            serde_json::to_value(&records[0])
                .unwrap()
                .get("diagnostic")
                .is_none()
        );
        assert!(records[0].stages.iter().any(|stage| stage.waiting));
        assert!(records[0].waiting.is_empty());
    }

    #[test]
    fn a_saturated_queue_drops_without_blocking_and_counts_the_gap() {
        let (recorder, _receiver) = recorder(1);
        let operation = operation_using(recorder.clone(), "search", None, None);
        operation.finish("success");
        assert!(recorder.gaps.load(Ordering::Relaxed) > 0);
        assert!(recorder.queued_bytes.load(Ordering::Relaxed) <= QUEUE_BYTES);
    }

    #[test]
    #[ignore = "measures recorder cost and persisted size for capacity planning"]
    fn monitoring_performance_probe() {
        let directory = tempfile::tempdir().unwrap();
        let writer = journal::Writer::open(directory.path()).unwrap();
        let (recorder, receiver) = recorder(1024);
        let weak = Arc::downgrade(&recorder);
        let worker = std::thread::spawn(move || writer.run(receiver, weak));
        let mut producer = std::time::Duration::ZERO;
        let started = Instant::now();
        for index in 0..100_000 {
            let query = format!(
                "symbol:Type{}.Method{} path:src/**",
                index % 100,
                index % 23
            );
            let before = Instant::now();
            operation_using(
                recorder.clone(),
                "search",
                Some("https://github.com/example/project"),
                Some(&query),
            )
            .finish("success");
            producer += before.elapsed();
            if index % 100 == 99 {
                while recorder.queued_bytes.load(Ordering::Relaxed) != 0 {
                    std::thread::yield_now();
                }
            }
        }
        recorder.sender.send(Message::Stop).unwrap();
        worker.join().unwrap();
        let total_bytes: u64 = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        let before = Instant::now();
        let mut page = export(directory.path(), None).unwrap();
        let mut count = page.operations.len();
        while page.more {
            page = export(directory.path(), Some(&page.cursor)).unwrap();
            count += page.operations.len();
        }
        let full_export = before.elapsed();
        let before = Instant::now();
        let incremental = export(directory.path(), Some(&page.cursor)).unwrap();
        eprintln!(
            "operations={count} gaps={} producer_ns/op={} total_ms={} bytes/op={:.1} full_export_ms={} unchanged_export_us={}",
            page.gaps,
            producer.as_nanos() / 100_000,
            started.elapsed().as_millis(),
            total_bytes as f64 / count as f64,
            full_export.as_millis(),
            before.elapsed().as_micros()
        );
        assert_eq!(count, 100_000);
        assert_eq!(page.gaps, 0);
        assert!(incremental.operations.is_empty());
    }
}
