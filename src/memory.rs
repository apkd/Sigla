//! memory admission and reclamation for completed work and idle caches.
use crate::cancellation::Cancellation;
use std::sync::{Condvar, Mutex, OnceLock};
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

const MIB: u64 = 1024 * 1024;
static PARSER_BYTES: OnceLock<u64> = OnceLock::new();
static PARSING: OnceLock<Budget> = OnceLock::new();
static SOURCES: OnceLock<Budget> = OnceLock::new();

pub(crate) fn configure_parser_memory(mib: Option<u64>) -> anyhow::Result<()> {
    let bytes = match mib {
        Some(mib) => mib
            .checked_mul(MIB)
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| anyhow::anyhow!("Parser memory must be a positive MiB count"))?,
        None => automatic_parser_budget(),
    };
    if let Err(bytes) = PARSER_BYTES.set(bytes) {
        anyhow::ensure!(
            PARSER_BYTES.get() == Some(&bytes),
            "Parser memory was already configured"
        );
    }
    tracing::info!(
        parser_memory_bytes = bytes,
        source_memory_bytes = (bytes / 4).max(1),
        parser_workers = parser_workers(),
        "Resource admission configured"
    );
    Ok(())
}

fn limit_from_cgroup(
    mut limit: Option<u64>,
    group: &std::path::Path,
    root: &std::path::Path,
) -> Option<u64> {
    let mut directory = group.to_path_buf();
    while directory.starts_with(root) {
        for name in ["memory.high", "memory.max"] {
            if let Ok(value) = std::fs::read_to_string(directory.join(name))
                && let Ok(bytes) = value.trim().parse::<u64>()
            {
                limit = Some(limit.map_or(bytes, |limit| limit.min(bytes)));
            }
        }
        if !directory.pop() {
            break;
        }
    }
    limit
}

fn memory_limit() -> Option<u64> {
    let physical = unsafe {
        let pages = u64::try_from(libc::sysconf(libc::_SC_PHYS_PAGES)).ok();
        let page_size = u64::try_from(libc::sysconf(libc::_SC_PAGESIZE)).ok();
        pages
            .zip(page_size)
            .and_then(|(pages, size)| pages.checked_mul(size))
            .filter(|bytes| *bytes > 0)
    };
    if let Ok(groups) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(group) = groups.lines().find_map(|line| line.strip_prefix("0::"))
    {
        let root = std::path::Path::new("/sys/fs/cgroup");
        return limit_from_cgroup(physical, &root.join(group.trim_start_matches('/')), root);
    }
    physical
}

fn automatic_parser_budget() -> u64 {
    memory_limit().map_or(256 * MIB, |bytes| (bytes / 4).clamp(1, 4096 * MIB))
}

fn parser_bytes() -> u64 {
    *PARSER_BYTES.get_or_init(automatic_parser_budget)
}

fn parser_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
}

/// Budget cached heap independently of mapped database pages and active jobs.
pub fn cache_budget() -> usize {
    static BYTES: OnceLock<usize> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let mut limit = 2 * 1024 * 1024 * 1024u64;
        if let Ok(groups) = std::fs::read_to_string("/proc/self/cgroup")
            && let Some(group) = groups.lines().find_map(|line| line.strip_prefix("0::"))
        {
            let root = std::path::Path::new("/sys/fs/cgroup");
            let mut directory = root.join(group.trim_start_matches('/'));
            while directory.starts_with(root) {
                for name in ["memory.high", "memory.max"] {
                    if let Ok(value) = std::fs::read_to_string(directory.join(name))
                        && let Ok(bytes) = value.trim().parse::<u64>()
                    {
                        limit = limit.min(bytes);
                    }
                }
                if !directory.pop() {
                    break;
                }
            }
        }
        (limit / 4).clamp(32 * 1024 * 1024, 512 * 1024 * 1024) as usize
    })
}

/// Count serialized data without allocating a second copy. Call once when a
/// cache entry is published; containers also retain allocation overhead.
pub fn encoded_size(value: &impl serde::Serialize) -> usize {
    struct Counter(usize);
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    postcard::to_io(value, Counter(0))
        .expect("serializable cache entry")
        .0
}

// leave room for two active jobs and temporary parser allocations.
pub const IDLE_CACHE_HIGH_WATER: u64 = 640 * 1024 * 1024;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const INDEXING_FREE_HIGH_WATER: usize = 64 * 1024 * 1024;

pub struct Budget {
    capacity: u64,
    workers: usize,
    state: Mutex<AdmissionState>,
    changed: Condvar,
}
struct AdmissionState {
    available: u64,
    workers: usize,
    next_ticket: u64,
    waiting: VecDeque<(u64, Instant)>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AdmissionStats {
    pub capacity_bytes: u64,
    pub reserved_bytes: u64,
    pub worker_limit: usize,
    pub active_workers: usize,
    pub waiting: usize,
    pub oldest_wait_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Resources {
    pub parsing: AdmissionStats,
    pub sources: AdmissionStats,
}

pub fn resources() -> Resources {
    Resources {
        parsing: parsing().stats(),
        sources: sources().stats(),
    }
}

impl Budget {
    fn new(capacity: u64, workers: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            workers,
            state: Mutex::new(AdmissionState {
                available: capacity,
                workers,
                next_ticket: 0,
                waiting: VecDeque::new(),
            }),
            changed: Condvar::new(),
        }
    }
    fn reserve(
        &self,
        bytes: u64,
        worker: bool,
        cancel: &dyn Cancellation,
    ) -> anyhow::Result<Reservation<'_>> {
        assert!(!worker || self.workers > 0);
        // An oversized input runs alone. FIFO admission also lets a large
        // pending input make progress under a stream of small inputs.
        let bytes = bytes.max(1).min(self.capacity);
        let mut state = self.state.lock().unwrap();
        cancel.check()?;
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        state.waiting.push_back((ticket, Instant::now()));
        self.changed.notify_all();
        loop {
            if let Err(error) = cancel.check() {
                state.waiting.retain(|(id, _)| *id != ticket);
                self.changed.notify_all();
                return Err(error);
            }
            if state.waiting.front().is_some_and(|(id, _)| *id == ticket)
                && state.available >= bytes
                && (!worker || state.workers > 0)
            {
                state.waiting.pop_front();
                state.available -= bytes;
                state.workers -= usize::from(worker);
                self.changed.notify_all();
                return Ok(Reservation {
                    budget: self,
                    bytes,
                    worker,
                });
            }
            // AtomicBool callers cannot signal a condvar on cancellation.
            // Bound the wait without waking busy workers or allocating a task.
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    fn stats(&self) -> AdmissionStats {
        let state = self.state.lock().unwrap();
        AdmissionStats {
            capacity_bytes: self.capacity,
            reserved_bytes: self.capacity - state.available,
            worker_limit: self.workers,
            active_workers: self.workers - state.workers,
            waiting: state.waiting.len(),
            oldest_wait_ms: state
                .waiting
                .front()
                .map_or(0, |(_, start)| start.elapsed().as_millis() as u64),
        }
    }
}
pub struct Reservation<'a> {
    budget: &'a Budget,
    bytes: u64,
    worker: bool,
}
impl Reservation<'_> {
    pub fn release_worker(&mut self) {
        if self.worker {
            self.worker = false;
            self.budget.state.lock().unwrap().workers += 1;
            self.budget.changed.notify_all();
        }
    }
    /// Release parser working space while retaining the publication estimate.
    /// Never acquire more budget while holding another reservation.
    pub fn shrink_to(&mut self, bytes: u64) {
        let retained = bytes.min(self.bytes);
        let released = self.bytes - retained;
        if released != 0 {
            self.bytes = retained;
            self.budget.state.lock().unwrap().available += released;
            self.budget.changed.notify_all();
        }
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock().unwrap();
        state.available += self.bytes;
        state.workers += usize::from(self.worker);
        self.budget.changed.notify_all();
    }
}

fn parsing() -> &'static Budget {
    PARSING.get_or_init(|| Budget::new(parser_bytes(), parser_workers()))
}

pub(crate) fn admit_file(
    bytes: u64,
    assembly: bool,
    cancel: &dyn Cancellation,
) -> anyhow::Result<Reservation<'static>> {
    // account for syntax nodes, owned facts, compression, and publication buffers.
    // these are admission estimates, not a bound on an arbitrary parser's heap.
    parsing().reserve(
        bytes.saturating_mul(if assembly { 8 } else { 32 }),
        true,
        cancel,
    )
}

pub(crate) fn admit_batch(
    total: u64,
    largest: u64,
    encoded_limit: u64,
    assembly: bool,
    cancel: &dyn Cancellation,
) -> anyhow::Result<Reservation<'static>> {
    let expansion = if assembly { 8 } else { 32 };
    // A worker parses one file at a time, retaining bounded encodings and posting
    // names for publication. Reserving syntax trees for every file serializes
    // large batches even when their actual live memory fits several workers.
    parsing().reserve(
        total.saturating_mul(expansion).min(
            largest
                .saturating_mul(expansion)
                .saturating_add(encoded_limit.saturating_mul(8)),
        ),
        true,
        cancel,
    )
}

fn sources() -> &'static Budget {
    SOURCES.get_or_init(|| Budget::new((parser_bytes() / 4).max(1), 0))
}

pub(crate) fn admit_source(
    bytes: u64,
    cancel: &dyn Cancellation,
) -> anyhow::Result<Reservation<'static>> {
    sources().reserve(bytes.saturating_mul(4), false, cancel)
}

/// Source buffers keep their admission through decoding and rendering.
pub(crate) struct Source<T> {
    value: T,
    reservation: Reservation<'static>,
}

impl<T> Source<T> {
    pub fn read(
        bytes: u64,
        cancel: &dyn Cancellation,
        reader: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Self> {
        let reservation = admit_source(bytes, cancel)?;
        let value = reader()?;
        cancel.check()?;
        Ok(Self { value, reservation })
    }

    pub fn map<U>(self, map: impl FnOnce(T) -> anyhow::Result<U>) -> anyhow::Result<Source<U>> {
        Ok(Source {
            value: map(self.value)?,
            reservation: self.reservation,
        })
    }
}

impl<T> std::ops::Deref for Source<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn resident(field: &str) -> io::Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| io::Error::other("Missing resident memory accounting"))?;
    Ok(kib * 1024)
}

pub fn reclaim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Ok(resident) = resident("RssAnon:") {
        // Allocated bytes may include untouched mappings, so they cannot be
        // subtracted from resident bytes to estimate reclaimable memory.
        if resident > IDLE_CACHE_HIGH_WATER {
            release_unused_pages();
        }
    }
}

pub fn reclaim_after_indexing() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if unsafe { libc::mallinfo2().fordblks } >= INDEXING_FREE_HIGH_WATER {
        // Run once after a batch: trimmed arena pages still count as free, so
        // polling this counter would repeatedly trim the same arenas.
        release_unused_pages();
    }
}

pub fn start_reclaimer() -> io::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        static STARTED: OnceLock<io::Result<()>> = OnceLock::new();
        STARTED
            .get_or_init(|| {
                std::thread::Builder::new()
                    .name("sigla-memory".into())
                    .spawn(|| {
                        loop {
                            // An active request can stall before reaching its cleanup.
                            // Reclaim independently of request completion and Tokio.
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            reclaim();
                        }
                    })
                    .map(drop)
            })
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
    }
    Ok(())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn release_unused_pages() {
    // freeing parser and query buffers leaves pages in glibc's thread arenas.
    // return those pages while requests are active and before evicting caches.
    unsafe { libc::malloc_trim(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn shrinking_a_reservation_releases_capacity_before_publication_finishes() {
        let budget = Budget::new(10, 1);
        let mut held = budget.reserve(10, true, &()).unwrap();
        held.release_worker();
        std::thread::scope(|threads| {
            let (admitted, ready) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel();
            let worker_budget = &budget;
            threads.spawn(move || {
                let _next = worker_budget.reserve(6, true, &()).unwrap();
                admitted.send(()).unwrap();
                released.recv().unwrap();
            });
            assert!(ready.try_recv().is_err());
            held.shrink_to(4);
            let result = ready.recv_timeout(std::time::Duration::from_secs(10));
            held.shrink_to(8); // Shrinking must never acquire additional budget.
            assert_eq!(held.bytes, 4);
            drop(held);
            release.send(()).unwrap();
            assert!(
                result.is_ok(),
                "Released parser capacity did not wake the next worker"
            );
        });
        assert_eq!(budget.stats().reserved_bytes, 0);
        assert_eq!(budget.stats().active_workers, 0);
    }

    #[test]
    fn concurrent_reservations_bound_work_and_release_capacity() {
        let budget = Budget::new(12, 2);
        let active = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..32 {
                        let _reservation = budget.reserve(3, true, &()).unwrap();
                        let count = active.fetch_add(3, Ordering::SeqCst) + 3;
                        peak.fetch_max(count, Ordering::SeqCst);
                        std::thread::yield_now();
                        active.fetch_sub(3, Ordering::SeqCst);
                    }
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= budget.capacity);
        assert_eq!(budget.stats().reserved_bytes, 0);
        assert_eq!(budget.stats().active_workers, 0);
        let _oversized = budget.reserve(budget.capacity + 1, true, &()).unwrap();
        assert_eq!(budget.stats().reserved_bytes, budget.capacity);
    }

    fn wait_for_waiters(budget: &Budget, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = budget.state.lock().unwrap();
        while state.waiting.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "Worker did not enter admission");
            state = budget.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }

    #[test]
    fn admission_never_reserves_only_one_resource_and_cancellation_unqueues() {
        use std::sync::atomic::AtomicBool;
        for retain_worker in [false, true] {
            let budget = Budget::new(12, 1);
            let mut held = budget
                .reserve(if retain_worker { 6 } else { 12 }, true, &())
                .unwrap();
            if !retain_worker {
                held.release_worker();
            }
            let cancel = AtomicBool::new(false);
            std::thread::scope(|threads| {
                let (send, receive) = std::sync::mpsc::channel();
                let worker_budget = &budget;
                let worker_cancel = &cancel;
                threads.spawn(move || {
                    let result = worker_budget.reserve(6, true, worker_cancel);
                    send.send(result.is_err()).unwrap();
                });
                wait_for_waiters(&budget, 1);
                let stats = budget.stats();
                assert_eq!(stats.reserved_bytes, held.bytes);
                assert_eq!(stats.active_workers, usize::from(retain_worker));
                cancel.store(true, Ordering::Release);
                let result = receive.recv_timeout(Duration::from_secs(5));
                drop(held);
                assert!(result.unwrap());
            });
            assert_eq!(budget.stats().reserved_bytes, 0);
            assert_eq!(budget.stats().active_workers, 0);
            assert_eq!(budget.stats().waiting, 0);
        }
    }

    #[test]
    fn cancelling_a_large_head_waiter_allows_the_next_request() {
        use std::sync::atomic::AtomicBool;
        let budget = Budget::new(10, 2);
        let held = budget.reserve(6, true, &()).unwrap();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|threads| {
            let large = threads.spawn(|| budget.reserve(8, true, &cancel));
            wait_for_waiters(&budget, 1);
            let (send, receive) = std::sync::mpsc::channel();
            let worker_budget = &budget;
            threads.spawn(move || {
                let _small = worker_budget.reserve(2, true, &()).unwrap();
                send.send(()).unwrap();
            });
            wait_for_waiters(&budget, 2);
            assert!(
                receive.try_recv().is_err(),
                "Small work bypassed the queued large input"
            );
            cancel.store(true, Ordering::Release);
            let result = receive.recv_timeout(Duration::from_secs(5));
            drop(held);
            result.unwrap();
            assert!(large.join().unwrap().is_err());
        });
    }

    #[test]
    fn memory_detection_honors_ancestors_and_physical_memory() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("parent/service");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(root.path().join("memory.max"), "1000").unwrap();
        std::fs::write(child.parent().unwrap().join("memory.high"), "600").unwrap();
        std::fs::write(child.join("memory.max"), "max").unwrap();
        assert_eq!(
            limit_from_cgroup(Some(2000), &child, root.path()),
            Some(600)
        );
        assert_eq!(limit_from_cgroup(Some(400), &child, root.path()), Some(400));
        assert_eq!(limit_from_cgroup(None, &child, root.path()), Some(600));
    }

    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn freed_worker_pages_are_returned_to_kernel() {
        const CHILD: &str = "SIGLA_MEMORY_RECLAIM_TEST";
        if std::env::var_os(CHILD).is_none() {
            for mode in ["indexing", "pressure"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "memory::tests::freed_worker_pages_are_returned_to_kernel",
                        "--nocapture",
                    ])
                    .env(CHILD, mode)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let indexing = std::env::var(CHILD).unwrap() == "indexing";

        // isolate allocator settings from other tests and force free arena pages
        // to remain resident until reclamation, as with fragmented worker heaps.
        unsafe {
            assert_eq!(libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024 * 1024), 1);
            assert_eq!(libc::mallopt(libc::M_TRIM_THRESHOLD, -1), 1);
        }
        release_unused_pages();
        let baseline = resident("RssAnon:").unwrap();
        let per_worker = if indexing {
            INDEXING_FREE_HIGH_WATER
        } else {
            IDLE_CACHE_HIGH_WATER as usize
        } / (64 * 1024);
        let pins = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(move || {
                        let buffers: Vec<_> =
                            (0..per_worker).map(|_| vec![1u8; 64 * 1024]).collect();
                        let pin = vec![7u8; 1024 * 1024];
                        std::hint::black_box(&buffers);
                        drop(buffers);
                        pin
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let retained = resident("RssAnon:").unwrap();
        let released = 2 * per_worker as u64 * 64 * 1024;
        assert!(retained > baseline + released / 2);
        // A large untouched live allocation must not hide the freed pages.
        let sparse = unsafe { libc::malloc(released as usize * 2) };
        assert!(!sparse.is_null());
        if indexing {
            assert!(retained < IDLE_CACHE_HIGH_WATER);
            reclaim_after_indexing();
        } else {
            start_reclaimer().unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while resident("RssAnon:").unwrap() + released / 2 >= retained
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let reclaimed = resident("RssAnon:").unwrap();
        unsafe { libc::free(sparse) };
        assert!(reclaimed + released / 2 < retained);
        assert!(pins.iter().all(|pin| pin.iter().all(|&byte| byte == 7)));
    }
}
