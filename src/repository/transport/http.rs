//! Cancellable HTTP reads on the blocking Git worker. The session owns the runtime.
use bytes::Bytes;
use gix_transport::client::blocking_io::http::{self, Http};
use std::{
    io::{self, BufRead, Cursor, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Engine {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    cancel: Arc<AtomicBool>,
    idle: Duration,
}

impl Engine {
    fn run<T>(&self, future: impl Future<Output = Result<T, reqwest::Error>>) -> io::Result<T> {
        self.runtime.block_on(async {
            let cancelled = async {
                loop {
                    if self.cancel.load(Ordering::Acquire) { break; }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            tokio::select! {
                biased;
                _ = cancelled => Err(io::Error::other(
                    anyhow::Error::new(crate::diagnostics::Cancelled).context("Git HTTP request cancelled")
                )),
                result = tokio::time::timeout(self.idle, future) => match result {
                    Ok(result) => result.map_err(|error| io::Error::new(
                        if error.is_timeout() { io::ErrorKind::TimedOut }
                        else if error.is_connect() { io::ErrorKind::ConnectionAborted }
                        else { io::ErrorKind::Other }, error)),
                    Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "Git HTTP response stalled")),
                },
            }
        })
    }
}

pub(super) struct CancellableHttp {
    engine: Arc<Engine>,
}

impl CancellableHttp {
    pub(super) fn new(cancel: Arc<AtomicBool>) -> anyhow::Result<Self> {
        Self::with_idle_timeout(cancel, Duration::from_secs(30))
    }

    pub(super) fn with_idle_timeout(
        cancel: Arc<AtomicBool>,
        idle: Duration,
    ) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let client = {
            let _entered = runtime.enter();
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?
        };
        Ok(Self {
            engine: Arc::new(Engine {
                runtime,
                client,
                cancel,
                idle,
            }),
        })
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        body: Option<Arc<Mutex<Vec<u8>>>>,
    ) -> Result<(Reader, Reader), http::Error> {
        let mut request = self.engine.client.request(method, url);
        for header in headers {
            let (name, value) =
                header
                    .as_ref()
                    .split_once(':')
                    .ok_or_else(|| http::Error::Detail {
                        description: "Invalid Git HTTP header".into(),
                    })?;
            request = request.header(name.trim(), value.trim());
        }
        let exchange = Arc::new(Mutex::new(Exchange {
            engine: self.engine.clone(),
            state: State::Pending(request, body),
        }));
        Ok((
            Reader::new(exchange.clone(), Part::Headers),
            Reader::new(exchange, Part::Body),
        ))
    }
}

enum State {
    Pending(reqwest::RequestBuilder, Option<Arc<Mutex<Vec<u8>>>>),
    Ready(reqwest::Response),
    Failed(io::ErrorKind, String),
    Finished,
}

struct Exchange {
    engine: Arc<Engine>,
    state: State,
}

impl Exchange {
    fn next(&mut self, part: Part) -> io::Result<Bytes> {
        let result = self.read(part);
        if let Err(error) = &result {
            // Discard the response; dropping the session also closes its runtime and sockets.
            self.state = State::Failed(error.kind(), error.to_string());
        }
        result
    }

    fn read(&mut self, part: Part) -> io::Result<Bytes> {
        if matches!(self.state, State::Pending(..)) {
            let State::Pending(mut request, body) =
                std::mem::replace(&mut self.state, State::Finished)
            else {
                unreachable!()
            };
            if let Some(body) = body {
                request = request.body(std::mem::take(&mut *body.lock().unwrap()));
            }
            let response = self.engine.run(request.send())?;
            let status = response.status();
            if !status.is_success() {
                let kind = if status == reqwest::StatusCode::UNAUTHORIZED {
                    io::ErrorKind::PermissionDenied
                } else if status.is_server_error() {
                    io::ErrorKind::ConnectionAborted
                } else {
                    io::ErrorKind::Other
                };
                return Err(io::Error::new(
                    kind,
                    format!("Received HTTP status {status}"),
                ));
            }
            self.state = State::Ready(response);
        }
        match &mut self.state {
            State::Ready(response) => match part {
                Part::Headers => {
                    let mut headers = Vec::new();
                    for (name, value) in response.headers() {
                        headers.extend_from_slice(name.as_str().as_bytes());
                        headers.push(b':');
                        headers.extend_from_slice(value.as_bytes());
                        headers.push(b'\n');
                    }
                    Ok(headers.into())
                }
                Part::Body => match self.engine.run(async {
                    while let Some(bytes) = response.chunk().await? {
                        if !bytes.is_empty() {
                            return Ok(Some(bytes));
                        }
                    }
                    Ok(None)
                })? {
                    Some(bytes) => Ok(bytes),
                    None => {
                        self.state = State::Finished;
                        Ok(Bytes::new())
                    }
                },
                Part::Done => Ok(Bytes::new()),
            },
            State::Failed(kind, message) => Err(io::Error::new(*kind, message.clone())),
            State::Finished => Ok(Bytes::new()),
            State::Pending(..) => unreachable!(),
        }
    }
}

#[derive(Clone, Copy)]
enum Part {
    Headers,
    Body,
    Done,
}

pub(super) struct Reader {
    exchange: Arc<Mutex<Exchange>>,
    part: Part,
    buffer: Cursor<Bytes>,
}
impl Reader {
    fn new(exchange: Arc<Mutex<Exchange>>, part: Part) -> Self {
        Self {
            exchange,
            part,
            buffer: Cursor::new(Bytes::new()),
        }
    }
}
impl BufRead for Reader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.buffer.position() as usize == self.buffer.get_ref().len()
            && !matches!(self.part, Part::Done)
        {
            let bytes = self.exchange.lock().unwrap().next(self.part)?;
            if bytes.is_empty() || matches!(self.part, Part::Headers) {
                self.part = Part::Done;
            }
            self.buffer = Cursor::new(bytes);
        }
        self.buffer.fill_buf()
    }
    fn consume(&mut self, count: usize) {
        self.buffer.consume(count);
    }
}
impl Read for Reader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let bytes = self.fill_buf()?;
        let count = bytes.len().min(output.len());
        output[..count].copy_from_slice(&bytes[..count]);
        self.consume(count);
        Ok(count)
    }
}

pub(super) struct Upload(Arc<Mutex<Vec<u8>>>);
impl Write for Upload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Http for CancellableHttp {
    type Headers = Reader;
    type ResponseBody = Reader;
    type PostBody = Upload;

    fn get(
        &mut self,
        url: &str,
        _: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<http::GetResponse<Reader, Reader>, http::Error> {
        let (headers, body) = self.request(reqwest::Method::GET, url, headers, None)?;
        Ok(http::GetResponse { headers, body })
    }

    fn post(
        &mut self,
        url: &str,
        _: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        body: http::PostBodyDataKind,
    ) -> Result<http::PostResponse<Reader, Reader, Upload>, http::Error> {
        // Upload-pack sends bounded negotiation packets; pack payloads are responses.
        if body != http::PostBodyDataKind::BoundedAndFitsIntoMemory {
            return Err(http::Error::Detail {
                description: "Unbounded Git uploads are unsupported".into(),
            });
        }
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let (headers, body) =
            self.request(reqwest::Method::POST, url, headers, Some(buffer.clone()))?;
        Ok(http::PostResponse {
            headers,
            body,
            post_body: Upload(buffer),
        })
    }

    fn configure(
        &mut self,
        _: &dyn std::any::Any,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("Git HTTP options are set when constructing the transport".into())
    }
}
