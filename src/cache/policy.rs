//! Pure retention decisions and filesystem accounting; no deletion lives here.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

const HALF_LIFE_MS: f64 = 86_400_000.0;
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub last_use: u64,
    heat: f64,
    at: u64,
}
impl Usage {
    pub fn new(at: u64) -> Self {
        Self {
            last_use: at,
            heat: 1.0,
            at,
        }
    }
    pub fn record(&mut self, now: u64) {
        if now.saturating_sub(self.at) >= 60_000 {
            self.heat = self.frequency(now) + 1.0;
            self.at = now;
        }
        self.last_use = self.last_use.max(now);
    }
    fn frequency(&self, now: u64) -> f64 {
        self.heat * (-(now.saturating_sub(self.at) as f64) / HALF_LIFE_MS).exp2()
    }
    pub fn priority(&self, now: u64, bytes: u64) -> f64 {
        self.frequency(now) / bytes.max(1024 * 1024) as f64
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_bytes: u64,
    pub headroom_percent: u8,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 16_000_000_000,
            headroom_percent: 20,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Disk {
    pub capacity: u64,
    pub available: u64,
}
impl Disk {
    pub fn read(path: &Path) -> Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        ensure!(
            unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
            "Cannot measure cache filesystem: {}",
            std::io::Error::last_os_error()
        );
        let stat = unsafe { stat.assume_init() };
        Ok(Self {
            capacity: stat.f_blocks * stat.f_frsize,
            available: stat.f_bavail * stat.f_frsize,
        })
    }
}
impl Limits {
    pub fn pressured(self, allocated: u64, disk: Disk) -> bool {
        allocated > self.max_bytes
            || disk.available < disk.capacity / 100 * u64::from(self.headroom_percent)
    }
    pub fn reclaim(self, allocated: u64, disk: Disk) -> u64 {
        if !self.pressured(allocated, disk) {
            return 0;
        }
        let size = allocated.saturating_sub(self.max_bytes / 10 * 9);
        let free = if self.headroom_percent == 0 {
            0
        } else {
            (disk.capacity / 100 * u64::from(self.headroom_percent.saturating_add(2).min(100)))
                .saturating_sub(disk.available)
        };
        size.max(free)
    }
}

/// Share each inode's cost equally between its owning views, not its hardlink count.
pub fn attributed(owners: &[(PathBuf, Vec<PathBuf>)]) -> Result<BTreeMap<PathBuf, u64>> {
    fn scan(
        path: &Path,
        owner: usize,
        files: &mut BTreeMap<(u64, u64), (u64, BTreeSet<usize>)>,
    ) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        files
            .entry((metadata.dev(), metadata.ino()))
            .or_insert_with(|| (metadata.blocks() * 512, BTreeSet::new()))
            .1
            .insert(owner);
        if metadata.is_dir() {
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        && path.ends_with("work") =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                scan(&entry?.path(), owner, files)?;
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    let mut result: BTreeMap<_, _> = owners.iter().map(|(owner, _)| (owner.clone(), 0)).collect();
    for (owner, (_, roots)) in owners.iter().enumerate() {
        for root in roots {
            scan(root, owner, &mut files)?;
        }
    }
    for (bytes, indices) in files.values() {
        for owner in indices {
            *result.get_mut(&owners[*owner].0).unwrap() += bytes / indices.len() as u64;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frequency_ages_and_bursts_do_not_inflate_it() {
        let mut frequent = Usage::new(0);
        let mut burst = Usage::new(0);
        for at in 1..60_000 {
            burst.record(at);
        }
        assert_eq!(burst.frequency(60_000), Usage::new(0).frequency(60_000));
        frequent.record(60_000);
        assert!(frequent.priority(60_000, 1) > burst.priority(60_000, 1));
        assert!(
            frequent.priority(10 * HALF_LIFE_MS as u64, 1)
                < Usage::new(10 * HALF_LIFE_MS as u64).priority(10 * HALF_LIFE_MS as u64, 1)
        );
        assert!(frequent.priority(60_000, 1) > frequent.priority(60_000, 100_000_000));
    }
    #[test]
    fn either_limit_triggers_reclamation_with_hysteresis() {
        let limits = Limits {
            max_bytes: 1000,
            headroom_percent: 20,
        };
        let ample = Disk {
            capacity: 10_000,
            available: 5000,
        };
        assert!(!limits.pressured(500, ample));
        assert!(limits.reclaim(1500, ample) > 500);
        let low = Disk {
            available: 1000,
            ..ample
        };
        assert!(limits.reclaim(500, low) > 1000);
        let no_reserve = Limits {
            headroom_percent: 0,
            ..limits
        };
        assert_eq!(
            no_reserve.reclaim(1500, low),
            no_reserve.reclaim(1500, ample)
        );
    }

    #[test]
    fn owners_share_costs_even_when_they_use_the_same_view() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, vec![42; 8192]).unwrap();
        fs::hard_link(&first, &second).unwrap();
        let costs = attributed(&[
            (PathBuf::from("a"), vec![first.clone(), second]),
            (PathBuf::from("b"), vec![first.clone()]),
        ])
        .unwrap();
        assert_eq!(costs[Path::new("a")], costs[Path::new("b")]);
        assert_eq!(
            costs.values().sum::<u64>(),
            fs::metadata(first).unwrap().blocks() * 512
        );
    }
}
