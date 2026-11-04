//! Exercise the HTTP adapter against Git's real smart HTTP protocol.
use super::{Fetch, Filter, Session, http::CancellableHttp};
use anyhow::{Context, Result, ensure};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn run(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "Git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct Server {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<Vec<Request>>>>,
}

impl Server {
    fn start(root: PathBuf) -> Result<Self> {
        Self::start_inner(root, None)
    }

    fn start_stalled_post(root: PathBuf) -> Result<(Self, Receiver<()>)> {
        let (arrived, receiver) = mpsc::channel();
        Ok((Self::start_inner(root, Some(arrived))?, receiver))
    }

    fn start_inner(root: PathBuf, stalled_post: Option<Sender<()>>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((connection, _)) => {
                        requests.push(serve(connection, &root, stalled_post.as_ref())?)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(requests)
        });
        Ok(Self {
            address,
            stop,
            thread: Some(thread),
        })
    }

    fn finish(mut self) -> Result<Vec<Request>> {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("HTTP fixture panicked")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    mut connection: TcpStream,
    root: &Path,
    stalled_post: Option<&Sender<()>>,
) -> Result<Request> {
    connection.set_read_timeout(Some(Duration::from_secs(3)))?;
    connection.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut reader = BufReader::new(connection.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().context("Missing request method")?.to_owned();
    let target = parts.next().context("Missing request target")?.to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let mut length = 0;
    let mut content_type = String::new();
    let mut protocol = String::new();
    loop {
        line.clear();
        ensure!(
            reader.read_line(&mut line)? > 0,
            "Incomplete request headers"
        );
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').context("Invalid request header")?;
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse()?;
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = value.trim().to_owned();
        } else if name.eq_ignore_ascii_case("git-protocol") {
            protocol = value.trim().to_owned();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    if method == "POST"
        && let Some(arrived) = stalled_post
    {
        arrived.send(())?;
        let mut byte = [0; 1];
        ensure!(
            reader.read(&mut byte)? == 0,
            "Stalled HTTP request remained open"
        );
        return Ok(Request {
            method,
            path: target,
            body,
        });
    }
    let mut backend = git();
    backend
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("REQUEST_METHOD", &method)
        .env("CONTENT_TYPE", &content_type)
        .env("CONTENT_LENGTH", length.to_string())
        .env("HTTP_GIT_PROTOCOL", protocol)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = backend.spawn()?;
    child.stdin.take().unwrap().write_all(&body)?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "Git HTTP backend failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let boundary = output
        .stdout
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .context("Missing CGI response headers")?;
    let headers = std::str::from_utf8(&output.stdout[..boundary])?;
    let content_type = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-type")
                .then(|| value.trim())
        })
        .context("Missing CGI content type")?;
    ensure!(
        !headers
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("status:")),
        "Git HTTP backend returned an error: {headers}"
    );
    let response = &output.stdout[boundary + 4..];
    write!(
        connection,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    connection.write_all(response)?;
    Ok(Request {
        method,
        path: target,
        body,
    })
}

#[test]
fn cancellation_during_ref_lookup_preserves_the_typed_cause() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    run(git()
        .args(["init", "--bare", "--template="])
        .arg(temporary.path().join("remote.git")))?;
    let (server, arrived) = Server::start_stalled_post(temporary.path().to_owned())?;
    let url = format!("http://{}/remote.git", server.address);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let thread = thread::spawn(move || -> Result<()> {
        let transport = gix_transport::client::blocking_io::http::connect_http(
            CancellableHttp::new(worker_cancel)?,
            gix_url::Url::try_from(url.as_str())?,
            gix_transport::Protocol::V2,
            false,
        );
        let mut session = Session::handshake(Box::new(transport), false)?;
        session.refs(&["refs/heads/".into()])?;
        Ok(())
    });
    let reached = arrived.recv_timeout(Duration::from_secs(5));
    cancel.store(true, Ordering::Release);
    let outcome = thread.join().expect("HTTP fixture worker panicked");
    let requests = server.finish()?;
    reached.context("Reference lookup did not reach the server")?;
    let error = outcome.expect_err("Reference lookup unexpectedly completed");
    assert!(
        error
            .chain()
            .any(|cause| cause.is::<crate::diagnostics::Cancelled>()),
        "Cancellation lost its typed cause: {error:?}"
    );
    assert!(requests.iter().any(|request| request.method == "POST"));
    Ok(())
}

#[test]
fn fetch_filtered_pack_over_smart_http() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let work = temporary.path().join("work");
    let remote = temporary.path().join("remote.git");
    let destination = temporary.path().join("destination.git");
    run(git()
        .args(["init", "--initial-branch=main", "--template="])
        .arg(&work))?;
    std::fs::write(
        work.join("source.txt"),
        "content excluded from filtered pack\n",
    )?;
    run(git().arg("-C").arg(&work).args(["add", "source.txt"]))?;
    run(git().arg("-C").arg(&work).args([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-m",
        "initial",
    ]))?;
    let commit = run(git().arg("-C").arg(&work).args(["rev-parse", "HEAD"]))?;
    let blob = run(git()
        .arg("-C")
        .arg(&work)
        .args(["rev-parse", "HEAD:source.txt"]))?;
    run(git()
        .args(["clone", "--bare", "--no-local"])
        .arg(&work)
        .arg(&remote))?;
    run(git()
        .arg("--git-dir")
        .arg(&remote)
        .args(["config", "uploadpack.allowFilter", "true"]))?;
    run(git()
        .args(["init", "--bare", "--template="])
        .arg(&destination))?;

    let server = Server::start(temporary.path().to_owned())?;
    let cancel = Arc::new(AtomicBool::new(false));
    let url = format!("http://{}/remote.git", server.address);
    let transport = gix_transport::client::blocking_io::http::connect_http(
        CancellableHttp::new(cancel.clone())?,
        gix_url::Url::try_from(url.as_str())?,
        gix_transport::Protocol::V2,
        false,
    );
    let mut session = Session::handshake(Box::new(transport), false)?;
    assert!(session.can_fetch_again());
    let missing = super::super::revision::resolve(
        &mut session,
        &super::super::materialize::Target::Branch("missing-branch".into()),
        |_, _| unreachable!("A branch lookup does not resolve commit prefixes"),
    );
    assert!(missing.is_err(), "An absent branch unexpectedly resolved");
    let existing = super::super::revision::resolve(
        &mut session,
        &super::super::materialize::Target::Branch("main".into()),
        |_, _| unreachable!("A branch lookup does not resolve commit prefixes"),
    )?;
    let object: gix_hash::ObjectId = commit.parse()?;
    assert_eq!(existing.id, object);
    let refs = session.refs(&["refs/heads/".into()])?;
    assert!(
        refs.iter()
            .any(|reference| reference.unpack().1 == Some(object.as_ref()))
    );
    let mut pack = Vec::new();
    session.pack_filtered(
        vec![object],
        &destination,
        &Fetch {
            filter: Filter::Trees,
            depth: None,
            haves: &[],
            cancel: &cancel,
        },
        &mut pack,
    )?;
    drop(session);
    let requests = server.finish()?;
    assert!(
        requests
            .iter()
            .any(|r| r.method == "GET" && r.path.contains("info/refs"))
    );
    assert!(requests.iter().any(|r| {
        r.method == "POST"
            && r.path.ends_with("git-upload-pack")
            && r.body
                .windows(b"filter blob:none".len())
                .any(|bytes| bytes == b"filter blob:none")
    }));

    let mut index = git()
        .arg("--git-dir")
        .arg(&destination)
        .args(["index-pack", "--stdin", "--fix-thin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    index.stdin.take().unwrap().write_all(&pack)?;
    let indexed = index.wait_with_output()?;
    ensure!(
        indexed.status.success(),
        "Cannot index received pack: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    run(git().arg("--git-dir").arg(&destination).args([
        "cat-file",
        "-e",
        &format!("{commit}^{{tree}}"),
    ]))?;
    assert!(
        !git()
            .arg("--git-dir")
            .arg(&destination)
            .args(["cat-file", "-e", &blob])
            .status()?
            .success(),
        "Filtered pack unexpectedly contains blob"
    );
    Ok(())
}
