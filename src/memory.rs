//! memory admission and reclamation for completed work and idle caches.
use std::io;
use std::sync::{Condvar, Mutex, OnceLock};

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
    available: Mutex<u64>,
    changed: Condvar,
}
impl Budget {
    fn new(capacity: u64) -> Self {
        Self {
            capacity,
            available: Mutex::new(capacity),
            changed: Condvar::new(),
        }
    }
    fn reserve(&self, bytes: u64) -> Reservation<'_> {
        // a single oversized input runs alone rather than waiting forever.
        let bytes = bytes.min(self.capacity);
        let mut available = self.available.lock().unwrap();
        while *available < bytes {
            available = self.changed.wait(available).unwrap();
        }
        *available -= bytes;
        Reservation {
            budget: self,
            bytes,
        }
    }
}
pub struct Reservation<'a> {
    budget: &'a Budget,
    bytes: u64,
}
impl Reservation<'_> {
    /// Release parser working space while retaining the publication estimate.
    /// Never acquire more budget while holding another reservation.
    pub fn shrink_to(&mut self, bytes: u64) {
        let retained = bytes.min(self.bytes);
        let released = self.bytes - retained;
        if released != 0 {
            self.bytes = retained;
            *self.budget.available.lock().unwrap() += released;
            self.budget.changed.notify_all();
        }
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        *self.budget.available.lock().unwrap() += self.bytes;
        self.budget.changed.notify_all();
    }
}

fn admit_parsing(bytes: u64) -> Reservation<'static> {
    static PARSING: OnceLock<Budget> = OnceLock::new();
    let budget = PARSING.get_or_init(|| Budget::new(256 * 1024 * 1024));
    budget.reserve(bytes)
}

pub fn admit_file(bytes: u64, assembly: bool) -> Reservation<'static> {
    // account for syntax nodes, owned facts, compression, and publication buffers.
    // these are admission estimates, not a bound on an arbitrary parser's heap.
    admit_parsing(bytes.saturating_mul(if assembly { 8 } else { 32 }))
}

pub fn admit_batch(
    total: u64,
    largest: u64,
    encoded_limit: u64,
    assembly: bool,
) -> Reservation<'static> {
    let expansion = if assembly { 8 } else { 32 };
    // A worker parses one file at a time, retaining bounded encodings and posting
    // names for publication. Reserving syntax trees for every file serializes
    // large batches even when their actual live memory fits several workers.
    admit_parsing(
        total.saturating_mul(expansion).min(
            largest
                .saturating_mul(expansion)
                .saturating_add(encoded_limit.saturating_mul(8)),
        ),
    )
}

pub fn admit_worker() -> Reservation<'static> {
    static WORKERS: OnceLock<Budget> = OnceLock::new();
    WORKERS
        .get_or_init(|| {
            Budget::new(
                std::thread::available_parallelism()
                    .map_or(1, usize::from)
                    .min(4) as u64,
            )
        })
        .reserve(1)
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
        let budget = Budget::new(10);
        let mut held = budget.reserve(10);
        std::thread::scope(|threads| {
            let (admitted, ready) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel();
            let worker_budget = &budget;
            threads.spawn(move || {
                let _next = worker_budget.reserve(6);
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
        assert_eq!(*budget.available.lock().unwrap(), budget.capacity);
    }

    #[test]
    fn concurrent_reservations_bound_work_and_release_capacity() {
        let budget = Budget::new(12);
        let active = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..32 {
                        let _reservation = budget.reserve(3);
                        let count = active.fetch_add(3, Ordering::SeqCst) + 3;
                        peak.fetch_max(count, Ordering::SeqCst);
                        std::thread::yield_now();
                        active.fetch_sub(3, Ordering::SeqCst);
                    }
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= budget.capacity);
        assert_eq!(*budget.available.lock().unwrap(), budget.capacity);
        let _oversized = budget.reserve(budget.capacity + 1);
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
