use std::sync::Arc;

use rig_core::tool::rmcp::McpClientHandler;
use rig_core::tool::server::ToolServerHandle;
use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::mcp_legacy_sse::LegacySseClientTransport;

pub struct McpConnections {
    _home_assistant: Option<rmcp::service::RunningService<rmcp::RoleClient, McpClientHandler>>,
}

impl McpConnections {
    pub async fn connect(config: Arc<AppConfig>, tool_server: ToolServerHandle) -> Self {
        let ha = &config.mcp.home_assistant;
        if !ha.enabled {
            return Self {
                _home_assistant: None,
            };
        }

        let client_info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("dodo-xiaoai-agent", env!("CARGO_PKG_VERSION")),
        );
        let handler = McpClientHandler::new(client_info, tool_server);
        let service = if is_legacy_sse_url(&ha.url) {
            info!("connecting Home Assistant MCP over legacy SSE");
            let transport = LegacySseClientTransport::new(
                ha.url.clone(),
                ha.token.clone(),
                crate::config::timeout_duration(ha.timeout_s),
            );
            handler.connect(transport).await
        } else {
            info!("connecting Home Assistant MCP over streamable HTTP");
            let mut transport_config =
                StreamableHttpClientTransportConfig::with_uri(ha.url.clone());
            if !ha.token.trim().is_empty() {
                transport_config = transport_config.auth_header(ha.token.clone());
            }
            let transport = StreamableHttpClientTransport::from_config(transport_config);
            handler.connect(transport).await
        };

        match service {
            Ok(service) => {
                info!("connected Home Assistant MCP tools");
                Self {
                    _home_assistant: Some(service),
                }
            }
            Err(err) => {
                warn!("failed to connect Home Assistant MCP: {err}");
                Self {
                    _home_assistant: None,
                }
            }
        }
    }
}

fn is_legacy_sse_url(url: &str) -> bool {
    url.trim_end_matches('/').ends_with("/sse")
}
