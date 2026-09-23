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
    ensure!(
        !STOPPED.load(std::sync::atomic::Ordering::Acquire),
        "Service is shutting down"
    );
    use std::os::unix::process::CommandExt;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(destination.map(Stdio::from).unwrap_or_else(Stdio::piped))
        .stderr(Stdio::piped());
    let owns_group = OWNS_GROUP.load(std::sync::atomic::Ordering::Relaxed);
    if owns_group {
        command.process_group(0);
    }
    let mut child = command.spawn().with_context(|| {
        format!(
            "Cannot start required subprocess `{}`",
            command.get_program().to_string_lossy()
        )
    })?;
    let pid = child.id();
    if owns_group {
        let mut children = CHILDREN.lock().unwrap();
        children.insert(pid as i32);
        if STOPPED.load(std::sync::atomic::Ordering::Acquire) {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
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
    // Each job starts a private process group. Also stop descendants left by a
    // successfully completed parent before importing generated files.
    unsafe {
        libc::kill(
            if owns_group {
                -(pid as i32)
            } else {
                pid as i32
            },
            libc::SIGKILL,
        );
    }
    let _ = child.wait();
    if owns_group {
        CHILDREN.lock().unwrap().remove(&(pid as i32));
    }
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
