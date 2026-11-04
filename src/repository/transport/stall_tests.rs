//! Deterministic HTTP stalls used to exercise cancellation during Git transport.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum Stall {
    Headers,
    Body,
    PacedBody,
}

struct Server {
    address: SocketAddr,
    arrival: Receiver<String>,
    disconnected: Receiver<()>,
    progress: Receiver<()>,
    release: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

struct Worker {
    cancel: Arc<AtomicBool>,
    release: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            thread.join().expect("HTTP request thread panicked");
        }
    }
}

impl Server {
    fn start(stall: Stall) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (arrived, arrival) = mpsc::channel();
        let (closed, disconnected) = mpsc::channel();
        let (advanced, progress) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut connection = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if released.try_recv().is_ok() {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("Cannot accept fixture connection: {error}"),
                }
            };
            connection
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                match connection.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(length) => {
                        request.extend_from_slice(&buffer[..length]);
                        assert!(
                            request.len() < 64 * 1024,
                            "Fixture request headers too large"
                        );
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        if released.try_recv().is_ok() {
                            return;
                        }
                    }
                    Err(error) => panic!("Cannot read fixture request: {error}"),
                }
            }
            if matches!(stall, Stall::Body | Stall::PacedBody) {
                let length = if matches!(stall, Stall::PacedBody) {
                    16
                } else {
                    1048576
                };
                connection
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-result\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n000dpackfile\n").as_bytes())
                    .unwrap();
                connection.flush().unwrap();
            }
            let _ = arrived.send(String::from_utf8_lossy(&request).into_owned());
            let mut sent = 0;
            loop {
                if released.try_recv().is_ok() {
                    if matches!(stall, Stall::PacedBody) {
                        if connection.write_all(b"x").is_err() {
                            let _ = closed.send(());
                            return;
                        }
                        let _ = advanced.send(());
                        sent += 1;
                        if sent < 3 {
                            continue;
                        }
                    }
                    break;
                }
                match connection.read(&mut [0; 1024]) {
                    Ok(0) => {
                        let _ = closed.send(());
                        return;
                    }
                    Ok(_) => (),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                        let _ = closed.send(());
                        return;
                    }
                    Err(error) => panic!("Cannot read fixture connection: {error}"),
                }
            }
        });
        Self {
            address,
            arrival,
            disconnected,
            progress,
            release,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn arrived(&self) -> String {
        self.arrival
            .recv_timeout(WAIT)
            .expect("HTTP fixture was not reached")
    }

    fn release(&self) {
        let _ = self.release.send(());
    }

    fn disconnected(&self) {
        self.disconnected
            .recv_timeout(WAIT)
            .expect("HTTP client did not disconnect");
    }

    fn advance(&self) {
        self.release();
        self.progress
            .recv_timeout(WAIT)
            .expect("HTTP fixture did not send its next chunk");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            thread.join().expect("HTTP fixture thread panicked");
        }
    }
}

#[test]
fn fixture_signals_request_and_releases_each_stall() {
    for stall in [Stall::Headers, Stall::Body, Stall::PacedBody] {
        let server = Server::start(stall);
        assert!(server.url("/repo").ends_with("/repo"));
        let mut client = TcpStream::connect(server.address).unwrap();
        client
            .write_all(
                b"POST /repo/git-upload-pack HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
        assert!(server.arrived().starts_with("POST /repo/git-upload-pack "));
        client.set_read_timeout(Some(WAIT)).unwrap();
        if matches!(stall, Stall::Headers) {
            drop(client);
            server.disconnected();
        } else {
            let mut prefix = [0; 128];
            let read = client.read(&mut prefix).unwrap();
            assert!(prefix[..read].starts_with(b"HTTP/1.1 200 OK\r\n"));
            if matches!(stall, Stall::PacedBody) {
                for _ in 0..3 {
                    server.advance();
                }
            } else {
                server.release();
            }
            let mut rest = Vec::new();
            client.read_to_end(&mut rest).unwrap();
            if matches!(stall, Stall::PacedBody) {
                assert!(rest.ends_with(b"xxx"));
            }
        }
    }
}

#[test]
fn cancellation_closes_http_during_headers_and_upload_pack_body() {
    use super::http::CancellableHttp;
    use gix_transport::client::blocking_io::http::{Http, PostBodyDataKind};

    for stall in [Stall::Headers, Stall::Body] {
        let server = Server::start(stall);
        let path = match stall {
            Stall::Headers => "/repo/info/refs?service=git-upload-pack",
            Stall::Body => "/repo/git-upload-pack",
            Stall::PacedBody => unreachable!(),
        };
        let url = server.url(path);
        let base = server.url("/repo");
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (ready, body_ready) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let thread = thread::spawn(move || {
            let outcome = (|| -> anyhow::Result<()> {
                let mut http = CancellableHttp::new(worker_cancel)?;
                let (mut headers, mut body) = match stall {
                    Stall::Headers => {
                        let response = http.get(&url, &base, std::iter::empty::<&str>())?;
                        (response.headers, response.body)
                    }
                    Stall::Body => {
                        let mut response = http.post(
                            &url,
                            &base,
                            std::iter::empty::<&str>(),
                            PostBodyDataKind::BoundedAndFitsIntoMemory,
                        )?;
                        response.post_body.write_all(b"0000")?;
                        drop(response.post_body);
                        (response.headers, response.body)
                    }
                    Stall::PacedBody => unreachable!(),
                };
                headers.read_to_end(&mut Vec::new())?;
                if matches!(stall, Stall::Body) {
                    let mut prefix = [0; 13];
                    body.read_exact(&mut prefix)?;
                    ready.send(())?;
                    body.read_exact(&mut [0; 1])?;
                }
                Ok(())
            })();
            let _ = finished.send(outcome);
        });
        let worker = Worker {
            cancel: cancel.clone(),
            release: server.release.clone(),
            thread: Some(thread),
        };
        let request = server.arrived();
        let reached_body = !matches!(stall, Stall::Body) || body_ready.recv_timeout(WAIT).is_ok();
        cancel.store(true, Ordering::Release);
        let outcome = result.recv_timeout(WAIT);
        let closed = if matches!(&outcome, Ok(Err(_))) {
            server.disconnected.recv_timeout(WAIT).is_ok()
        } else {
            false
        };
        server.release();
        drop(worker);
        drop(server);
        assert!(
            request.starts_with(if matches!(stall, Stall::Headers) {
                "GET /repo/info/refs?service=git-upload-pack "
            } else {
                "POST /repo/git-upload-pack "
            }),
            "Unexpected request: {request}"
        );
        assert!(
            reached_body,
            "Client did not begin reading the response body"
        );
        let error = outcome
            .expect("Cancellation did not stop the request")
            .expect_err("Cancellation did not stop the request");
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::diagnostics::Cancelled>()),
            "Cancellation lost its typed cause: {error:?}; I/O inner: {:?}",
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                .and_then(std::io::Error::get_ref)
        );
        assert!(closed, "Cancellation did not close the connection");
    }
}

#[test]
fn idle_timeout_closes_the_socket_when_session_is_dropped() {
    use super::http::CancellableHttp;
    use gix_transport::client::blocking_io::http::Http;
    use std::io::ErrorKind;

    for stall in [Stall::Headers, Stall::Body] {
        let server = Server::start(stall);
        let url = server.url("/repo/info/refs?service=git-upload-pack");
        let base = server.url("/repo");
        let (ready, body_ready) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut http = CancellableHttp::with_idle_timeout(
                Arc::new(AtomicBool::new(false)),
                Duration::from_millis(200),
            )
            .unwrap();
            let response = http.get(&url, &base, std::iter::empty::<&str>()).unwrap();
            let (mut headers, mut body) = (response.headers, response.body);
            let outcome = (|| -> std::io::Result<()> {
                headers.read_to_end(&mut Vec::new())?;
                if matches!(stall, Stall::Body) {
                    body.read_exact(&mut [0; 13])?;
                    ready.send(()).unwrap();
                    body.read_exact(&mut [0; 1])?;
                }
                Ok(())
            })();
            drop((headers, body, http));
            let _ = finished.send(outcome.map_err(|error| error.kind()));
        });
        server.arrived();
        let reached_body = !matches!(stall, Stall::Body) || body_ready.recv_timeout(WAIT).is_ok();
        let outcome = result.recv_timeout(WAIT);
        let disconnected = server.disconnected.recv_timeout(WAIT).is_ok();
        server.release();
        thread.join().unwrap();
        drop(server);
        assert!(
            reached_body,
            "Client did not begin reading the response body"
        );
        assert_eq!(outcome.unwrap(), Err(ErrorKind::TimedOut));
        assert!(
            disconnected,
            "Timed out socket remained open after session drop"
        );
    }
}

#[test]
fn paced_http_body_can_run_longer_than_the_idle_timeout() {
    use super::http::CancellableHttp;
    use gix_transport::client::blocking_io::http::Http;

    let server = Server::start(Stall::PacedBody);
    let url = server.url("/repo/info/refs?service=git-upload-pack");
    let base = server.url("/repo");
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let (ready, body_ready) = mpsc::channel();
    let (finished, result) = mpsc::channel();
    let thread = thread::spawn(move || {
        let outcome = (|| -> anyhow::Result<Vec<u8>> {
            let mut http =
                CancellableHttp::with_idle_timeout(worker_cancel, Duration::from_millis(900))?;
            let mut response = http.get(&url, &base, std::iter::empty::<&str>())?;
            response.headers.read_to_end(&mut Vec::new())?;
            let mut output = vec![0; 13];
            response.body.read_exact(&mut output)?;
            ready.send(())?;
            response.body.read_to_end(&mut output)?;
            Ok(output)
        })();
        let _ = finished.send(outcome);
    });
    let worker = Worker {
        cancel: cancel.clone(),
        release: server.release.clone(),
        thread: Some(thread),
    };
    let request = server.arrived();
    let reached_body = body_ready.recv_timeout(WAIT).is_ok();
    let mut sustained = reached_body;
    for _ in 0..3 {
        if !matches!(
            result.recv_timeout(Duration::from_millis(400)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            sustained = false;
            break;
        }
        server.advance();
    }
    if !sustained {
        cancel.store(true, Ordering::Release);
    }
    let outcome = result.recv_timeout(WAIT);
    server.release();
    drop(worker);
    drop(server);
    assert!(request.starts_with("GET /repo/info/refs?service=git-upload-pack "));
    assert!(
        reached_body,
        "Client did not begin reading the response body"
    );
    assert!(
        sustained,
        "Progressing response timed out before completion"
    );
    let output = outcome.expect("HTTP request did not finish").unwrap();
    assert_eq!(output, b"000dpackfile\nxxx");
}

#[test]
#[ignore = "measures the old HTTP backend's full timeout"]
fn cancellation_tail_probe() {
    use super::http::CancellableHttp;
    use gix_transport::client::blocking_io::http::{Http, PostBodyDataKind, reqwest::Remote};
    use std::time::Instant;

    fn request<H: Http>(
        mut http: H,
        stall: Stall,
        url: &str,
        base: &str,
        ready: Sender<()>,
    ) -> anyhow::Result<()> {
        let (mut headers, mut body) = match stall {
            Stall::Headers => {
                let response = http.get(url, base, std::iter::empty::<&str>())?;
                (response.headers, response.body)
            }
            Stall::Body => {
                let mut response = http.post(
                    url,
                    base,
                    std::iter::empty::<&str>(),
                    PostBodyDataKind::BoundedAndFitsIntoMemory,
                )?;
                response.post_body.write_all(b"0000")?;
                drop(response.post_body);
                (response.headers, response.body)
            }
            Stall::PacedBody => unreachable!(),
        };
        headers.read_to_end(&mut Vec::new())?;
        if matches!(stall, Stall::Body) {
            body.read_exact(&mut [0; 13])?;
            ready.send(())?;
            body.read_exact(&mut [0; 1])?;
        }
        Ok(())
    }

    println!("backend,phase,tail_ms,error,error_detail");
    for (backend, samples) in [("old", 1), ("new", 5)] {
        for stall in [Stall::Headers, Stall::Body] {
            for _ in 0..samples {
                let server = Server::start(stall);
                let path = if matches!(stall, Stall::Headers) {
                    "/repo/info/refs?service=git-upload-pack"
                } else {
                    "/repo/git-upload-pack"
                };
                let url = server.url(path);
                let base = server.url("/repo");
                let cancel = Arc::new(AtomicBool::new(false));
                let worker_cancel = cancel.clone();
                let (ready, body_ready) = mpsc::channel();
                let (finished, result) = mpsc::channel();
                let thread = thread::spawn(move || {
                    let outcome = if backend == "old" {
                        request(Remote::default(), stall, &url, &base, ready)
                    } else {
                        CancellableHttp::new(worker_cancel)
                            .and_then(|http| request(http, stall, &url, &base, ready))
                    };
                    let _ = finished.send(outcome);
                });
                let worker = Worker {
                    cancel: cancel.clone(),
                    release: server.release.clone(),
                    thread: Some(thread),
                };
                server.arrived();
                if matches!(stall, Stall::Body) {
                    body_ready.recv_timeout(WAIT).expect("Body was not reached");
                }
                let started = Instant::now();
                cancel.store(true, Ordering::Release);
                let outcome = result.recv_timeout(Duration::from_secs(65));
                let tail = started.elapsed();
                if outcome.is_err() {
                    server.release();
                }
                drop(worker);
                drop(server);
                let error = outcome
                    .expect("HTTP request exceeded probe deadline")
                    .unwrap_err();
                let category = if error
                    .chain()
                    .any(|cause| cause.is::<crate::diagnostics::Cancelled>())
                {
                    "cancelled"
                } else if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(|error| error.is_timeout())
                        || cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
                }) {
                    "timeout"
                } else {
                    "other"
                };
                println!(
                    "{backend},{},{},{category},\"{}\"",
                    if matches!(stall, Stall::Headers) {
                        "headers"
                    } else {
                        "body"
                    },
                    tail.as_millis(),
                    format!("{error:#}").replace('"', "\"\"")
                );
            }
        }
    }
}
