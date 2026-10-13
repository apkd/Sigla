//! Storage-only replay: bounded prepared payloads, contended cold publication,
//! reused views, manifest publication, and cold/warm broad-query latency.
//! The record builder is synthetic; use workspace_validation for real archives.
use super::*;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

fn setting(name: &str, default: usize) -> Result<usize> {
    std::env::var(name).map_or(Ok(default), |value| Ok(value.parse()?))
}
fn identity(index: usize) -> ObjectId {
    *blake3::hash(&(index as u64).to_be_bytes()).as_bytes()
}
fn file(index: usize) -> String {
    format!("file-{index:08}")
}
fn encoded(index: usize, bytes: usize) -> Result<Encoded> {
    let mut body = vec![0; bytes];
    let mut state = index as u64 + 1;
    for chunk in body.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    let mut records = BTreeMap::from([(vec![1], body)]);
    let names = Names {
        declarations: (0..16)
            .map(|n| format!("Type{:05}", (index * 17 + n) % 2048))
            .collect(),
        occurrences: (0..16)
            .map(|n| format!("Use{:05}", (index * 19 + n) % 2048))
            .collect(),
        global_imports: index.is_multiple_of(127),
    };
    let names = crate::store::format::write(&[], None, true, &names, &mut records)?;
    Ok(Encoded { names, records })
}
fn publish(space: &Scope, files: usize, bytes: usize, reuse: bool) -> Result<()> {
    let batch = (8 * 1024 * 1024 / bytes.max(1)).clamp(1, 1024);
    let next = AtomicUsize::new(0);
    std::thread::scope(|threads| -> Result<()> {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                threads.spawn(|| -> Result<()> {
                    loop {
                        let first = next.fetch_add(batch, Ordering::Relaxed);
                        if first >= files {
                            break;
                        }
                        let pending: Vec<_> = (first..files.min(first + batch))
                            .map(|index| {
                                space.prepare_install(
                                    &file(index),
                                    vec![1],
                                    identity(index),
                                    100,
                                    || {
                                        ensure!(!reuse, "Replay rebuilt a cached object");
                                        encoded(index, bytes)
                                    },
                                )
                            })
                            .collect::<Result<_>>()?;
                        for result in space.install_batch(&pending, |_| Ok(()))? {
                            let result = result.context("Replay hit was collected")?;
                            ensure!(
                                result.reused == reuse && result.built != reuse,
                                "Unexpected replay outcome"
                            );
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Replay worker panicked"))??;
        }
        Ok(())
    })
}
fn queries(space: &Scope) -> Result<Vec<BTreeSet<String>>> {
    let tx = space.query_read()?;
    let run = || -> Result<Vec<BTreeSet<String>>> {
        Ok(vec![
            space.candidates(&tx, "Type00017", false, true, |_| false)?,
            space.candidates(&tx, "Type*03", false, false, |name| name.ends_with("03"))?,
            space.candidates(&tx, "Use*03", true, false, |name| name.ends_with("03"))?,
        ])
    };
    let started = Instant::now();
    let expected = run()?;
    let cold = started.elapsed();
    let started = Instant::now();
    for _ in 0..20 {
        assert_eq!(run()?, expected);
    }
    eprintln!(
        "queries: first_us={} warm_mean_us={} candidates={:?}",
        cold.as_micros(),
        started.elapsed().as_micros() / 20,
        expected.iter().map(BTreeSet::len).collect::<Vec<_>>()
    );
    Ok(expected)
}

#[test]
#[ignore = "opt-in storage replay; writes about 625 MiB with defaults"]
fn replay_storage_publication() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sigla::store::shared::publication=debug".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let files = setting("SIGLA_STORAGE_FILES", 5000)?;
    let bytes = setting("SIGLA_STORAGE_PAYLOAD_BYTES", 128 * 1024)?;
    ensure!(
        (1..=1_000_000).contains(&files),
        "Invalid replay file count"
    );
    ensure!(bytes <= 32 * 1024 * 1024, "Replay object exceeds 32 MiB");
    let parent = std::env::var_os("SIGLA_STORAGE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = tempfile::tempdir_in(parent)?;
    let db = Database::open(directory.path())?;
    let keep: BTreeSet<_> = (0..files).map(file).collect();
    let mut expected = None;
    for view in 1..=4u8 {
        let space = db.workspace(
            [view; 32],
            &PathBuf::from(format!("/storage-replay/{view}")),
            None,
        )?;
        let before = { space.read()?.id() };
        let disk = db.disk_bytes()?;
        let started = Instant::now();
        publish(&space, files, bytes, view != 1)?;
        let indexing = started.elapsed();
        let commits = space.read()?.id() - before;
        let started = Instant::now();
        space.finish_refresh(b"storage replay", &keep, 100)?;
        eprintln!(
            "view={view} files={files} reuse={} indexing_ms={} install_commits={} finish_ms={} allocated_delta={}",
            view != 1,
            indexing.as_millis(),
            commits,
            started.elapsed().as_millis(),
            db.disk_bytes()?.saturating_sub(disk)
        );
        let found = queries(&space)?;
        if let Some(expected) = &expected {
            assert_eq!(&found, expected);
        } else {
            expected = Some(found);
        }
    }
    Ok(())
}
