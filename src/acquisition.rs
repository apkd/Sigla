//! Downloads and confined extraction shared by Unity editors and packages.
mod archive;
use anyhow::{Context, Result, ensure};
use base64::Engine;
use sha2::Digest;
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{Read, Write},
    os::fd::AsFd,
    path::{Component, Path},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

struct Failure {
    at: Instant,
    version: String,
    error: Arc<anyhow::Error>,
}
type Preparation = Arc<Mutex<Option<Failure>>>;
struct Preparations {
    entries: Mutex<HashMap<String, Preparation>>,
    limit: usize,
}
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
static PREPARATIONS: LazyLock<Preparations> = LazyLock::new(|| Preparations {
    entries: Mutex::new(HashMap::new()),
    limit: 1024,
});

impl Preparations {
    fn enter<'a>(&'a self, identity: &'a str) -> Result<PreparationEntry<'a>> {
        let mut entries = self.entries.lock().unwrap();
        if !entries.contains_key(identity) {
            entries.retain(|_, entry| !Self::removable(entry));
            ensure!(
                entries.len() < self.limit,
                crate::diagnostics::DependencyUnavailable(
                    "Dependency preparation is busy; retry shortly"
                )
            );
        }
        let state = entries
            .entry(identity.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        Ok(PreparationEntry {
            registry: self,
            identity,
            state: Some(state),
        })
    }

    fn removable(entry: &Preparation) -> bool {
        Arc::strong_count(entry) == 1 && Self::finished(entry, true)
    }

    fn finished(entry: &Preparation, expired: bool) -> bool {
        let state = match entry.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        state
            .as_ref()
            .is_none_or(|failure| expired && failure.at.elapsed() >= FAILURE_COOLDOWN)
    }
}

struct PreparationEntry<'a> {
    registry: &'a Preparations,
    identity: &'a str,
    state: Option<Preparation>,
}
impl PreparationEntry<'_> {
    fn state(&self) -> &Mutex<Option<Failure>> {
        self.state.as_deref().unwrap()
    }
}
impl Drop for PreparationEntry<'_> {
    fn drop(&mut self) {
        let mut entries = self.registry.entries.lock().unwrap();
        // Release this owner's reference under the registry lock. Otherwise two
        // concurrent drops can both observe the other owner and skip removal.
        drop(self.state.take());
        let state = &entries[self.identity];
        if Arc::strong_count(state) == 1 && Preparations::finished(state, false) {
            entries.remove(self.identity);
        }
    }
}
static CAPACITY: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());
static STOPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn shutdown() {
    STOPPED.store(true, std::sync::atomic::Ordering::Release);
    CAPACITY.1.notify_all();
}
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        *CAPACITY.0.lock().unwrap() -= 1;
        CAPACITY.1.notify_one();
    }
}

pub fn shared<T>(identity: &str, prepare: impl FnOnce() -> Result<T>) -> Result<T> {
    shared_versioned(identity, "", prepare)
}

/// Serialize one cache destination while allowing a new source location to retry.
pub(crate) fn shared_versioned<T>(
    identity: &str,
    version: &str,
    prepare: impl FnOnce() -> Result<T>,
) -> Result<T> {
    serialized_versioned(identity, version, || admitted(prepare))
}

fn admitted<T>(prepare: impl FnOnce() -> Result<T>) -> Result<T> {
    let mut active = CAPACITY.0.lock().unwrap();
    while *active >= 2 && !STOPPED.load(std::sync::atomic::Ordering::Acquire) {
        active = CAPACITY.1.wait(active).unwrap();
    }
    check_running()?;
    *active += 1;
    drop(active);
    let _permit = Permit;
    prepare()
}

/// Lock destinations before taking one permit for their combined preparation.
/// Record each destination's preparation and publication failures independently.
pub(crate) fn shared_many_versioned<T>(
    identities: &[String],
    version: &str,
    prepare: impl FnOnce(&[usize]) -> Vec<Result<T>>,
) -> Result<Vec<Result<T>>> {
    let mut order: Vec<_> = (0..identities.len()).collect();
    order.sort_unstable_by_key(|&index| &identities[index]);
    ensure!(
        order
            .windows(2)
            .all(|pair| identities[pair[0]] != identities[pair[1]]),
        "Repeated acquisition destination"
    );
    let entries = order
        .iter()
        .map(|&index| PREPARATIONS.enter(&identities[index]))
        .collect::<Result<Vec<_>>>()?;
    let mut states: BTreeMap<_, _> = order
        .into_iter()
        .zip(entries.iter().map(|entry| {
            entry
                .state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }))
        .collect();
    let mut results = BTreeMap::new();
    let mut pending = Vec::new();
    for (&index, state) in &states {
        match check_failure(state, version) {
            Ok(()) => pending.push(index),
            Err(error) => {
                results.insert(index, Err(error));
            }
        }
    }
    if !pending.is_empty() {
        match admitted(|| Ok(prepare(&pending))) {
            Ok(prepared) => {
                assert_eq!(prepared.len(), pending.len(), "Missing acquisition results");
                for (index, result) in pending.into_iter().zip(prepared) {
                    results.insert(
                        index,
                        remember(states.get_mut(&index).unwrap(), version, result),
                    );
                }
            }
            Err(error) => {
                let error = Arc::new(error);
                for index in pending {
                    results.insert(index, Err(crate::diagnostics::shared_error(error.clone())));
                }
            }
        }
    }
    Ok(results.into_values().collect())
}

/// Observe completed failures without waiting on an acquisition in progress.
pub(crate) fn check_cached_failure(identity: &str, version: &str) -> Result<()> {
    let entry = PREPARATIONS.entries.lock().unwrap().get(identity).cloned();
    let Some(entry) = entry else {
        return check_running();
    };
    match entry.try_lock() {
        Ok(state) => check_failure(&state, version),
        Err(std::sync::TryLockError::Poisoned(error)) => {
            check_failure(&error.into_inner(), version)
        }
        Err(std::sync::TryLockError::WouldBlock) => check_running(),
    }
}

/// Serialize a destination whose preparation owns its own resource admission.
pub(crate) fn serialized<T>(identity: &str, prepare: impl FnOnce() -> Result<T>) -> Result<T> {
    serialized_versioned(identity, "", prepare)
}

pub(crate) fn serialized_versioned<T>(
    identity: &str,
    version: &str,
    prepare: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let entry = PREPARATIONS.enter(identity)?;
    let mut state = entry
        .state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_failure(&state, version)?;
    remember(&mut state, version, prepare())
}

fn check_failure(state: &Option<Failure>, version: &str) -> Result<()> {
    if let Some(failure) = state
        && failure.version == version
        && failure.at.elapsed() < FAILURE_COOLDOWN
    {
        return Err(crate::diagnostics::shared_error(failure.error.clone()));
    }
    check_running()
}

pub(crate) fn check_running() -> Result<()> {
    ensure!(
        !STOPPED.load(std::sync::atomic::Ordering::Acquire),
        crate::diagnostics::Cancelled
    );
    Ok(())
}

fn remember<T>(state: &mut Option<Failure>, version: &str, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            *state = None;
            Ok(value)
        }
        Err(error) => {
            if crate::diagnostics::cancelled(&error) {
                *state = None;
                return Err(error);
            }
            let error = Arc::new(error);
            *state = Some(Failure {
                at: Instant::now(),
                version: version.into(),
                error: error.clone(),
            });
            Err(crate::diagnostics::shared_error(error))
        }
    }
}

pub fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(3600))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            // Official package tarballs redirect to Unity's CDN. Keep redirects
            // HTTPS-only and bounded; reqwest strips credentials across origins.
            let url = attempt.url();
            if attempt.previous().len() >= 5 {
                attempt.error("Too many dependency redirects")
            } else if url.scheme() != "https"
                || !url.username().is_empty()
                || url.password().is_some()
            {
                attempt.error("Dependency redirects require HTTPS without embedded credentials")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

pub fn https(url: &str) -> Result<url::Url> {
    let url = url::Url::parse(url)?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Dependency download requires an HTTPS URL without embedded credentials"
    );
    Ok(url)
}

pub fn download_extract(
    client: &reqwest::blocking::Client,
    url: &str,
    destination: &Path,
    integrity: Option<&str>,
    retain: impl Fn(&Path) -> bool,
) -> Result<(Vec<String>, u64)> {
    let response = client
        .get(https(url)?)
        .send()
        .map_err(|_| anyhow::anyhow!("Dependency download connection failed"))?;
    extract_transfer(response, destination, integrity, retain)
}

fn extract_transfer(
    response: reqwest::blocking::Response,
    destination: &Path,
    integrity: Option<&str>,
    retain: impl Fn(&Path) -> bool,
) -> Result<(Vec<String>, u64)> {
    receive(
        |writer| transfer_verified(response, writer, integrity),
        |reader| extract(reader.as_fd(), destination, retain),
    )
}

pub(crate) fn stream<T>(
    response: reqwest::blocking::Response,
    consume: impl FnOnce(&mut dyn Read) -> Result<T>,
) -> Result<T> {
    receive(
        |writer| transfer_verified(response, writer, None),
        |reader| consume(reader),
    )
    .map(|(value, _)| value)
}

fn receive<T>(
    download: impl FnOnce(&mut std::io::PipeWriter) -> Result<u64> + Send,
    consume: impl FnOnce(&mut std::io::PipeReader) -> Result<T>,
) -> Result<(T, u64)> {
    let (mut reader, mut writer) = std::io::pipe()?;
    std::thread::scope(|scope| {
        let download = scope.spawn(move || download(&mut writer));
        let extracted = consume(&mut reader);
        // Tar readers may stop before the compressed stream ends. Drain the
        // bounded pipe so every downloaded byte participates in verification.
        let drained = if extracted.is_ok() {
            std::io::copy(&mut reader, &mut std::io::sink()).map(|_| ())
        } else {
            Ok(())
        };
        // On extraction failure, closing the reader releases a blocked writer.
        drop(reader);
        let downloaded = download
            .join()
            .map_err(|_| anyhow::anyhow!("Archive download worker panicked"))?;
        // A decoder rejection closes the pipe. Other transfer failures must
        // preserve their original cause, especially typed cancellation.
        match (extracted, downloaded) {
            (Err(error), _) if crate::diagnostics::cancelled(&error) => Err(error),
            (Err(extracted), Err(downloaded))
                if downloaded
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe) =>
            {
                Err(extracted)
            }
            (_, Err(error)) => Err(error),
            (Err(error), _) => Err(error),
            (Ok(inventory), Ok(bytes)) => {
                drained?;
                Ok((inventory, bytes))
            }
        }
    })
}

pub fn transfer(
    response: reqwest::blocking::Response,
    file: &mut File,
    integrity: Option<&str>,
) -> Result<()> {
    transfer_verified(response, file, integrity)?;
    file.sync_all()?;
    Ok(())
}

fn transfer_verified(
    response: reqwest::blocking::Response,
    output: &mut impl Write,
    integrity: Option<&str>,
) -> Result<u64> {
    ensure!(
        response.status().is_success(),
        "Dependency download returned HTTP {}",
        response.status().as_u16()
    );
    let length = response.content_length();
    let mut response = response;
    let mut integrity = integrity
        .map(|integrity| -> Result<_> {
            let (algorithm, encoded) = integrity
                .split_once('-')
                .context("Invalid archive integrity metadata")?;
            let hash: Box<dyn sha2::digest::DynDigest> = match algorithm {
                "md5" => Box::new(md5::Md5::new()),
                "sha1" => Box::new(sha1::Sha1::new()),
                "sha256" => Box::new(sha2::Sha256::new()),
                "sha512" => Box::new(sha2::Sha512::new()),
                _ => anyhow::bail!("Unsupported archive integrity algorithm"),
            };
            Ok((
                hash,
                base64::engine::general_purpose::STANDARD.decode(encoded)?,
            ))
        })
        .transpose()?;
    let mut size = 0u64;
    let mut bytes = [0u8; 128 * 1024];
    loop {
        ensure!(
            !STOPPED.load(std::sync::atomic::Ordering::Acquire),
            crate::diagnostics::Cancelled
        );
        let count = response.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        output.write_all(&bytes[..count])?;
        if let Some((hash, _)) = &mut integrity {
            hash.update(&bytes[..count]);
        }
        size += count as u64;
    }
    ensure!(
        length.is_none_or(|expected| size == expected),
        "Dependency archive transfer was truncated"
    );
    if let Some((hash, expected)) = integrity {
        let digest = hash.finalize().to_vec();
        // Unity encodes the hexadecimal MD5 text; registry SRI normally encodes raw digest bytes.
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        ensure!(
            expected == digest || expected == hex.as_bytes(),
            "Dependency archive integrity check failed"
        );
    }
    Ok(size)
}

pub fn extract(
    reader: impl std::os::fd::AsRawFd,
    destination: &Path,
    retain: impl Fn(&Path) -> bool,
) -> Result<Vec<String>> {
    let mut archive = archive::Archive::open(reader)?;
    let mut retained = Vec::new();
    while let Some(entry) = archive.next()? {
        let path = entry.path;
        ensure!(
            !path.is_absolute()
                && path
                    .components()
                    .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
            "Archive path escapes extraction directory"
        );
        let path: std::path::PathBuf = path
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        if entry.link || !entry.regular || !retain(&path) {
            continue;
        }
        ensure!(!path.as_os_str().is_empty(), "Empty archive file path");
        let output = destination.join(&path);
        fs::create_dir_all(output.parent().unwrap())?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&output)
            .with_context(|| {
                format!("Cannot extract {} to {}", path.display(), output.display())
            })?;
        archive
            .copy(&mut file)
            .with_context(|| format!("Cannot decode {}", path.display()))?;
        retained.push(
            path.to_str()
                .context("Archive path is not UTF-8")?
                .to_owned(),
        );
    }
    Ok(retained)
}

pub fn analysis_input(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("cs" | "asmdef" | "asmref" | "rsp" | "dll")
    ) || crate::native::language(path).is_some()
        || name == "package.json"
        || [".asmdef.meta", ".asmref.meta", ".dll.meta", ".rsp.meta"]
            .iter()
            .any(|s| name.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_preparation_keeps_order_and_independent_cooldowns() {
        let root = tempfile::tempdir().unwrap();
        let keys: Vec<_> = ["last", "first", "middle"]
            .map(|name| root.path().join(name).display().to_string())
            .into();
        let first = shared_many_versioned(&keys, "one", |indices| {
            assert_eq!(indices, &[0, 1, 2]);
            assert!(check_cached_failure(&keys[0], "one").is_ok());
            vec![Ok(0), Err(anyhow::anyhow!("bad member")), Ok(2)]
        })
        .unwrap();
        assert!(first[0].is_ok());
        assert!(first[1].is_err());
        assert!(first[2].is_ok());
        assert!(check_cached_failure(&keys[1], "one").is_err());
        let retried = shared_many_versioned(&keys, "one", |indices| {
            assert_eq!(indices, &[0, 2]);
            indices.iter().map(|&index| Ok(index)).collect()
        })
        .unwrap();
        assert_eq!(*retried[0].as_ref().unwrap(), 0);
        assert!(retried[1].is_err());
        assert_eq!(*retried[2].as_ref().unwrap(), 2);
        let blocked =
            shared_many_versioned::<()>(&keys[1..2], "one", |_| panic!("Cached failure retried"))
                .unwrap();
        assert!(blocked[0].is_err());
        assert!(
            shared_many_versioned::<()>(&[keys[0].clone(), keys[0].clone()], "one", |_| panic!(
                "Duplicate keys admitted"
            ))
            .is_err()
        );
        let changed = shared_many_versioned(&keys, "two", |indices| {
            indices.iter().map(|_| Ok(())).collect()
        })
        .unwrap();
        assert!(changed.iter().all(Result::is_ok));
    }

    #[test]
    fn preparation_registry_preserves_waiters_and_bounds_failure_bursts() {
        let registry = Preparations {
            entries: Mutex::new(HashMap::new()),
            limit: 1,
        };
        std::thread::scope(|scope| {
            let registry = &registry;
            let first = registry.enter("one").unwrap();
            let state = first.state().lock().unwrap();
            let (entered, ready) = std::sync::mpsc::channel();
            let waiter = scope.spawn(move || {
                let second = registry.enter("one").unwrap();
                entered.send(()).unwrap();
                let _state = second.state().lock().unwrap();
            });
            ready.recv().unwrap();
            assert!(registry.enter("two").is_err());
            drop(state);
            drop(first);
            waiter.join().unwrap();
        });
        assert!(registry.entries.lock().unwrap().is_empty());

        let failed = registry.enter("one").unwrap();
        *failed.state().lock().unwrap() = Some(Failure {
            at: Instant::now(),
            version: "release".into(),
            error: Arc::new(anyhow::anyhow!("unavailable")),
        });
        drop(failed);
        assert!(registry.enter("two").is_err());
        let failed = registry.enter("one").unwrap();
        failed.state().lock().unwrap().as_mut().unwrap().at = Instant::now() - FAILURE_COOLDOWN;
        drop(failed);
        drop(registry.enter("two").unwrap());
        assert!(registry.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn changed_acquisition_version_bypasses_a_preserved_failure() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().to_str().unwrap();
        let failed = serialized_versioned::<()>(key, "old", || {
            Err(crate::diagnostics::DependencyUnavailable("unavailable").into())
        });
        assert!(failed.is_err());
        let cached = serialized_versioned::<()>(key, "old", || panic!("failure cooldown was lost"));
        assert!(
            cached
                .unwrap_err()
                .chain()
                .any(|cause| cause.is::<crate::diagnostics::DependencyUnavailable>())
        );
        serialized_versioned(key, "new", || Ok(())).unwrap();
        assert!(!PREPARATIONS.entries.lock().unwrap().contains_key(key));
    }

    #[test]
    fn cancelled_or_panicking_acquisitions_allow_an_immediate_retry() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().to_str().unwrap();
        let cancelled = serialized(key, || {
            receive(
                |writer| {
                    writer.write_all(b"x")?;
                    Err(crate::diagnostics::Cancelled.into())
                },
                |reader| {
                    reader.read_exact(&mut [0; 2])?;
                    Ok(())
                },
            )
        });
        assert!(crate::diagnostics::cancelled(&cancelled.unwrap_err()));
        serialized(key, || Ok(())).unwrap();
        assert!(
            std::panic::catch_unwind(|| serialized::<()>(key, || panic!("failed worker"))).is_err()
        );
        serialized(key, || Ok(())).unwrap();
        assert!(!PREPARATIONS.entries.lock().unwrap().contains_key(key));
    }

    #[test]
    fn streamed_failures_keep_transport_and_consumer_causes() {
        let disconnected = receive(
            |writer| {
                writer.write_all(b"x")?;
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset).into())
            },
            |reader| {
                reader.read_exact(&mut [0; 2])?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(
            disconnected
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::ConnectionReset
        );

        let rejected = receive(
            |writer| {
                writer.write_all(&[0; 128 * 1024])?;
                Ok(0)
            },
            |_| Err::<(), _>(crate::diagnostics::DependencyUnavailable("invalid archive").into()),
        )
        .unwrap_err();
        assert!(rejected.is::<crate::diagnostics::DependencyUnavailable>());

        let cancelled = receive(
            |_| Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset).into()),
            |_| Err::<(), _>(crate::diagnostics::Cancelled.into()),
        )
        .unwrap_err();
        assert!(crate::diagnostics::cancelled(&cancelled));
    }

    fn archive(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        std::io::Seek::rewind(&mut file).unwrap();
        file
    }

    fn tar(paths: &[&str]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for path in paths {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, b"x".as_slice())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn compressed_packages_extract() {
        use flate2::{Compression, write::GzEncoder};

        let bytes = tar(&["package/Code.cs"]);
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&bytes).unwrap();
        for compressed in [
            gzip.finish().unwrap(),
            zstd::encode_all(bytes.as_slice(), 0).unwrap(),
        ] {
            let root = tempfile::tempdir().unwrap();
            let inventory = extract(archive(&compressed), root.path(), analysis_input).unwrap();
            assert_eq!(inventory.len(), 1);
            assert_eq!(fs::read(root.path().join("package/Code.cs")).unwrap(), b"x");
        }
    }

    #[test]
    fn streamed_extraction_verifies_trailing_bytes_and_releases_failed_readers() {
        use std::{io::BufRead, net::TcpListener};

        fn response(
            bytes: Vec<u8>,
            length: usize,
            extracted: Option<std::sync::mpsc::Receiver<()>>,
        ) -> reqwest::blocking::Response {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                for line in std::io::BufReader::new(&mut stream).lines() {
                    if line.unwrap().is_empty() {
                        break;
                    }
                }
                // The client may close early when the archive is invalid.
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                )
                .and_then(|_| {
                    if let Some(extracted) = extracted {
                        let (prefix, remaining) = bytes.split_at(128 * 1024);
                        stream.write_all(prefix)?;
                        extracted.recv_timeout(Duration::from_secs(10)).unwrap();
                        stream.write_all(remaining)
                    } else {
                        stream.write_all(&bytes)
                    }
                });
            });
            reqwest::blocking::get(format!("http://{address}")).unwrap()
        }

        let mut bytes = tar(&["package/Code.cs"]);
        // Exceed the pipe capacity after tar's end marker: the consumer must
        // drain these bytes rather than blocking or accepting a partial hash.
        bytes.resize(bytes.len() + 512 * 1024, 0);
        let integrity = format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&bytes))
        );
        let root = tempfile::tempdir().unwrap();
        let (ready, extracted) = std::sync::mpsc::sync_channel(0);
        let (inventory, size) = extract_transfer(
            response(bytes.clone(), bytes.len(), Some(extracted)),
            root.path(),
            Some(&integrity),
            |path| {
                ready.send(()).unwrap();
                analysis_input(path)
            },
        )
        .unwrap();
        assert_eq!(inventory.len(), 1);
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(fs::read(root.path().join("package/Code.cs")).unwrap(), b"x");

        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&bytes).unwrap();
        for body in [
            gzip.finish().unwrap(),
            zstd::encode_all(bytes.as_slice(), 0).unwrap(),
        ] {
            let root = tempfile::tempdir().unwrap();
            let integrity = format!(
                "sha256-{}",
                base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&body))
            );
            let (inventory, size) = extract_transfer(
                response(body.clone(), body.len(), None),
                root.path(),
                Some(&integrity),
                analysis_input,
            )
            .unwrap();
            assert_eq!(inventory.len(), 1);
            assert_eq!(size, body.len() as u64);
            assert_eq!(fs::read(root.path().join("package/Code.cs")).unwrap(), b"x");
        }

        let mut corrupt = bytes.clone();
        *corrupt.last_mut().unwrap() = 1;
        for (body, length) in [
            (corrupt, bytes.len()),
            (bytes.clone(), bytes.len() + 1),
            (vec![b'x'; bytes.len()], bytes.len()),
        ] {
            let root = tempfile::tempdir().unwrap();
            assert!(
                extract_transfer(
                    response(body, length, None),
                    root.path(),
                    Some(&integrity),
                    analysis_input,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn unity_long_names_remain_distinct_with_nonstandard_tar_magic() {
        let directory = "long-directory/".repeat(8);
        let first = format!("package/{directory}First.cs");
        let second = format!("package/{directory}Second.cs");
        let mut bytes = tar(&[&first, &second]);
        let mut offset = 0;
        while offset + 512 <= bytes.len() && bytes[offset..offset + 512].iter().any(|b| *b != 0) {
            let mut header = tar::Header::from_byte_slice(&bytes[offset..offset + 512]).clone();
            let size = header.entry_size().unwrap() as usize;
            header.as_mut_bytes()[257..265].copy_from_slice(b"ustar\0 \0");
            header.set_cksum();
            bytes[offset..offset + 512].copy_from_slice(header.as_bytes());
            offset += 512 + size.div_ceil(512) * 512;
        }
        let root = tempfile::tempdir().unwrap();
        extract(archive(&bytes), root.path(), analysis_input).unwrap();
        for path in [first, second] {
            assert_eq!(fs::read(root.path().join(path)).unwrap(), b"x");
        }
    }

    #[test]
    fn duplicate_files_and_escaping_paths_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            extract(
                archive(&tar(&["Code.cs", "Code.cs"])),
                root.path(),
                analysis_input
            )
            .is_err()
        );
        let mut bytes = tar(&["Fine.cs"]);
        let mut header = tar::Header::from_byte_slice(&bytes[..512]).clone();
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..10].copy_from_slice(b"../Bad.cs\0");
        header.set_cksum();
        bytes[..512].copy_from_slice(header.as_bytes());
        assert!(extract(archive(&bytes), root.path(), analysis_input).is_err());
    }

    #[test]
    #[ignore = "requires SIGLA_TEST_ARCHIVES containing downloaded package paths"]
    fn downloaded_package_archives_extract() {
        let Some(paths) = std::env::var_os("SIGLA_TEST_ARCHIVES") else {
            return;
        };
        for path in std::env::split_paths(&paths) {
            let root = tempfile::tempdir().unwrap();
            let inventory =
                extract(File::open(&path).unwrap(), root.path(), analysis_input).unwrap();
            assert!(!inventory.is_empty());
            assert!(
                inventory
                    .iter()
                    .all(|path| root.path().join(path).is_file())
            );
            eprintln!("{}: {} selected files", path.display(), inventory.len());
        }
    }
    #[test]
    fn extraction_keeps_analysis_inputs_without_unpacking_assets() {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes) in [
            ("package/Code.cs", "class Code {}"),
            ("package/texture.png", "binary asset"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, bytes.as_bytes())
                .unwrap();
        }
        let bytes = builder.into_inner().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut archive = tempfile::tempfile().unwrap();
        archive.write_all(&bytes).unwrap();
        std::io::Seek::rewind(&mut archive).unwrap();
        extract(archive, root.path(), analysis_input).unwrap();
        assert!(root.path().join("package/Code.cs").is_file());
        assert!(!root.path().join("package/texture.png").exists());
    }
}
