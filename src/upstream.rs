use anyhow::{Context, Result, ensure};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientRequest,
        ServerResult,
    },
    service::{Peer, PeerRequestOptions, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub(crate) fn validate_endpoint(endpoint: &str) -> Result<()> {
    let url = url::Url::parse(endpoint).context("Invalid upstream URL")?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    ensure!(
        url.host().is_some() && (url.scheme() == "https" || url.scheme() == "http" && loopback),
        "Upstream requires HTTPS, or HTTP on loopback"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "Upstream URL must not contain credentials or a fragment"
    );
    Ok(())
}

type Connection = RunningService<RoleClient, ()>;

pub(crate) struct Upstream {
    client: reqwest::Client,
    config: StreamableHttpClientTransportConfig,
    connection: Mutex<Option<Arc<Connection>>>,
}

impl Upstream {
    /// Reuse transport configuration, never another downstream session's state.
    pub fn session(&self) -> Self {
        Self {
            client: self.client.clone(),
            config: self.config.clone(),
            connection: Mutex::new(None),
        }
    }
    pub fn new(endpoint: &str, token_file: Option<&Path>) -> Result<Self> {
        validate_endpoint(endpoint)?;
        let token = token_file
            .map(std::fs::read_to_string)
            .transpose()
            .context("Cannot read upstream token file")?;
        let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned())
            .reinit_on_expired_session(false);
        if let Some(token) = token {
            let token = token.trim();
            ensure!(!token.is_empty(), "Upstream token file is empty");
            config = config.auth_header(token);
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .build()?,
            config,
            connection: Mutex::new(None),
        })
    }

    async fn connect(&self) -> Result<Arc<Connection>> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(connection.clone());
        }
        let transport =
            StreamableHttpClientTransport::with_client(self.client.clone(), self.config.clone());
        let connection = tokio::time::timeout(Duration::from_secs(30), ().serve(transport))
            .await
            .context("Upstream connection timed out")?
            .map_err(|_| {
                anyhow::anyhow!(
                    "Cannot connect to upstream MCP server; check endpoint, authentication, and TLS"
                )
            })?;
        let connection = Arc::new(connection);
        *slot = Some(connection.clone());
        Ok(connection)
    }

    pub async fn call(
        &self,
        name: &'static str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<CallToolResult> {
        let connection = self.connect().await?;
        let request = ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
            CallToolRequestParams::new(name).with_arguments(arguments),
        ));
        let result = async {
            let handle = connection
                .send_cancellable_request(request, PeerRequestOptions::no_options())
                .await?;
            let mut cancel = CancelOnDrop(Some((handle.peer.clone(), handle.id.clone())));
            let result = handle.await_response().await;
            cancel.0 = None;
            match result? {
                ServerResult::CallToolResult(result) => Ok(result),
                _ => Err(ServiceError::UnexpectedResponse),
            }
        }
        .await;
        match result {
            Ok(result) => Ok(result),
            Err(ServiceError::McpError(error)) => {
                Err(anyhow::anyhow!("Upstream MCP error: {}", error.message))
            }
            Err(_) => {
                let mut slot = self.connection.lock().await;
                if slot
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &connection))
                {
                    slot.take();
                }
                anyhow::bail!("Upstream MCP request failed; retry to reconnect")
            }
        }
    }

    pub async fn shutdown(&self) {
        if let Some(connection) = self.connection.lock().await.take() {
            connection.cancellation_token().cancel();
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.get_mut().take() {
            connection.cancellation_token().cancel();
        }
    }
}

// Dropping a downstream request must cancel only its corresponding upstream call.
struct CancelOnDrop(Option<(Peer<RoleClient>, rmcp::model::RequestId)>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some((peer, id)) = self.0.take() {
            tokio::spawn(async move {
                let _ = peer
                    .notify_cancelled(CancelledNotificationParam::new(Some(id), None))
                    .await;
            });
        }
    }
}
