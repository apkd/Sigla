//! Measure complete remote setup through the same MCP requests as a client.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Options {
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    codebase: String,
    #[arg(long)]
    query: String,
    /// Required text in the complete result, to reject empty or fallback results.
    #[arg(long)]
    expect: String,
    /// New directory for raw responses and timing samples.
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 1800)]
    timeout_seconds: u64,
}

struct Session {
    client: Client,
    endpoint: String,
    id: String,
}

impl Session {
    fn open(endpoint: String, timeout: Duration) -> Result<Self> {
        let client = Client::builder().timeout(timeout).build()?;
        let response = client
            .post(&endpoint)
            .header("Accept", "application/json, text/event-stream")
            .json(
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-03-26","capabilities":{},
                    "clientInfo":{"name":"setup-probe","version":"1"}
                }}),
            )
            .send()?
            .error_for_status()?;
        let id = response
            .headers()
            .get("mcp-session-id")
            .context("Missing MCP session")?
            .to_str()?
            .to_owned();
        let session = Self {
            client,
            endpoint,
            id,
        };
        session.post(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            timeout,
        )?;
        Ok(session)
    }

    fn post(&self, message: &Value, timeout: Duration) -> Result<Response> {
        Ok(self
            .client
            .post(&self.endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &self.id)
            .timeout(timeout)
            .json(message)
            .send()?
            .error_for_status()?)
    }

    fn search(&self, id: u64, codebase: &str, query: &str, deadline: Instant) -> Result<Value> {
        let response = self.post(
            &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":"search","arguments":{"codebase":codebase,"query":query}}
            }),
            remaining(deadline)?,
        )?;
        if !response
            .headers()
            .get("content-type")
            .is_some_and(|header| {
                header
                    .to_str()
                    .unwrap_or("")
                    .starts_with("text/event-stream")
            })
        {
            return Ok(response.json()?);
        }
        let mut data = String::new();
        for line in std::io::BufReader::new(response).lines() {
            let line = line?;
            if let Some(value) = line.strip_prefix("data:") {
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
                data.push('\n');
                continue;
            }
            if !line.is_empty() {
                continue;
            }
            if data.trim().is_empty() {
                data.clear();
                continue;
            }
            let message: Value = serde_json::from_str(&data)?;
            data.clear();
            if message["method"] == "ping" {
                self.post(
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":{}}),
                    remaining(deadline)?,
                )?;
            } else if message["id"] == id && message.get("method").is_none() {
                return Ok(message);
            }
        }
        anyhow::bail!("MCP stream ended before its response")
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "Complete setup exceeded the probe deadline"
    );
    Ok(remaining)
}

fn main() -> Result<()> {
    let options = Options::parse();
    ensure!(
        options.timeout_seconds > 0 && !options.expect.is_empty(),
        "Set a positive timeout and expected result text"
    );
    fs::create_dir(&options.output).context("Output directory must be new")?;
    let mut samples = fs::File::create(options.output.join("samples.jsonl"))?;
    let timeout = Duration::from_secs(options.timeout_seconds);
    let session = Session::open(options.endpoint, timeout)?;
    let started = Instant::now();
    let deadline = started
        .checked_add(timeout)
        .context("Probe timeout is too large")?;
    for attempt in 0.. {
        let query = if attempt == 0
            || options
                .query
                .split_whitespace()
                .any(|part| part == "wait:complete")
        {
            options.query.clone()
        } else {
            format!("{} wait:complete", options.query)
        };
        let request_started = Instant::now();
        let response = session.search(attempt + 2, &options.codebase, &query, deadline)?;
        remaining(deadline)?;
        let request_ms = request_started.elapsed().as_millis();
        let elapsed_ms = started.elapsed().as_millis();
        fs::write(
            options.output.join(format!("response-{attempt}.json")),
            serde_json::to_vec_pretty(&response)?,
        )?;
        let result = response
            .get("result")
            .context("MCP response has no result")?;
        let contents = result["content"]
            .as_array()
            .context("Tool response has no content")?;
        let text = contents
            .iter()
            .filter_map(|content| content["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Sigla marks incomplete results explicitly; complete results omit it.
        let complete = match result
            .get("structuredContent")
            .and_then(|value| value.get("complete"))
        {
            None => true,
            Some(value) => value.as_bool().context("Invalid completeness marker")?,
        };
        let error = result["isError"] == true;
        let sample = json!({"attempt":attempt,"request_ms":request_ms,"elapsed_ms":elapsed_ms,
            "complete":complete,"error":error,"output_hash":blake3::hash(text.as_bytes()).to_hex().to_string()});
        writeln!(samples, "{sample}")?;
        samples.flush()?;
        println!("{sample}");
        ensure!(!error, "Query failed: {text}");
        if complete {
            ensure!(
                text.contains(&options.expect),
                "Complete query did not contain the expected result: {text}"
            );
            break;
        }
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    Ok(())
}
