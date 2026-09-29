//! Git wire operations. A pack is requested only after the same session advertises filtering.
use anyhow::{Context, Result, ensure};
use gix_protocol::{
    fetch::{self, negotiate},
    handshake::Ref,
};
use gix_transport::IsSpuriousError;
use gix_transport::client::blocking_io::{self, Transport};
use std::{io::Write, path::Path, sync::atomic::AtomicBool, time::Duration};

pub struct Session {
    transport: Box<dyn Transport + Send>,
    handshake: gix_protocol::Handshake,
    pub endpoint: Option<String>,
}

fn agent() -> gix_protocol::command::Feature {
    ("agent", Some("sigla".into()))
}

fn alternatives<T>(
    endpoints: &[String],
    mut connect: impl FnMut(&str) -> Result<T>,
) -> Result<(T, String)> {
    let mut failures = Vec::new();
    for endpoint in endpoints {
        let mut result = connect(endpoint);
        if result.as_ref().is_err_and(transient) {
            std::thread::sleep(Duration::from_millis(250));
            result = connect(endpoint);
        }
        match result {
            Ok(session) => return Ok((session, endpoint.clone())),
            Err(error) => failures.push(format!(
                "{}: {}",
                if endpoint.starts_with("https:") {
                    "HTTPS"
                } else {
                    "SSH"
                },
                failure_reason(&error)
            )),
        }
    }
    anyhow::bail!("{}", failures.join("; "))
}

fn transient(error: &anyhow::Error) -> bool {
    error.chain().any(|error| {
        error
            .downcast_ref::<gix_protocol::handshake::Error>()
            .is_some_and(IsSpuriousError::is_spurious)
            || error
                .downcast_ref::<gix_transport::client::Error>()
                .is_some_and(IsSpuriousError::is_spurious)
            || error
                .downcast_ref::<std::io::Error>()
                .is_some_and(IsSpuriousError::is_spurious)
    })
}

// Do not return raw transport errors: they can contain credential URLs or helper output.
fn failure_reason(error: &anyhow::Error) -> &'static str {
    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("certificate")
        || message.contains("unknownissuer")
        || message.contains("untrustedroot")
    {
        "certificate verification failed"
    } else if message.contains("timed out") || message.contains("timeout") {
        "connection timed out"
    } else if message.contains("resolve") || message.contains("dns") {
        "host lookup failed"
    } else if message.contains("404")
        || message.contains("not found")
        || message.contains("notfound")
    {
        "repository unavailable or authentication required"
    } else if message.contains("401")
        || message.contains("403")
        || message.contains("permission denied")
        || message.contains("authentication")
        || message.contains("credential")
    {
        "authentication failed or access denied"
    } else if message.contains("redirect") {
        "repository redirected to another address"
    } else if message.contains("host key") {
        "SSH host key verification failed"
    } else if transient(error) {
        "temporary connection or server failure"
    } else if message.contains("connection") {
        "connection failed"
    } else {
        "Git protocol negotiation failed"
    }
}

#[cfg(test)]
mod access_tests {
    use super::*;
    #[test]
    fn falls_back_and_retries_only_transient_errors() {
        let endpoints = vec![
            "https://example.invalid/repo".into(),
            "git@example.invalid:repo".into(),
        ];
        let mut calls = Vec::new();
        let (_, selected) = alternatives(&endpoints, |endpoint| {
            calls.push(endpoint.to_owned());
            if endpoint.starts_with("https:") {
                anyhow::bail!("authentication required");
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, endpoints);
        assert_eq!(selected, endpoints[1]);
        let mut attempts = 0;
        alternatives(&endpoints, |_| {
            attempts += 1;
            if attempts == 1 {
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(attempts, 2);
    }
    #[test]
    fn failure_output_identifies_causes_without_echoing_credentials() {
        let error = alternatives::<()>(&["https://example.invalid/repo".into()], |_| {
            anyhow::bail!(
                "certificate failure at https://user:secret@example.invalid/repo?token=secret"
            )
        })
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("certificate"));
        assert!(!text.contains("secret") && !text.contains("user"));
    }
}

impl Session {
    /// Callers authorize the parsed repository before opening a session.
    pub fn connect(repository: &super::Repository, preferred: Option<&str>) -> Result<Self> {
        let (mut session, endpoint) =
            alternatives(&repository.transports(preferred), Self::connect_one)
                .with_context(|| format!("Cannot access {}", repository.identity))?;
        session.endpoint = Some(endpoint);
        Ok(session)
    }

    fn connect_one(endpoint: &str) -> Result<Self> {
        let mut transport = blocking_io::connect::connect(endpoint, blocking_io::connect::Options {
            version: gix_transport::Protocol::V2,
            ssh: blocking_io::ssh::connect::Options {
                command: Some("ssh -oBatchMode=yes -oStrictHostKeyChecking=yes -oConnectTimeout=30 -oServerAliveInterval=15 -oServerAliveCountMax=2".into()),
                ..Default::default()
            },
            trace: false,
        })?;
        if endpoint.starts_with("https://") {
            transport
                .configure(&blocking_io::http::Options {
                    follow_redirects: blocking_io::http::options::FollowRedirects::None,
                    connect_timeout: Some(Duration::from_secs(30)),
                    low_speed_limit_bytes_per_second: 1,
                    low_speed_time_seconds: 30,
                    ..Default::default()
                })
                .map_err(anyhow::Error::from_boxed)?;
        }
        Self::handshake(transport)
    }

    fn handshake(mut transport: Box<dyn Transport + Send>) -> Result<Self> {
        let handshake = gix_protocol::handshake(
            &mut transport,
            gix_transport::Service::UploadPack,
            gix_protocol::credentials::builtin,
            Vec::new(),
            &mut gix_features::progress::Discard,
        )?;
        Ok(Self {
            transport,
            handshake,
            endpoint: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn local(path: &Path) -> Result<Self> {
        Self::handshake(blocking_io::connect::connect(
            path.to_str().unwrap(),
            blocking_io::connect::Options {
                version: gix_transport::Protocol::V2,
                ..Default::default()
            },
        )?)
    }

    pub fn refs(&mut self, prefixes: &[String]) -> Result<Vec<Ref>> {
        if let Some(refs) = &self.handshake.refs {
            return Ok(refs.clone());
        }
        let mut selected = gix_protocol::ls_refs::RefPrefixes::new();
        selected.extend(prefixes.iter().map(|s| s.as_str().into()));
        gix_protocol::LsRefsCommand::new(Some(selected), &self.handshake.capabilities, agent())
            .invoke_blocking(
                &mut self.transport,
                &mut gix_features::progress::Discard,
                false,
            )
            .map_err(|_| anyhow::anyhow!("Repository reference lookup failed"))
    }

    /// Stream a filtered pack to staging. Explicit blob wants are used only by the materializer.
    pub fn pack(
        &mut self,
        objects: Vec<gix_hash::ObjectId>,
        store: &Path,
        depth_one: bool,
        output: &mut impl Write,
    ) -> Result<()> {
        ensure!(!objects.is_empty(), "Cannot request an empty Git pack");
        let mut wants = Wants {
            objects,
            unsupported: false,
        };
        let shallow = if depth_one {
            fetch::Shallow::DepthAtRemote(1.try_into().unwrap())
        } else {
            fetch::Shallow::NoChange
        };
        let result = gix_protocol::fetch(
            &mut wants,
            |reader, _, _| {
                std::io::copy(reader, output)?;
                Ok::<_, std::io::Error>(true)
            },
            gix_features::progress::Discard,
            &AtomicBool::new(false),
            fetch::Context {
                handshake: &mut self.handshake,
                transport: &mut self.transport,
                user_agent: agent(),
                trace_packetlines: false,
            },
            fetch::Options {
                shallow_file: store.join("shallow"),
                shallow: &shallow,
                tags: fetch::Tags::None,
                reject_shallow_remote: false,
            },
        );
        ensure!(
            !wants.unsupported,
            "Repository endpoint does not support filtered acquisition; no pack was requested"
        );
        result.map_err(|_| anyhow::anyhow!("Filtered repository transfer failed"))?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = gix_protocol::indicate_end_of_interaction(&mut self.transport, false);
    }
}

struct Wants {
    objects: Vec<gix_hash::ObjectId>,
    unsupported: bool,
}
impl fetch::Negotiate for Wants {
    fn mark_complete_and_common_ref(&mut self) -> Result<negotiate::Action, negotiate::Error> {
        Ok(negotiate::Action::MustNegotiate {
            remote_ref_target_known: vec![false; self.objects.len()],
        })
    }
    fn add_wants(&mut self, arguments: &mut fetch::Arguments, _: &[bool]) -> bool {
        // This check occurs before arguments.send(), including on every later blob batch.
        if !arguments.can_use_filter() {
            self.unsupported = true;
            return false;
        }
        arguments.filter("blob:none");
        for object in &self.objects {
            arguments.want(object);
        }
        true
    }
    fn one_round(
        &mut self,
        _: &mut negotiate::one_round::State,
        _: &mut fetch::Arguments,
        _: Option<&fetch::Response>,
    ) -> Result<(negotiate::Round, bool), negotiate::Error> {
        Ok((
            negotiate::Round {
                haves_sent: 0,
                in_vain: 0,
                haves_to_send: 0,
                previous_response_had_at_least_one_in_common: false,
            },
            true,
        ))
    }
}
