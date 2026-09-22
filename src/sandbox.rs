//! One Linux bubblewrap launcher for remote project-aware managed operations.
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

pub struct Sandbox {
    pub source: PathBuf,
    pub writable: PathBuf,
    pub toolchains: BTreeSet<PathBuf>,
    hidden: Vec<(PathBuf, bool)>,
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
            for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/ssl/certs"] {
                if Path::new(path).exists() {
                    command
                        .arg("--ro-bind")
                        .arg(Path::new(path).canonicalize()?)
                        .arg(path);
                }
            }
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
