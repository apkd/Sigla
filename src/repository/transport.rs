//! Git wire operations. A pack is requested only after the same session advertises filtering.
use anyhow::{Result, ensure};
use gix_protocol::{
    fetch::{self, negotiate},
    handshake::Ref,
};
use gix_transport::client::blocking_io::{self, Transport};
use std::{io::Write, path::Path, sync::atomic::AtomicBool, time::Duration};

pub struct Session {
    transport: Box<dyn Transport + Send>,
    handshake: gix_protocol::Handshake,
}

fn agent() -> gix_protocol::command::Feature {
    ("agent", Some("sigla".into()))
}

impl Session {
    /// Callers authorize the parsed repository before opening a session.
    pub fn connect(repository: &super::Repository) -> Result<Self> {
        let mut transport = blocking_io::connect::connect(repository.transport.as_str(), blocking_io::connect::Options {
            version: gix_transport::Protocol::V2,
            ssh: blocking_io::ssh::connect::Options {
                command: Some("ssh -oBatchMode=yes -oStrictHostKeyChecking=yes -oConnectTimeout=30 -oServerAliveInterval=15 -oServerAliveCountMax=2".into()),
                ..Default::default()
            },
            trace: false,
        }).map_err(|_| anyhow::anyhow!("Cannot connect to repository transport"))?;
        if repository.transport.starts_with("https://") {
            transport
                .configure(&blocking_io::http::Options {
                    follow_redirects: blocking_io::http::options::FollowRedirects::None,
                    connect_timeout: Some(Duration::from_secs(30)),
                    low_speed_limit_bytes_per_second: 1,
                    low_speed_time_seconds: 30,
                    ..Default::default()
                })
                .map_err(|_| anyhow::anyhow!("Cannot configure repository transport"))?;
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
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "Repository handshake failed; check access and noninteractive authentication"
            )
        })?;
        Ok(Self {
            transport,
            handshake,
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
