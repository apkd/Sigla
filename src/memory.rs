//! linux resident-memory accounting for eviction of idle, rebuildable caches.
use std::io;
use std::sync::{Condvar, Mutex, OnceLock};

// leave room for two active jobs and temporary parser allocations.
pub const IDLE_CACHE_HIGH_WATER: u64 = 640 * 1024 * 1024;

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
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        *self.budget.available.lock().unwrap() += self.bytes;
        self.budget.changed.notify_all();
    }
}

pub fn admit_file(bytes: u64, assembly: bool) -> Reservation<'static> {
    static PARSING: OnceLock<Budget> = OnceLock::new();
    let budget = PARSING.get_or_init(|| Budget::new(256 * 1024 * 1024));
    // account for syntax nodes, owned facts, compression, and publication buffers.
    // these are admission estimates, not a bound on an arbitrary parser's heap.
    budget.reserve(bytes.saturating_mul(if assembly { 8 } else { 32 }))
}

pub fn resident_bytes() -> io::Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| io::Error::other("Missing resident memory accounting"))?;
    Ok(kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
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
}
