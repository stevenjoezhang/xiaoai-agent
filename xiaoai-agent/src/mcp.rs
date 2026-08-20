use std::sync::Arc;
use std::time::Duration;

use rig_core::tool::rmcp::{McpClientError, McpClientHandler};
use rig_core::tool::server::ToolServerHandle;
use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::{AppConfig, HomeAssistantMcpConfig};
use crate::mcp_legacy_sse::LegacySseClientTransport;

const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

type HomeAssistantService = rmcp::service::RunningService<rmcp::RoleClient, McpClientHandler>;

pub struct McpConnections {
    home_assistant_supervisor: Option<JoinHandle<()>>,
}

impl McpConnections {
    pub async fn connect(config: Arc<AppConfig>, tool_server: ToolServerHandle) -> Self {
        let ha = &config.mcp.home_assistant;
        if !ha.enabled {
            return Self {
                home_assistant_supervisor: None,
            };
        }

        let service = match connect_home_assistant(ha, tool_server.clone()).await {
            Ok(service) => {
                info!("connected Home Assistant MCP tools");
                Some(service)
            }
            Err(err) => {
                warn!("failed to connect Home Assistant MCP: {err}");
                None
            }
        };

        let ha = ha.clone();
        let supervisor = tokio::spawn(async move {
            supervise_home_assistant(ha, tool_server, service).await;
        });

        Self {
            home_assistant_supervisor: Some(supervisor),
        }
    }
}

impl Drop for McpConnections {
    fn drop(&mut self) {
        if let Some(supervisor) = self.home_assistant_supervisor.take() {
            supervisor.abort();
        }
    }
}

async fn connect_home_assistant(
    ha: &HomeAssistantMcpConfig,
    tool_server: ToolServerHandle,
) -> Result<HomeAssistantService, McpClientError> {
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("xiaoai-agent", env!("CARGO_PKG_VERSION")),
    );
    let handler = McpClientHandler::new(client_info, tool_server);

    if is_legacy_sse_url(&ha.url) {
        info!("connecting Home Assistant MCP over legacy SSE");
        let transport = LegacySseClientTransport::new(
            ha.url.clone(),
            ha.token.clone(),
            crate::config::timeout_duration(ha.timeout_s),
        );
        handler.connect(transport).await
    } else {
        info!("connecting Home Assistant MCP over streamable HTTP");
        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(ha.url.clone());
        if !ha.token.trim().is_empty() {
            transport_config = transport_config.auth_header(ha.token.clone());
        }
        let transport = StreamableHttpClientTransport::from_config(transport_config);
        handler.connect(transport).await
    }
}

async fn supervise_home_assistant(
    ha: HomeAssistantMcpConfig,
    tool_server: ToolServerHandle,
    mut service: Option<HomeAssistantService>,
) {
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;

    loop {
        if let Some(active_service) = service.take() {
            let outcome = active_service.waiting().await;
            warn!(?outcome, "Home Assistant MCP connection closed");
            reconnect_delay = INITIAL_RECONNECT_DELAY;
        }

        info!(
            reconnect_in_s = reconnect_delay.as_secs(),
            "waiting to reconnect Home Assistant MCP"
        );
        sleep(reconnect_delay).await;

        match connect_home_assistant(&ha, tool_server.clone()).await {
            Ok(new_service) => {
                info!("reconnected Home Assistant MCP tools");
                service = Some(new_service);
                reconnect_delay = INITIAL_RECONNECT_DELAY;
            }
            Err(err) => {
                reconnect_delay = next_reconnect_delay(reconnect_delay);
                warn!(
                    retry_in_s = reconnect_delay.as_secs(),
                    "failed to reconnect Home Assistant MCP: {err}"
                );
            }
        }
    }
}

fn next_reconnect_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RECONNECT_DELAY)
}

fn is_legacy_sse_url(url: &str) -> bool {
    url.trim_end_matches('/').ends_with("/sse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_delay_is_exponential_and_capped() {
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(32)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn detects_legacy_sse_urls() {
        assert!(is_legacy_sse_url(
            "http://homeassistant.local/mcp_server/sse"
        ));
        assert!(is_legacy_sse_url(
            "http://homeassistant.local/mcp_server/sse/"
        ));
        assert!(!is_legacy_sse_url(
            "http://homeassistant.local/mcp_server/mcp"
        ));
    }
}
