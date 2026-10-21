use super::{Event, Export, Message, Record, Recorder, Summary, now_ms};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Weak, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

const MAGIC: &[u8; 8] = b"SGMON001";
const HEADER_BYTES: u64 = 16;
const FRAME_HEADER_BYTES: u64 = 20;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const EXPORT_BYTES: u64 = 4 * 1024 * 1024;
const DAY_MS: u64 = 86_400_000;
const RETAIN_DAYS: u64 = 365;
const MAX_ACTIVE: usize = 1024;
const BATCH_BYTES: usize = MAX_FRAME_BYTES / 2;

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    measured_ms: u64,
    gaps: u64,
    active: BTreeMap<String, Record>,
}

#[derive(Default, Serialize, Deserialize)]
struct Cursor {
    identity: String,
    days: BTreeMap<u64, Position>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Position {
    generation: u64,
    offset: u64,
}

pub(super) struct Writer {
    directory: PathBuf,
    _lock: File,
    run_id: String,
    snapshot: Snapshot,
    active_bytes: usize,
    pending: BTreeMap<u64, Vec<Event>>,
    pending_bytes: usize,
    prepared: BTreeMap<u64, ()>,
    dirty: bool,
}

fn random_id() -> Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&temporary)?;
    file.set_permissions(fs::Permissions::from_mode(0o640))?;
    file.write_all(bytes)?;
    file.sync_data()?;
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn read_snapshot(directory: &Path) -> Result<Snapshot> {
    let path = directory.join("active.bin");
    match File::open(path) {
        Ok(file) => {
            ensure!(
                file.metadata()?.len() <= MAX_FRAME_BYTES as u64,
                "monitor snapshot exceeds size limit"
            );
            let mut decoded = Vec::new();
            zstd::stream::read::Decoder::new(file)?
                .take(MAX_FRAME_BYTES as u64 + 1)
                .read_to_end(&mut decoded)?;
            ensure!(
                decoded.len() <= MAX_FRAME_BYTES,
                "monitor snapshot exceeds decoded size limit"
            );
            Ok(postcard::from_bytes(&decoded)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot::default()),
        Err(error) => Err(error.into()),
    }
}

fn day_path(directory: &Path, day: u64) -> PathBuf {
    directory.join(format!("{day}.journal"))
}

fn days(directory: &Path) -> Result<Vec<u64>> {
    let mut days = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if let Some(day) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".journal"))
            .and_then(|day| day.parse::<u64>().ok())
        {
            days.push(day);
        }
    }
    days.sort_unstable();
    Ok(days)
}

fn header(file: &mut File) -> Result<u64> {
    let mut bytes = [0; HEADER_BYTES as usize];
    file.read_exact(&mut bytes)?;
    ensure!(&bytes[..8] == MAGIC, "unsupported monitor journal format");
    Ok(u64::from_le_bytes(bytes[8..].try_into().unwrap()))
}

fn create_day(path: &Path, generation: u64) -> Result<File> {
    let mut file = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o640)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o640))?;
    file.write_all(MAGIC)?;
    file.write_all(&generation.to_le_bytes())?;
    Ok(file)
}

/// A partially written last frame is invisible until it is complete.
fn read_frame(file: &mut File) -> Result<Option<(Vec<Event>, u64, usize)>> {
    let start = file.stream_position()?;
    let length = file.metadata()?.len();
    if length.saturating_sub(start) < FRAME_HEADER_BYTES {
        return Ok(None);
    }
    let mut prefix = [0; FRAME_HEADER_BYTES as usize];
    file.read_exact(&mut prefix)?;
    let encoded_length = u32::from_le_bytes(prefix[..4].try_into().unwrap()) as usize;
    ensure!(
        encoded_length <= MAX_FRAME_BYTES,
        "monitor frame exceeds size limit"
    );
    if length.saturating_sub(start + FRAME_HEADER_BYTES) < encoded_length as u64 {
        return Ok(None);
    }
    let mut encoded = vec![0; encoded_length];
    file.read_exact(&mut encoded)?;
    if blake3::hash(&encoded).as_bytes()[..16] != prefix[4..] {
        if start + FRAME_HEADER_BYTES + encoded_length as u64 == length {
            return Ok(None);
        }
        anyhow::bail!("monitor frame checksum mismatch before journal tail");
    }
    let mut decoded = Vec::new();
    zstd::stream::read::Decoder::new(encoded.as_slice())?
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut decoded)?;
    ensure!(
        decoded.len() <= MAX_FRAME_BYTES,
        "monitor frame exceeds decoded size limit"
    );
    let events = super::wire::decode(&decoded)?;
    let expanded_bytes = postcard::experimental::serialized_size(&events)?;
    ensure!(
        expanded_bytes <= MAX_FRAME_BYTES,
        "monitor frame exceeds expanded size limit"
    );
    Ok(Some((
        events,
        FRAME_HEADER_BYTES + encoded_length as u64,
        expanded_bytes,
    )))
}

fn write_frame(file: &mut File, events: &[Event]) -> Result<()> {
    let expanded_bytes = postcard::experimental::serialized_size(events)?;
    let decoded = super::wire::encode(events)?;
    if expanded_bytes > MAX_FRAME_BYTES || decoded.len() > MAX_FRAME_BYTES {
        ensure!(events.len() > 1, "monitor event exceeds frame size limit");
        let middle = events.len() / 2;
        write_frame(file, &events[..middle])?;
        return write_frame(file, &events[middle..]);
    }
    let encoded = zstd::stream::encode_all(decoded.as_slice(), 3)?;
    ensure!(
        encoded.len() <= MAX_FRAME_BYTES,
        "compressed monitor frame exceeds size limit"
    );
    file.write_all(&(encoded.len() as u32).to_le_bytes())?;
    file.write_all(&blake3::hash(&encoded).as_bytes()[..16])?;
    file.write_all(&encoded)?;
    Ok(())
}

impl Writer {
    pub(super) fn open(directory: &Path) -> Result<Self> {
        let existed = directory.exists();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(directory)?;
        if !existed {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o750))?;
        }
        let lock_path = directory.join("writer.lock");
        let lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o640)
            .open(&lock_path)
        {
            Ok(file) => {
                file.set_permissions(fs::Permissions::from_mode(0o640))?;
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                OpenOptions::new().read(true).write(true).open(&lock_path)?
            }
            Err(error) => return Err(error.into()),
        };
        // The lock belongs to this file descriptor and is released even after a crash.
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another process owns the monitoring journal"
        );
        let identity = directory.join("identity");
        if !identity.exists() {
            atomic_write(&identity, random_id()?.as_bytes())?;
        }
        let snapshot = match read_snapshot(directory) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "monitor active snapshot was unreadable; completed journal remains available");
                fs::rename(
                    directory.join("active.bin"),
                    directory.join("active.damaged"),
                )?;
                Snapshot {
                    gaps: 1,
                    ..Snapshot::default()
                }
            }
        };
        let mut writer = Self {
            directory: directory.to_owned(),
            _lock: lock,
            run_id: random_id()?,
            snapshot,
            active_bytes: 0,
            pending: BTreeMap::new(),
            pending_bytes: 0,
            prepared: BTreeMap::new(),
            dirty: true,
        };
        for day in days(directory)? {
            let path = day_path(directory, day);
            if fs::metadata(&path)?.len() < HEADER_BYTES {
                let mut bytes = MAGIC.to_vec();
                bytes.extend_from_slice(&4u64.to_le_bytes());
                atomic_write(&path, &bytes)?;
                writer.snapshot.gaps = writer.snapshot.gaps.saturating_add(1);
            }
        }
        // A prior process cannot still be running after exclusive admission succeeds.
        let mut active = std::mem::take(&mut writer.snapshot.active);
        let active_days: std::collections::BTreeSet<u64> = active
            .values()
            .map(|record| record.start_ms / DAY_MS)
            .collect();
        for day in active_days {
            let path = day_path(directory, day);
            if !path.exists() {
                continue;
            }
            let mut file = File::open(path)?;
            header(&mut file)?;
            while let Some((events, _, _)) = read_frame(&mut file)? {
                for event in events {
                    if let Event::Operation(record) = event {
                        active.remove(&record.id);
                    }
                }
            }
        }
        for (_, mut record) in active {
            record.end_ms = Some(writer.snapshot.measured_ms.max(record.start_ms));
            record.status = "unfinished".into();
            record.detailed = true;
            record.current_stage = None;
            record.waiting.clear();
            writer
                .pending
                .entry(record.start_ms / DAY_MS)
                .or_default()
                .push(Event::Operation(record));
        }
        writer.flush()?;
        Ok(writer)
    }

    pub(super) fn run_id(&self) -> &str {
        &self.run_id
    }
    pub(super) fn gaps(&self) -> u64 {
        self.snapshot.gaps
    }

    fn accept(&mut self, mut event: Event) {
        if !matches!(&event, Event::Operation(record) if record.end_ms.is_none()) {
            self.pending_bytes = self.pending_bytes.saturating_add(
                postcard::experimental::serialized_size(&event).unwrap_or(MAX_FRAME_BYTES),
            );
        }
        match &mut event {
            Event::Operation(record) if record.end_ms.is_none() => {
                if let Some(previous) = self.snapshot.active.get(&record.id) {
                    record.query = previous.query.clone();
                }
                let bytes =
                    postcard::experimental::serialized_size(record).unwrap_or(MAX_FRAME_BYTES);
                let previous = self
                    .snapshot
                    .active
                    .get(&record.id)
                    .and_then(|record| postcard::experimental::serialized_size(record).ok())
                    .unwrap_or(0);
                let mut total = self
                    .active_bytes
                    .saturating_sub(previous)
                    .saturating_add(bytes);
                while (previous == 0 && self.snapshot.active.len() >= MAX_ACTIVE)
                    || total > MAX_FRAME_BYTES / 2
                {
                    let uncertain = self
                        .snapshot
                        .active
                        .iter()
                        .find(|(id, candidate)| {
                            candidate.status == "unknown" && id.as_str() != record.id
                        })
                        .map(|(id, _)| id.clone());
                    let Some(id) = uncertain else {
                        break;
                    };
                    let removed = self.snapshot.active.remove(&id).unwrap();
                    let removed_bytes =
                        postcard::experimental::serialized_size(&removed).unwrap_or(0);
                    self.active_bytes = self.active_bytes.saturating_sub(removed_bytes);
                    total = self
                        .active_bytes
                        .saturating_sub(previous)
                        .saturating_add(bytes);
                }
                if (previous == 0 && self.snapshot.active.len() >= MAX_ACTIVE)
                    || total > MAX_FRAME_BYTES / 2
                {
                    self.snapshot.gaps += 1;
                } else {
                    self.active_bytes = total;
                    self.snapshot
                        .active
                        .insert(record.id.clone(), record.clone());
                }
            }
            Event::Operation(record) => {
                if let Some(previous) = self.snapshot.active.remove(&record.id) {
                    self.active_bytes = self.active_bytes.saturating_sub(
                        postcard::experimental::serialized_size(&previous).unwrap_or(0),
                    );
                }
                let day = record.start_ms / DAY_MS;
                if record.status == "noop" && !record.detailed {
                    let pending = self.pending.entry(day).or_default();
                    let duration = record
                        .end_ms
                        .unwrap_or(record.start_ms)
                        .saturating_sub(record.start_ms);
                    if let Some(Event::Summary(summary)) = pending.iter_mut().find(|event| matches!(event,
                        Event::Summary(summary) if summary.kind == record.kind && summary.repository == record.repository)) {
                        summary.count += 1;
                        summary.total_duration_ms = summary.total_duration_ms.saturating_add(duration);
                    } else {
                        pending.push(Event::Summary(Summary { day, kind: record.kind.clone(), repository: record.repository.clone(), count: 1, total_duration_ms: duration }));
                    }
                } else {
                    self.pending.entry(day).or_default().push(event);
                }
            }
            Event::Disk(sample) => self
                .pending
                .entry(sample.timestamp_ms / DAY_MS)
                .or_default()
                .push(event),
            Event::Summary(summary) => self.pending.entry(summary.day).or_default().push(event),
        }
        self.dirty = true;
    }

    fn append(&mut self, day: u64, events: &[Event]) -> Result<()> {
        let path = day_path(&self.directory, day);
        let mut file = if path.exists() {
            OpenOptions::new().read(true).write(true).open(&path)?
        } else {
            create_day(&path, 0)?
        };
        if let std::collections::btree_map::Entry::Vacant(entry) = self.prepared.entry(day) {
            file.seek(SeekFrom::Start(0))?;
            let generation = header(&mut file)?;
            let mut complete = HEADER_BYTES;
            while let Some((_, bytes, _)) = read_frame(&mut file)? {
                complete += bytes;
            }
            // Late completions must make a packed day eligible for retention again.
            let mut next_generation = if generation & 3 != 0 {
                (generation & !3).saturating_add(4)
            } else {
                generation
            };
            if file.metadata()?.len() > complete {
                self.snapshot.gaps = self.snapshot.gaps.saturating_add(1);
                next_generation = next_generation.saturating_add(4);
            }
            if next_generation != generation {
                file.seek(SeekFrom::Start(8))?;
                file.write_all(&next_generation.to_le_bytes())?;
            }
            file.set_len(complete)?;
            entry.insert(());
        }
        file.seek(SeekFrom::End(0))?;
        let start = file.stream_position()?;
        if let Err(error) =
            write_frame(&mut file, events).and_then(|()| file.sync_data().map_err(Into::into))
        {
            // Do not strand a partial frame before the next accepted batch.
            let _ = file.set_len(start);
            return Err(error);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        for (day, events) in pending {
            if let Err(error) = self.append(day, &events) {
                self.snapshot.gaps = self.snapshot.gaps.saturating_add(events.len() as u64);
                tracing::warn!(%error, "monitor journal append failed");
            }
        }
        if self.dirty {
            self.snapshot.measured_ms = now_ms();
            let encoded =
                zstd::stream::encode_all(postcard::to_allocvec(&self.snapshot)?.as_slice(), 3)?;
            atomic_write(&self.directory.join("active.bin"), &encoded)?;
            self.dirty = false;
        }
        Ok(())
    }

    pub(super) fn run(mut self, receiver: mpsc::Receiver<Message>, recorder: Weak<Recorder>) {
        let mut last_flush = Instant::now();
        let mut last_retention = Instant::now() - Duration::from_secs(60);
        let mut shared_gaps = self.snapshot.gaps;
        loop {
            let stop = match receiver.recv_timeout(Duration::from_millis(250)) {
                Ok(Message::Event(event, bytes)) => {
                    if let Some(recorder) = recorder.upgrade() {
                        recorder.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
                    }
                    self.accept(event);
                    false
                }
                Ok(Message::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => true,
                Err(mpsc::RecvTimeoutError::Timeout) => false,
            };
            if let Some(recorder) = recorder.upgrade() {
                let gaps = recorder.gaps.load(Ordering::Relaxed);
                if gaps > shared_gaps || self.snapshot.gaps > shared_gaps {
                    for active in self.snapshot.active.values_mut() {
                        active.status = "unknown".into();
                    }
                    self.dirty = true;
                }
                if gaps > shared_gaps {
                    self.snapshot.gaps = self.snapshot.gaps.saturating_add(gaps - shared_gaps);
                    self.dirty = true;
                }
                let local = self.snapshot.gaps.saturating_sub(gaps);
                shared_gaps = recorder
                    .gaps
                    .fetch_add(local, Ordering::Relaxed)
                    .saturating_add(local);
                self.snapshot.gaps = shared_gaps;
            }
            let pending_count: usize = self.pending.values().map(Vec::len).sum();
            if stop
                || last_flush.elapsed() >= Duration::from_secs(1)
                || pending_count >= 256
                || self.pending_bytes >= BATCH_BYTES
            {
                if let Err(error) = self.flush() {
                    self.snapshot.gaps = self.snapshot.gaps.saturating_add(1);
                    tracing::warn!(%error, "monitor snapshot write failed");
                }
                last_flush = Instant::now();
            }
            if stop {
                break;
            }
            if last_retention.elapsed() >= Duration::from_secs(60) {
                let mut stopped = false;
                let result = self.retain_one(now_ms() / DAY_MS, |writer| {
                    // Repacking a large day must keep admitting current work.
                    for _ in 0..256 {
                        match receiver.try_recv() {
                            Ok(Message::Event(event, bytes)) => {
                                if let Some(recorder) = recorder.upgrade() {
                                    recorder.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
                                }
                                writer.accept(event);
                            }
                            Ok(Message::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                                stopped = true;
                                break;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                        }
                        if writer.pending_bytes >= BATCH_BYTES {
                            break;
                        }
                    }
                    if last_flush.elapsed() >= Duration::from_secs(1)
                        || writer.pending.values().map(Vec::len).sum::<usize>() >= 256
                        || writer.pending_bytes >= BATCH_BYTES
                    {
                        writer.flush()?;
                        last_flush = Instant::now();
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    tracing::warn!(%error, "monitor retention failed");
                }
                if stopped {
                    let _ = self.flush();
                    break;
                }
                last_retention = Instant::now();
            }
        }
    }

    fn retain_one(
        &mut self,
        today: u64,
        mut yield_work: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<()> {
        for day in days(&self.directory)? {
            if day >= today {
                break;
            }
            // An operation can span midnight or be resumed from a crashed process.
            if self
                .snapshot
                .active
                .values()
                .any(|record| record.start_ms / DAY_MS == day)
            {
                continue;
            }
            let path = day_path(&self.directory, day);
            let mut file = File::open(&path)?;
            let generation = header(&mut file)?;
            let rollup = day.saturating_add(RETAIN_DAYS) < today;
            if generation & 3 >= if rollup { 2 } else { 1 } {
                continue;
            }
            let discarded_tail = compact(
                &path,
                day,
                file,
                (generation & !3) | if rollup { 2 } else { 1 },
                rollup,
                || yield_work(self),
            )?;
            if discarded_tail {
                self.snapshot.gaps = self.snapshot.gaps.saturating_add(1);
                self.dirty = true;
            }
            self.prepared.remove(&day);
            break;
        }
        Ok(())
    }
}

fn compact(
    path: &Path,
    day: u64,
    mut input: File,
    generation: u64,
    rollup: bool,
    mut yield_work: impl FnMut() -> Result<()>,
) -> Result<bool> {
    let temporary = path.with_extension("compact");
    // An interrupted previous replacement was never published.
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let mut output = create_day(&temporary, generation)?;
    let mut summaries: BTreeMap<(String, Option<String>), Summary> = BTreeMap::new();
    let mut kept = Vec::new();
    let mut kept_bytes = 0;
    let discarded_tail;
    loop {
        yield_work()?;
        let start = input.stream_position()?;
        let Some((events, _, _)) = read_frame(&mut input)? else {
            discarded_tail = start < input.metadata()?.len();
            break;
        };
        for event in events {
            match event {
                Event::Operation(record)
                    if rollup && !record.detailed && record.status == "success" =>
                {
                    let summary = summaries
                        .entry((record.kind.clone(), record.repository.clone()))
                        .or_insert_with(|| Summary {
                            day,
                            kind: record.kind.clone(),
                            repository: record.repository.clone(),
                            count: 0,
                            total_duration_ms: 0,
                        });
                    summary.count += 1;
                    summary.total_duration_ms = summary.total_duration_ms.saturating_add(
                        record
                            .end_ms
                            .unwrap_or(record.start_ms)
                            .saturating_sub(record.start_ms),
                    );
                }
                Event::Summary(summary) => {
                    let accumulated = summaries
                        .entry((summary.kind.clone(), summary.repository.clone()))
                        .or_insert_with(|| Summary {
                            day,
                            kind: summary.kind.clone(),
                            repository: summary.repository.clone(),
                            count: 0,
                            total_duration_ms: 0,
                        });
                    accumulated.count = accumulated.count.saturating_add(summary.count);
                    accumulated.total_duration_ms = accumulated
                        .total_duration_ms
                        .saturating_add(summary.total_duration_ms);
                }
                event => {
                    kept_bytes += postcard::experimental::serialized_size(&event)?;
                    kept.push(event);
                }
            }
            if kept.len() >= 256 || kept_bytes >= BATCH_BYTES {
                write_frame(&mut output, &kept)?;
                kept.clear();
                kept_bytes = 0;
            }
        }
    }
    if !kept.is_empty() {
        write_frame(&mut output, &kept)?;
    }
    let summaries: Vec<Event> = summaries.into_values().map(Event::Summary).collect();
    if !summaries.is_empty() {
        write_frame(&mut output, &summaries)?;
    }
    output.sync_all()?;
    fs::rename(temporary, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(discarded_tail)
}

/// Reads only new compressed frames. It never opens the cache or contacts the service.
pub fn export(directory: &Path, after: Option<&str>) -> Result<Export> {
    let identity = fs::read_to_string(directory.join("identity"))
        .context("monitor journal is not initialized")?;
    let mut cursor: Cursor = match after {
        Some(value) => {
            ensure!(
                value.len() <= 1024 * 1024,
                "monitor cursor exceeds size limit"
            );
            serde_json::from_str(value).context("invalid monitor cursor")?
        }
        None => Cursor::default(),
    };
    let reset = !cursor.identity.is_empty() && cursor.identity != identity;
    if cursor.identity != identity {
        cursor.days.clear();
        cursor.identity = identity;
    }
    let snapshot = read_snapshot(directory)?;
    let mut result = Export {
        version: 1,
        cursor: String::new(),
        more: false,
        reset,
        replace_days: Vec::new(),
        operations: Vec::new(),
        active: snapshot.active.into_values().collect(),
        disk: Vec::new(),
        summaries: Vec::new(),
        gaps: snapshot.gaps,
    };
    let mut consumed = 0;
    let mut decoded_bytes = 0;
    for day in days(directory)? {
        let mut file = File::open(day_path(directory, day))?;
        let generation = header(&mut file)?;
        let old = cursor.days.get(&day).copied();
        let position = match old {
            Some(position) if position.generation == generation => position,
            Some(_) => {
                result.replace_days.push(day);
                Position {
                    generation,
                    offset: HEADER_BYTES,
                }
            }
            None => Position {
                generation,
                offset: HEADER_BYTES,
            },
        };
        let size = file.metadata()?.len();
        ensure!(
            position.offset >= HEADER_BYTES && position.offset <= size,
            "monitor cursor points outside journal"
        );
        file.seek(SeekFrom::Start(position.offset))?;
        let mut offset = position.offset;
        while let Some((events, bytes, decoded)) = read_frame(&mut file)? {
            for event in events {
                match event {
                    Event::Operation(record) => result.operations.push(record),
                    Event::Disk(sample) => result.disk.push(sample),
                    Event::Summary(summary) => result.summaries.push(summary),
                }
            }
            offset += bytes;
            consumed += bytes;
            decoded_bytes += decoded;
            if consumed >= EXPORT_BYTES || decoded_bytes >= MAX_FRAME_BYTES {
                result.more = true;
                break;
            }
        }
        cursor.days.insert(day, Position { generation, offset });
        if result.more {
            break;
        }
    }
    // Without the writer lock, remaining active entries are last observations,
    // not evidence that a process is still working.
    if let Ok(lock) = File::open(directory.join("writer.lock"))
        && unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0
    {
        for active in &mut result.active {
            active.status = "unfinished".into();
            active.end_ms = Some(snapshot.measured_ms.max(active.start_ms));
        }
    }
    result.cursor = serde_json::to_string(&cursor)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, day: u64, status: &str, detailed: bool) -> Record {
        Record {
            id: id.into(),
            kind: "search".into(),
            repository: Some("repo".into()),
            target: None,
            query: Some("symbol:Example".into()),
            start_ms: day * DAY_MS + 100,
            end_ms: Some(day * DAY_MS + 200),
            status: status.into(),
            detailed,
            stages: Vec::new(),
            links: Vec::new(),
            current_stage: None,
            waiting: Vec::new(),
        }
    }

    #[test]
    fn incremental_export_reads_new_frames_and_replaces_compacted_days() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("fast", 1, "success", false)));
        writer.accept(Event::Operation(record("failed", 1, "failure", true)));
        writer.flush().unwrap();
        let first = export(directory.path(), None).unwrap();
        assert_eq!(first.operations.len(), 2);
        assert!(
            export(directory.path(), Some(&first.cursor))
                .unwrap()
                .operations
                .is_empty()
        );
        writer.accept(Event::Operation(record("new", 2, "success", false)));
        writer.flush().unwrap();
        let next = export(directory.path(), Some(&first.cursor)).unwrap();
        assert_eq!(next.operations.len(), 1);
        assert_eq!(next.operations[0].id, "new");
        writer.retain_one(RETAIN_DAYS + 2, |_| Ok(())).unwrap();
        let replacement = export(directory.path(), Some(&next.cursor)).unwrap();
        assert_eq!(replacement.replace_days, vec![1]);
        assert_eq!(replacement.operations.len(), 1);
        assert_eq!(replacement.operations[0].id, "failed");
        assert_eq!(
            replacement
                .summaries
                .iter()
                .map(|summary| summary.count)
                .sum::<u64>(),
            1
        );
        assert!(
            export(directory.path(), Some(&replacement.cursor))
                .unwrap()
                .replace_days
                .is_empty()
        );
    }

    #[test]
    fn late_completions_are_included_in_retention() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("first", 1, "success", false)));
        writer.flush().unwrap();
        writer.retain_one(RETAIN_DAYS + 2, |_| Ok(())).unwrap();
        let initial = export(directory.path(), None).unwrap();
        writer.accept(Event::Operation(record("late", 1, "success", false)));
        writer.flush().unwrap();
        writer.retain_one(RETAIN_DAYS + 2, |_| Ok(())).unwrap();
        let result = export(directory.path(), Some(&initial.cursor)).unwrap();
        assert_eq!(result.replace_days, vec![1]);
        assert!(result.operations.is_empty());
        assert_eq!(result.summaries.iter().map(|s| s.count).sum::<u64>(), 2);
    }

    #[test]
    fn crash_between_frame_and_snapshot_does_not_replace_completed_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        let mut active = record("completed", 1, "running", false);
        active.end_ms = None;
        writer.accept(Event::Operation(active));
        writer.flush().unwrap();
        writer
            .append(
                1,
                &[Event::Operation(record("completed", 1, "success", false))],
            )
            .unwrap();
        drop(writer); // Simulate termination before the active snapshot is replaced.
        let _writer = Writer::open(directory.path()).unwrap();
        let result = export(directory.path(), None).unwrap();
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].status, "success");
        assert!(result.active.is_empty());
    }

    #[test]
    fn interrupted_work_is_recovered_once_with_its_last_observation() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        let mut active = record("interrupted", 1, "running", false);
        active.end_ms = None;
        writer.accept(Event::Operation(active));
        writer.flush().unwrap();
        drop(writer);
        let stale = export(directory.path(), None).unwrap();
        assert_eq!(stale.active[0].status, "unfinished");
        let writer = Writer::open(directory.path()).unwrap();
        let recovered = export(directory.path(), None).unwrap();
        assert!(recovered.active.is_empty());
        assert_eq!(recovered.operations[0].status, "unfinished");
        assert!(recovered.operations[0].detailed);
        drop(writer);
        let _writer = Writer::open(directory.path()).unwrap();
        assert_eq!(export(directory.path(), None).unwrap().operations.len(), 1);
    }

    #[test]
    fn incomplete_tail_is_hidden_then_repaired_before_appending() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("first", 1, "success", false)));
        writer.flush().unwrap();
        let first = export(directory.path(), None).unwrap();
        drop(writer);
        let mut file = OpenOptions::new()
            .append(true)
            .open(day_path(directory.path(), 1))
            .unwrap();
        file.write_all(&[255, 1, 0, 0, 1, 2, 3]).unwrap();
        assert!(
            export(directory.path(), Some(&first.cursor))
                .unwrap()
                .operations
                .is_empty()
        );
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("second", 1, "success", false)));
        writer.flush().unwrap();
        let result = export(directory.path(), Some(&first.cursor)).unwrap();
        assert_eq!(result.replace_days, vec![1]);
        assert_eq!(result.operations.len(), 2);
        assert_eq!(result.operations[1].id, "second");
        assert!(result.gaps > 0);
    }

    #[test]
    fn corrupted_tail_does_not_prevent_later_records() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("first", 1, "success", false)));
        writer.flush().unwrap();
        writer.accept(Event::Operation(record("torn", 1, "success", false)));
        writer.flush().unwrap();
        drop(writer);
        let path = day_path(directory.path(), 1);
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&[255]).unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("later", 1, "success", false)));
        writer.flush().unwrap();
        let result = export(directory.path(), None).unwrap();
        assert_eq!(
            result
                .operations
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "later"]
        );
        assert!(result.gaps > 0);
    }

    #[test]
    fn repacking_reports_a_discarded_tail_without_new_appends() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("saved", 1, "success", false)));
        writer.flush().unwrap();
        OpenOptions::new()
            .append(true)
            .open(day_path(directory.path(), 1))
            .unwrap()
            .write_all(&[1, 2, 3])
            .unwrap();
        writer.retain_one(2, |_| Ok(())).unwrap();
        writer.flush().unwrap();
        let result = export(directory.path(), None).unwrap();
        assert_eq!(result.operations.len(), 1);
        assert!(result.gaps > 0);
    }

    #[test]
    fn noop_checks_are_counts_and_active_queries_survive_preview_updates() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("noop1", 1, "noop", false)));
        writer.accept(Event::Operation(record("noop2", 1, "noop", false)));
        let mut active = record("active", 1, "running", false);
        active.end_ms = None;
        active.query = Some("complete query".into());
        writer.accept(Event::Operation(active.clone()));
        active.query = Some("preview".into());
        writer.accept(Event::Operation(active));
        writer.flush().unwrap();
        let result = export(directory.path(), None).unwrap();
        assert!(result.operations.is_empty());
        assert_eq!(result.summaries[0].count, 2);
        assert_eq!(result.active[0].query.as_deref(), Some("complete query"));
    }

    #[test]
    fn another_journal_resets_a_cursor_and_writer_admission_is_exclusive() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let _first = Writer::open(first.path()).unwrap();
        let _second = Writer::open(second.path()).unwrap();
        assert!(Writer::open(first.path()).is_err());
        let cursor = export(first.path(), None).unwrap().cursor;
        assert!(export(second.path(), Some(&cursor)).unwrap().reset);
    }

    #[test]
    fn repeated_large_queries_split_by_expanded_size_and_export_in_pages() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        let query = "x".repeat(64 * 1024);
        for index in 0..256 {
            let mut record = record(&format!("large-{index}"), 1, "failure", true);
            record.query = Some(query.clone());
            writer.accept(Event::Operation(record));
        }
        writer.flush().unwrap();
        let first = export(directory.path(), None).unwrap();
        assert!(first.more);
        assert!(first.operations.len() < 256);
        let mut count = first.operations.len();
        let mut cursor = first.cursor;
        loop {
            let page = export(directory.path(), Some(&cursor)).unwrap();
            count += page.operations.len();
            cursor = page.cursor;
            if !page.more {
                break;
            }
        }
        assert_eq!(count, 256);
    }

    #[test]
    fn repacking_large_records_keeps_batches_within_the_memory_budget() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        let query = "x".repeat(super::super::MAX_QUERY_BYTES);
        let mut largest = 0;
        for index in 0..180 {
            let mut operation = record(&format!("large-{index}"), 1, "failure", true);
            operation.query = Some(query.clone());
            let event = Event::Operation(operation);
            largest = largest.max(postcard::experimental::serialized_size(&event).unwrap());
            writer.accept(event);
            writer.flush().unwrap();
        }
        writer.retain_one(2, |_| Ok(())).unwrap();
        let mut file = File::open(day_path(directory.path(), 1)).unwrap();
        header(&mut file).unwrap();
        let mut count = 0;
        while let Some((events, _, expanded)) = read_frame(&mut file).unwrap() {
            count += events.len();
            // A batch may cross its target by one record and a collection prefix.
            assert!(expanded <= BATCH_BYTES + largest + 10);
        }
        assert_eq!(count, 180);
    }

    #[test]
    fn storage_failure_is_counted_and_later_batches_can_recover() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        fs::create_dir(day_path(directory.path(), 1)).unwrap();
        writer.accept(Event::Operation(record("lost", 1, "success", false)));
        writer.flush().unwrap();
        fs::remove_dir(day_path(directory.path(), 1)).unwrap();
        writer.accept(Event::Operation(record("saved", 1, "success", false)));
        writer.flush().unwrap();
        let result = export(directory.path(), None).unwrap();
        assert_eq!(result.operations.len(), 1);
        assert!(result.gaps > 0);
    }

    #[test]
    fn a_damaged_active_snapshot_preserves_completed_history() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        writer.accept(Event::Operation(record("saved", 1, "success", false)));
        writer.flush().unwrap();
        drop(writer);
        fs::write(directory.path().join("active.bin"), b"damaged").unwrap();
        let _writer = Writer::open(directory.path()).unwrap();
        let result = export(directory.path(), None).unwrap();
        assert_eq!(result.operations.len(), 1);
        assert!(result.gaps > 0);
        assert!(directory.path().join("active.damaged").exists());
    }

    #[test]
    fn lossless_day_packing_keeps_records_and_drains_current_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        for index in 0..300 {
            writer.accept(Event::Operation(record(
                &format!("run-{index}"),
                1,
                "success",
                false,
            )));
            writer.flush().unwrap();
        }
        let before = export(directory.path(), None).unwrap();
        let bytes = fs::metadata(day_path(directory.path(), 1)).unwrap().len();
        let mut yields = 0;
        writer
            .retain_one(2, |writer| {
                yields += 1;
                if yields == 1 {
                    writer.accept(Event::Operation(record("current", 2, "success", false)));
                    writer.flush()?;
                }
                Ok(())
            })
            .unwrap();
        let after = export(directory.path(), Some(&before.cursor)).unwrap();
        assert_eq!(after.replace_days, vec![1]);
        assert_eq!(after.operations.len(), 301);
        assert!(after.summaries.is_empty());
        assert!(yields >= 300);
        assert!(fs::metadata(day_path(directory.path(), 1)).unwrap().len() < bytes);
    }

    #[test]
    fn a_torn_new_file_header_recovers_without_blocking_export() {
        let directory = tempfile::tempdir().unwrap();
        let writer = Writer::open(directory.path()).unwrap();
        drop(writer);
        fs::write(day_path(directory.path(), 1), &MAGIC[..4]).unwrap();
        let _writer = Writer::open(directory.path()).unwrap();
        let result = export(directory.path(), None).unwrap();
        assert!(result.gaps > 0);
    }

    #[test]
    #[ignore = "measures sparse traffic with one completed request per compressed frame"]
    fn sparse_storage_probe() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(directory.path()).unwrap();
        let before = Instant::now();
        for index in 0..1000 {
            let mut record = record(
                &format!("0123456789abcdef0123456789abcdef-{index}"),
                1,
                "success",
                false,
            );
            record.repository = Some("https://github.com/example/project".into());
            record.start_ms += index * 8640;
            record.end_ms = Some(record.start_ms + 10 + index % 90);
            record.query = Some(format!(
                "calls:Namespace.Type{}.Method{} path:src/**",
                index % 100,
                index % 23
            ));
            writer.accept(Event::Operation(record));
            writer.flush().unwrap();
        }
        let bytes = fs::metadata(day_path(directory.path(), 1)).unwrap().len();
        let flush_ms = before.elapsed().as_millis();
        let pack_started = Instant::now();
        writer.retain_one(2, |_| Ok(())).unwrap();
        let packed = fs::metadata(day_path(directory.path(), 1)).unwrap().len();
        eprintln!(
            "sparse bytes/op={:.1} packed bytes/op={:.1} projected_packed_MiB/year_at_10k_daily={:.1} flush_ms={} pack_ms={}",
            bytes as f64 / 1000.0,
            packed as f64 / 1000.0,
            packed as f64 / 1000.0 * 3_650_000.0 / 1_048_576.0,
            flush_ms,
            pack_started.elapsed().as_millis()
        );
    }
}
