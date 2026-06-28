use futures::StreamExt;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::service::RoleClient;
use rmcp::transport::worker::{Worker, WorkerConfig, WorkerContext, WorkerQuitReason};
use sse_stream::SseStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum LegacySseTransportError {
    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("SSE stream error: {0}")]
    Sse(#[from] sse_stream::Error),
    #[error("invalid SSE endpoint URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("legacy SSE endpoint event was not received")]
    MissingEndpoint,
    #[error("legacy SSE endpoint closed")]
    Closed,
    #[error("legacy SSE message parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("worker join error: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Clone)]
pub struct LegacySseClientTransport {
    url: String,
    token: String,
    post_timeout: std::time::Duration,
    client: reqwest::Client,
}

impl LegacySseClientTransport {
    pub fn new(url: String, token: String, post_timeout: std::time::Duration) -> Self {
        Self {
            url,
            token,
            post_timeout,
            client: reqwest::Client::new(),
        }
    }
}

impl Worker for LegacySseClientTransport {
    type Error = LegacySseTransportError;
    type Role = RoleClient;

    fn err_closed() -> Self::Error {
        LegacySseTransportError::Closed
    }

    fn err_join(e: tokio::task::JoinError) -> Self::Error {
        LegacySseTransportError::Join(e)
    }

    fn config(&self) -> WorkerConfig {
        let mut config = WorkerConfig::default();
        config.name = Some("home-assistant-legacy-sse".to_string());
        config.channel_buffer_capacity = 64;
        config
    }

    async fn run(self, context: WorkerContext<Self>) -> Result<(), WorkerQuitReason<Self::Error>> {
        run_legacy_sse(self, context).await
    }
}

async fn run_legacy_sse(
    transport: LegacySseClientTransport,
    mut context: WorkerContext<LegacySseClientTransport>,
) -> Result<(), WorkerQuitReason<LegacySseTransportError>> {
    let response = request_with_auth(transport.client.get(&transport.url), &transport.token)
        .send()
        .await
        .map_err(|err| WorkerQuitReason::fatal(err.into(), "open legacy SSE stream"))?
        .error_for_status()
        .map_err(|err| WorkerQuitReason::fatal(err.into(), "open legacy SSE stream"))?;

    let mut stream = SseStream::from_byte_stream(response.bytes_stream());
    let endpoint = read_endpoint(&mut stream, &transport.url).await?;
    info!("connected Home Assistant legacy SSE endpoint {endpoint}");

    let (incoming_tx, mut incoming_rx) = mpsc::channel::<ServerJsonRpcMessage>(64);
    let mut stream_task = tokio::spawn(read_messages(stream, incoming_tx));

    loop {
        tokio::select! {
            _ = context.cancellation_token.cancelled() => {
                stream_task.abort();
                return Err(WorkerQuitReason::Cancelled);
            }
            Some(message) = incoming_rx.recv() => {
                context.send_to_handler(message).await?;
            }
            request = context.from_handler_rx.recv() => {
                let Some(request) = request else {
                    stream_task.abort();
                    return Err(WorkerQuitReason::HandlerTerminated);
                };
                let client = transport.client.clone();
                let token = transport.token.clone();
                let endpoint = endpoint.clone();
                let timeout = transport.post_timeout;
                tokio::spawn(async move {
                    let result = post_message(client, token, endpoint, request.message, timeout).await;
                    let _ = request.responder.send(result);
                });
            }
            result = &mut stream_task => {
                return match result {
                    Ok(result) => result,
                    Err(err) => Err(err.into()),
                };
            }
        }
    }
}

async fn read_endpoint<S>(
    stream: &mut S,
    base_url: &str,
) -> Result<String, WorkerQuitReason<LegacySseTransportError>>
where
    S: futures::Stream<Item = Result<sse_stream::Sse, sse_stream::Error>> + Unpin,
{
    while let Some(event) = stream.next().await {
        let event =
            event.map_err(|err| WorkerQuitReason::fatal(err.into(), "read legacy SSE endpoint"))?;
        debug!(?event, "legacy SSE control event");
        if event.event.as_deref() == Some("endpoint") {
            let Some(data) = event.data else {
                return Err(WorkerQuitReason::fatal(
                    LegacySseTransportError::MissingEndpoint,
                    "read legacy SSE endpoint",
                ));
            };
            return absolutize_endpoint(base_url, &data).map_err(|err| {
                WorkerQuitReason::fatal(err, "resolve legacy SSE message endpoint")
            });
        }
        if event.event.as_deref().is_none() && event.data.is_some() {
            warn!("legacy SSE returned message before endpoint; ignoring");
        }
    }
    Err(WorkerQuitReason::fatal(
        LegacySseTransportError::MissingEndpoint,
        "read legacy SSE endpoint",
    ))
}

async fn read_messages<S>(
    mut stream: S,
    incoming_tx: mpsc::Sender<ServerJsonRpcMessage>,
) -> Result<(), WorkerQuitReason<LegacySseTransportError>>
where
    S: futures::Stream<Item = Result<sse_stream::Sse, sse_stream::Error>> + Unpin,
{
    while let Some(event) = stream.next().await {
        let event =
            event.map_err(|err| WorkerQuitReason::fatal(err.into(), "read legacy SSE message"))?;
        if !matches!(event.event.as_deref(), None | Some("") | Some("message")) {
            debug!(?event, "ignored legacy SSE control event");
            continue;
        }
        let Some(data) = event.data else {
            continue;
        };
        if data.trim().is_empty() {
            continue;
        }
        let message = serde_json::from_str::<ServerJsonRpcMessage>(&data)
            .map_err(|err| WorkerQuitReason::fatal(err.into(), "parse legacy SSE message"))?;
        incoming_tx
            .send(message)
            .await
            .map_err(|_| WorkerQuitReason::HandlerTerminated)?;
    }
    Err(WorkerQuitReason::TransportClosed)
}

async fn post_message(
    client: reqwest::Client,
    token: String,
    endpoint: String,
    message: ClientJsonRpcMessage,
    timeout: std::time::Duration,
) -> Result<(), LegacySseTransportError> {
    let response = request_with_auth(client.post(endpoint), &token)
        .timeout(timeout)
        .json(&message)
        .send()
        .await?
        .error_for_status()?;
    if response.status() != reqwest::StatusCode::ACCEPTED {
        debug!(status = %response.status(), "legacy SSE POST returned non-202 success");
    }
    Ok(())
}

fn request_with_auth(builder: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    if token.trim().is_empty() {
        builder
    } else {
        builder.bearer_auth(token)
    }
}

fn absolutize_endpoint(base_url: &str, endpoint: &str) -> Result<String, LegacySseTransportError> {
    let base = Url::parse(base_url)?;
    let resolved = base.join(endpoint)?;
    Ok(resolved.to_string())
}
