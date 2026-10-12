//! Bounded subprocess execution shared by preparation operations.
use anyhow::{Context, Result, ensure};
use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
static OWNS_GROUP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static STOPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CHILDREN: std::sync::Mutex<std::collections::BTreeSet<i32>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

pub fn shutdown() {
    STOPPED.store(true, std::sync::atomic::Ordering::Release);
    for pid in CHILDREN.lock().unwrap().iter() {
        unsafe {
            libc::kill(-*pid, libc::SIGKILL);
        }
    }
}

/// Internal worker subprocesses already belong to the service-owned job group.
pub fn inherit_process_group() {
    OWNS_GROUP.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// A persistent subprocess uses the same shutdown registry as one-shot jobs.
/// Dropping it kills its group and reaps the child, including on read errors.
pub(crate) struct Child {
    process: std::process::Child,
    group: bool,
    stopped: bool,
}
impl std::ops::Deref for Child {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.process
    }
}
impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}
impl Child {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        let pid = self.process.id() as i32;
        unsafe {
            libc::kill(if self.group { -pid } else { pid }, libc::SIGKILL);
        }
        let _ = self.process.wait();
        if self.group {
            CHILDREN.lock().unwrap().remove(&pid);
        }
        self.stopped = true;
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) fn spawn(command: &mut Command) -> Result<Child> {
    use std::os::unix::process::CommandExt;
    ensure!(
        !STOPPED.load(std::sync::atomic::Ordering::Acquire),
        "Service is shutting down"
    );
    let group = OWNS_GROUP.load(std::sync::atomic::Ordering::Relaxed);
    if group {
        command.process_group(0);
    }
    let process = command.spawn().with_context(|| {
        format!(
            "Cannot start required subprocess `{}`",
            command.get_program().to_string_lossy()
        )
    })?;
    let mut child = Child {
        process,
        group,
        stopped: false,
    };
    if group {
        CHILDREN.lock().unwrap().insert(child.id() as i32);
        if STOPPED.load(std::sync::atomic::Ordering::Acquire) {
            child.stop();
        }
    }
    Ok(child)
}

fn read(mut stream: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Ok((output, truncated));
        }
        let retained = count.min(OUTPUT_LIMIT - output.len());
        output.extend_from_slice(&buffer[..retained]);
        truncated |= retained != count;
    }
}

pub struct Output {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn run(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    let output = capture(command, timeout, None, None)?;
    ensure!(
        output.status.success(),
        "Preparation subprocess failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

pub fn capture(
    command: &mut Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
    destination: Option<std::fs::File>,
) -> Result<Output> {
    capture_cancellable(command, timeout, input, destination, None)
}

pub(crate) fn capture_cancellable(
    command: &mut Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
    destination: Option<std::fs::File>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(destination.map(Stdio::from).unwrap_or_else(Stdio::piped))
        .stderr(Stdio::piped());
    let mut child = spawn(command)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take().unwrap();
    let writer = input.zip(stdin).map(|(bytes, mut stdin)| {
        thread::spawn(move || std::io::Write::write_all(&mut stdin, &bytes))
    });
    let out = thread::spawn(move || stdout.map(read).unwrap_or_else(|| Ok((Vec::new(), false))));
    let err = thread::spawn(move || read(stderr));
    let started = Instant::now();
    let status = loop {
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
            break Err(anyhow::anyhow!("Subprocess cancelled"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                break Err(anyhow::anyhow!(
                    "Preparation subprocess exceeded its deadline"
                ));
            }
            Err(error) => break Err(error.into()),
        }
    };
    // Also stop descendants left by a successfully completed parent.
    child.stop();
    let (stdout, out_overflow) = out
        .join()
        .map_err(|_| anyhow::anyhow!("Subprocess output reader failed"))??;
    let (stderr, err_overflow) = err
        .join()
        .map_err(|_| anyhow::anyhow!("Subprocess error reader failed"))??;
    let status = status?;
    if let Some(writer) = writer {
        let written = writer
            .join()
            .map_err(|_| anyhow::anyhow!("Subprocess input writer failed"))?;
        if status.success() {
            written?;
        }
    }
    ensure!(
        !out_overflow && !err_overflow,
        "Preparation subprocess output exceeded its size limit"
    );
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
