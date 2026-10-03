//! One Linux bubblewrap launcher for remote project-aware managed operations.
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Local projects retain host inputs and credentials, with writes confined to cache and private IPC.
pub fn local_command(host: &Path, directory: &Path, cache: &Path, job: &Path) -> Result<Command> {
    let cache = cache.join("dotnet").canonicalize()?;
    for child in ["tmp", "home", "packages", "http-cache", "plugins"] {
        fs::create_dir_all(cache.join(child))?;
    }
    let mut command = Command::new("bwrap");
    command
        .args([
            "--die-with-parent",
            "--unshare-pid",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            // .NET named mutexes use this fixed path, ignoring TMPDIR.
            "--tmpfs",
            "/tmp/.dotnet",
            // Precreate both roots: .NET otherwise initializes them by renaming
            // temporary siblings from the read-only /tmp directory.
            "--dir",
            "/tmp/.dotnet/shm",
            "--dir",
            "/tmp/.dotnet/lockfiles",
        ])
        .arg("--bind")
        .arg(&cache)
        .arg(&cache)
        .arg("--bind")
        .arg(job)
        .arg(job)
        .args(["--proc", "/proc", "--dev", "/dev"])
        .arg("--chdir")
        .arg(directory);
    // DOTNET_CLI_HOME relocates NuGet's user settings too. Keep existing feed credentials readable.
    if let Some(home) = std::env::var_os("DOTNET_CLI_HOME").or_else(|| std::env::var_os("HOME")) {
        let config = PathBuf::from(home).join(".nuget/NuGet");
        if config.is_dir() {
            let target = cache.join("home/.nuget/NuGet");
            fs::create_dir_all(&target)?;
            command.arg("--ro-bind").arg(config).arg(target);
        }
    }
    command
        .arg("--")
        .arg(host)
        .env("TMPDIR", cache.join("tmp"))
        .env("DOTNET_CLI_HOME", cache.join("home"))
        .env("NUGET_PACKAGES", cache.join("packages"))
        .env("NUGET_HTTP_CACHE_PATH", cache.join("http-cache"))
        .env("NUGET_PLUGINS_CACHE_PATH", cache.join("plugins"))
        .env("MSBUILDDISABLENODEREUSE", "1")
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
        .env("DOTNET_NOLOGO", "1");
    Ok(command)
}

pub struct Sandbox {
    pub source: PathBuf,
    pub writable: PathBuf,
    pub toolchains: BTreeSet<PathBuf>,
    hidden: Vec<(PathBuf, bool)>,
}

/// Remove generated outputs after all sandbox processes have stopped.
pub fn remove_outputs(writable: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Overlayfs leaves its private work directory at mode 000 after unmounting.
    // It is outside the mounted workspace and is never exposed to project code.
    let work = writable.join("overlay-work/work");
    match fs::symlink_metadata(&work) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "Invalid overlay work directory");
            fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::remove_dir_all(writable) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

impl Sandbox {
    pub fn new(
        context: &crate::discovery::RemoteContext,
        toolchain_roots: &[PathBuf],
    ) -> Result<Self> {
        let mut toolchains = BTreeSet::new();
        for root in toolchain_roots {
            let root = root.canonicalize()?;
            if root.starts_with("/nix/store") {
                let store = root
                    .ancestors()
                    .find(|p| p.parent() == Some(Path::new("/nix/store")))
                    .context("Invalid installed toolchain location")?;
                let closure = crate::process::run(
                    Command::new("nix-store")
                        .args(["--query", "--requisites"])
                        .arg(store),
                    Duration::from_secs(30),
                )?;
                for path in std::str::from_utf8(&closure)?.lines() {
                    let path = PathBuf::from(path);
                    ensure!(
                        path.parent() == Some(Path::new("/nix/store")),
                        "Invalid toolchain dependency location"
                    );
                    toolchains.insert(path);
                }
            } else {
                toolchains.insert(root);
            }
        }
        for child in ["upper", "overlay-work", "packages", "jobs"] {
            fs::create_dir_all(context.writable.join(child))?;
        }
        let source = context.workspace.canonicalize()?;
        fn git_paths(
            root: &Path,
            directory: &Path,
            paths: &mut Vec<(PathBuf, bool)>,
        ) -> Result<()> {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                if entry.file_name() == ".git" {
                    paths.push((
                        Path::new("/workspace").join(entry.path().strip_prefix(root)?),
                        kind.is_dir(),
                    ));
                } else if kind.is_dir() {
                    git_paths(root, &entry.path(), paths)?;
                }
            }
            Ok(())
        }
        let mut hidden = Vec::new();
        git_paths(&source, &source, &mut hidden)?;
        Ok(Self {
            source,
            writable: context.writable.canonicalize()?,
            toolchains,
            hidden,
        })
    }

    pub fn command(
        &self,
        host: &Path,
        integration: &Path,
        job: &Path,
        directory: &Path,
        network: bool,
    ) -> Result<Command> {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        write_job_file(
            &job.join("passwd"),
            format!("sigla:x:{uid}:{gid}:Sigla:/home/sigla:/nonexistent\n").as_bytes(),
        )?;
        write_job_file(&job.join("group"), format!("sigla:x:{gid}:\n").as_bytes())?;
        write_job_file(
            &job.join("nsswitch.conf"),
            b"passwd: files\ngroup: files\nhosts: files dns\n",
        )?;
        let mut command = Command::new("bwrap");
        command.args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--cap-drop",
            "ALL",
        ]);
        if network {
            command.arg("--share-net");
        }
        command.args([
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/home/sigla",
        ]);
        for name in ["passwd", "group", "nsswitch.conf"] {
            command
                .arg("--ro-bind")
                .arg(job.join(name))
                .arg(format!("/etc/{name}"));
        }
        for path in &self.toolchains {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        if !host.starts_with("/nix/store") {
            for path in ["/lib", "/lib64", "/usr/lib", "/etc/ld.so.cache"] {
                if Path::new(path).exists() {
                    command.args(["--ro-bind", path, path]);
                }
            }
        }
        if network {
            for path in ["/etc/resolv.conf", "/etc/hosts"] {
                if Path::new(path).exists() {
                    command
                        .arg("--ro-bind")
                        .arg(Path::new(path).canonicalize()?)
                        .arg(path);
                }
            }
            command.args(["--dir", "/etc/ssl/certs"]);
            command
                .arg("--ro-bind")
                .arg(ca_bundle()?)
                .arg("/etc/ssl/certs/ca-certificates.crt")
                .args([
                    "--setenv",
                    "SSL_CERT_FILE",
                    "/etc/ssl/certs/ca-certificates.crt",
                ]);
        }
        command
            .arg("--overlay-src")
            .arg(&self.source)
            .arg("--overlay")
            .arg(self.writable.join("upper"))
            .arg(self.writable.join("overlay-work"))
            .arg("/workspace");
        for (path, directory) in &self.hidden {
            if *directory {
                command.arg("--tmpfs").arg(path);
            } else {
                command.args(["--ro-bind", "/dev/null"]).arg(path);
            }
        }
        command
            .arg("--ro-bind")
            .arg(integration)
            .arg("/integration")
            .arg("--bind")
            .arg(job)
            .arg("/job")
            .arg("--bind")
            .arg(self.writable.join("packages"))
            .arg("/packages")
            .args([
                "--setenv",
                "HOME",
                "/home/sigla",
                "--setenv",
                "TMPDIR",
                "/tmp",
                "--setenv",
                "DOTNET_CLI_HOME",
                "/home/sigla",
                "--setenv",
                "DOTNET_CLI_TELEMETRY_OPTOUT",
                "1",
                "--setenv",
                "DOTNET_SKIP_FIRST_TIME_EXPERIENCE",
                "1",
                "--setenv",
                "DOTNET_NOLOGO",
                "1",
                "--setenv",
                "DOTNET_PROCESSOR_COUNT",
                "2",
                "--setenv",
                "MSBUILDDISABLENODEREUSE",
                "1",
                "--setenv",
                "NUGET_PACKAGES",
                "/packages",
                "--setenv",
                "NUGET_HTTP_CACHE_PATH",
                "/tmp/nuget-http",
            ])
            .arg("--setenv")
            .arg("DOTNET_ROOT")
            .arg(host.parent().unwrap())
            .arg("--setenv")
            .arg("PATH")
            .arg(host.parent().unwrap())
            .arg("--chdir")
            .arg(self.input(directory)?)
            .arg("--")
            .arg(host);
        Ok(command)
    }

    pub fn input(&self, path: &Path) -> Result<PathBuf> {
        Ok(Path::new("/workspace").join(
            path.strip_prefix(&self.source)
                .context("Project entry is outside the approved workspace")?,
        ))
    }

    /// Called only after the sandbox and its descendants have terminated.
    pub fn output(&self, path: &Path) -> Result<PathBuf> {
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)),
            "Invalid returned discovery path"
        );
        if let Ok(relative) = path.strip_prefix("/workspace") {
            let canonical = self.source.join(relative);
            if canonical.exists() {
                return confined(&self.source, &canonical);
            }
            return confined(
                &self.writable.join("upper"),
                &self.writable.join("upper").join(relative),
            );
        }
        if let Ok(relative) = path.strip_prefix("/packages") {
            return confined(
                &self.writable.join("packages"),
                &self.writable.join("packages").join(relative),
            );
        }
        if self.toolchains.iter().any(|root| path.starts_with(root)) {
            let resolved = path.canonicalize()?;
            ensure!(
                self.toolchains
                    .iter()
                    .any(|root| resolved.starts_with(root)),
                "Toolchain reference escapes approved installation"
            );
            return Ok(resolved);
        }
        anyhow::bail!("Discovery returned a path outside approved locations")
    }

    pub fn watch_directory(&self, path: &Path) -> Result<PathBuf> {
        if let Ok(relative) = path.strip_prefix("/workspace") {
            ensure!(
                relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
                "Invalid returned source glob path"
            );
            let target = self.source.join(relative);
            if !target.exists() && !self.writable.join("upper").join(relative).exists() {
                let ancestor = target
                    .ancestors()
                    .find(|p| p.exists())
                    .context("Source glob has no approved ancestor")?;
                confined(&self.source, ancestor)?;
                return Ok(target);
            }
        }
        self.output(path)
    }

    pub fn validate_writes(&self) -> Result<()> {
        fn visit(root: &Path, source: &Path, upper: &Path) -> Result<()> {
            for entry in fs::read_dir(upper)? {
                let entry = entry?;
                let metadata = entry.file_type()?;
                ensure!(
                    !metadata.is_symlink(),
                    "Discovery generated an unsupported symbolic link"
                );
                let original = source.join(entry.file_name());
                if metadata.is_dir() {
                    visit(root, &original, &entry.path())?;
                } else {
                    ensure!(
                        metadata.is_file(),
                        "Discovery generated an unsupported special file"
                    );
                    if original.is_file() {
                        confined(root, &original)?;
                        ensure!(
                            fs::read(&original)? == fs::read(entry.path())?,
                            "MSBuild attempted to change a canonical project input"
                        );
                        fs::remove_file(entry.path())?;
                    }
                }
            }
            Ok(())
        }
        visit(&self.source, &self.source, &self.writable.join("upper"))
    }
}

fn ca_bundle() -> Result<PathBuf> {
    let configured = ["SSL_CERT_FILE", "NIX_SSL_CERT_FILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from);
    let path = configured.or_else(|| {
        [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/ssl/certs/ca-bundle.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
    });
    let path = path.context("No host CA certificate bundle is available for sandboxed restore")?;
    resolve_ca_bundle(&path)
}

fn resolve_ca_bundle(path: &Path) -> Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .with_context(|| format!("Cannot resolve CA certificate bundle {}", path.display()))?;
    ensure!(resolved.is_file(), "CA certificate bundle is not a file");
    Ok(resolved)
}

fn confined(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path.strip_prefix(root)?;
    let mut cursor = root.to_owned();
    for component in relative.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Invalid returned path component"
        );
        cursor.push(component);
        ensure!(
            !fs::symlink_metadata(&cursor)?.file_type().is_symlink(),
            "Discovery returned a symbolic link"
        );
    }
    ensure!(
        path.canonicalize()?.starts_with(root.canonicalize()?),
        "Discovery path escapes its approved mapping"
    );
    Ok(path.to_owned())
}

/// Job children are writable by project logic. Never follow their links in the parent.
pub fn write_job_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("Job path has no parent")?)?;
    file.write_all(bytes)?;
    file.persist(path)?;
    Ok(())
}

pub fn read_job_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "Discovery output is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Discovery output exceeds size limit"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_outputs_remove_private_overlay_work() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let outputs = root.path().join("generated");
        let work = outputs.join("overlay-work/work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("leftover"), "scratch").unwrap();
        fs::set_permissions(&work, fs::Permissions::from_mode(0o000)).unwrap();
        remove_outputs(&outputs).unwrap();
        assert!(!outputs.exists());
        remove_outputs(&outputs).unwrap();
    }

    #[test]
    fn ca_bundle_follows_symlinks_to_the_file() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("bundle.crt");
        fs::write(&bundle, b"certificate bundle").unwrap();
        let first = root.path().join("first.crt");
        let second = root.path().join("second.crt");
        std::os::unix::fs::symlink(&bundle, &first).unwrap();
        std::os::unix::fs::symlink(&first, &second).unwrap();
        assert_eq!(resolve_ca_bundle(&second).unwrap(), bundle);
    }

    #[test]
    fn job_files_cannot_redirect_parent_access() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("secret");
        let result = root.path().join("result");
        fs::write(&secret, b"private").unwrap();
        std::os::unix::fs::symlink(&secret, &result).unwrap();
        assert!(read_job_file(&result, 100).is_err());
        write_job_file(&result, b"request").unwrap();
        assert_eq!(fs::read(&secret).unwrap(), b"private");
        assert_eq!(read_job_file(&result, 100).unwrap(), b"request");
        assert!(read_job_file(&result, 2).is_err());
    }
}
