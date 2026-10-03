//! Downloads and confined extraction shared by Unity editors and packages.
mod archive;
use anyhow::{Context, Result, ensure};
use base64::Engine;
use sha2::Digest;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

type Preparation = Arc<Mutex<Option<(Instant, String)>>>;
static PREPARATIONS: LazyLock<Mutex<HashMap<String, Preparation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
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
    let state = PREPARATIONS
        .lock()
        .unwrap()
        .entry(identity.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone();
    let mut state = state.lock().unwrap();
    if let Some((failed, message)) = &*state {
        ensure!(failed.elapsed() >= Duration::from_secs(30), "{message}");
    }
    let mut active = CAPACITY.0.lock().unwrap();
    while *active >= 2 && !STOPPED.load(std::sync::atomic::Ordering::Acquire) {
        active = CAPACITY.1.wait(active).unwrap();
    }
    ensure!(
        !STOPPED.load(std::sync::atomic::Ordering::Acquire),
        "Service is shutting down"
    );
    *active += 1;
    drop(active);
    let _permit = Permit;
    let result = prepare();
    *state = result
        .as_ref()
        .err()
        .map(|error| (Instant::now(), error.to_string()));
    result
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

pub fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    file: &mut File,
    integrity: Option<&str>,
) -> Result<()> {
    let response = client
        .get(https(url)?)
        .send()
        .map_err(|_| anyhow::anyhow!("Dependency download connection failed"))?;
    transfer(response, file, integrity)
}

pub fn transfer(
    response: reqwest::blocking::Response,
    file: &mut File,
    integrity: Option<&str>,
) -> Result<()> {
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
            "Service is shutting down"
        );
        let count = response.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        file.write_all(&bytes[..count])?;
        if let Some((hash, _)) = &mut integrity {
            hash.update(&bytes[..count]);
        }
        size += count as u64;
    }
    ensure!(
        length.is_none_or(|expected| size == expected),
        "Dependency archive transfer was truncated"
    );
    file.sync_all()?;
    if let Some((hash, expected)) = integrity {
        let digest = hash.finalize().to_vec();
        // Unity encodes the hexadecimal MD5 text; registry SRI normally encodes raw digest bytes.
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        ensure!(
            expected == digest || expected == hex.as_bytes(),
            "Dependency archive integrity check failed"
        );
    }
    Ok(())
}

pub fn extract(
    reader: File,
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
