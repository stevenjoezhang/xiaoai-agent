use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rig_core::tool::rmcp::{McpClientError, McpTool};
use rig_core::tool::server::ToolServerHandle;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientCapabilities, ClientInfo, Implementation, Tool};
use rmcp::service::{NotificationContext, RoleClient, RunningService, ServerSink};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport};
use rmcp::ServiceExt;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::{AppConfig, McpServerConfig};
use crate::mcp_legacy_sse::LegacySseClientTransport;

const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

type McpService = RunningService<RoleClient, McpToolHandler>;

pub struct McpConnections {
    supervisors: Vec<JoinHandle<()>>,
}

impl McpConnections {
    pub async fn connect(config: Arc<AppConfig>, tool_server: ToolServerHandle) -> Self {
        let mut supervisors = Vec::new();

        for server in configured_servers(&config) {
            if !server.enabled {
                continue;
            }
            if server.url.trim().is_empty() {
                warn!(server = %server.name, "ignoring MCP server with empty URL");
                continue;
            }

            let service = match connect_mcp_server(&server, tool_server.clone()).await {
                Ok(service) => {
                    info!(server = %server.name, "connected MCP tools");
                    Some(service)
                }
                Err(err) => {
                    warn!(server = %server.name, "failed to connect MCP server: {err}");
                    None
                }
            };

            let supervisor =
                tokio::spawn(supervise_mcp_server(server, tool_server.clone(), service));
            supervisors.push(supervisor);
        }

        Self { supervisors }
    }
}

impl Drop for McpConnections {
    fn drop(&mut self) {
        for supervisor in self.supervisors.drain(..) {
            supervisor.abort();
        }
    }
}

fn configured_servers(config: &AppConfig) -> Vec<McpServerConfig> {
    let mut servers = config.mcp.servers.clone();
    let legacy = &config.mcp.home_assistant;
    if legacy.enabled && !servers.iter().any(|server| server.name == "home_assistant") {
        servers.push(McpServerConfig {
            name: "home_assistant".to_string(),
            enabled: true,
            url: legacy.url.clone(),
            token: legacy.token.clone(),
            timeout_s: legacy.timeout_s,
            tools: Vec::new(),
        });
    }
    servers
}

async fn connect_mcp_server(
    server: &McpServerConfig,
    tool_server: ToolServerHandle,
) -> Result<McpService, McpClientError> {
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("xiaoai-agent", env!("CARGO_PKG_VERSION")),
    );
    let handler = McpToolHandler::new(
        server.name.clone(),
        client_info,
        tool_server,
        &server.tools,
        crate::config::timeout_duration(server.timeout_s),
    );

    if is_legacy_sse_url(&server.url) {
        info!(server = %server.name, "connecting MCP over legacy SSE");
        let transport = LegacySseClientTransport::new(
            server.url.clone(),
            server.token.clone(),
            crate::config::timeout_duration(server.timeout_s),
        );
        handler.connect(transport).await
    } else {
        info!(server = %server.name, "connecting MCP over streamable HTTP");
        let mut transport_config =
            StreamableHttpClientTransportConfig::with_uri(server.url.clone());
        if !server.token.trim().is_empty() {
            transport_config = transport_config.auth_header(server.token.clone());
        }
        let transport = StreamableHttpClientTransport::from_config(transport_config);
        handler.connect(transport).await
    }
}

async fn supervise_mcp_server(
    server: McpServerConfig,
    tool_server: ToolServerHandle,
    mut service: Option<McpService>,
) {
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;

    loop {
        if let Some(active_service) = service.take() {
            let outcome = active_service.waiting().await;
            warn!(server = %server.name, ?outcome, "MCP connection closed");
            reconnect_delay = INITIAL_RECONNECT_DELAY;
        }

        info!(
            server = %server.name,
            reconnect_in_s = reconnect_delay.as_secs(),
            "waiting to reconnect MCP server"
        );
        sleep(reconnect_delay).await;

        match connect_mcp_server(&server, tool_server.clone()).await {
            Ok(new_service) => {
                info!(server = %server.name, "reconnected MCP tools");
                service = Some(new_service);
                reconnect_delay = INITIAL_RECONNECT_DELAY;
            }
            Err(err) => {
                reconnect_delay = next_reconnect_delay(reconnect_delay);
                warn!(
                    server = %server.name,
                    retry_in_s = reconnect_delay.as_secs(),
                    "failed to reconnect MCP server: {err}"
                );
            }
        }
    }
}

struct McpToolHandler {
    server_name: String,
    client_info: ClientInfo,
    tool_server: ToolServerHandle,
    allowed_tools: HashSet<String>,
    timeout: Duration,
    managed_tool_names: RwLock<Vec<String>>,
}

impl McpToolHandler {
    fn new(
        server_name: String,
        client_info: ClientInfo,
        tool_server: ToolServerHandle,
        allowed_tools: &[String],
        timeout: Duration,
    ) -> Self {
        Self {
            server_name,
            client_info,
            tool_server,
            allowed_tools: allowed_tools.iter().cloned().collect(),
            timeout,
            managed_tool_names: RwLock::new(Vec::new()),
        }
    }

    fn tool_allowed(&self, name: &str) -> bool {
        self.allowed_tools.is_empty() || self.allowed_tools.contains(name)
    }

    async fn replace_tools(
        &self,
        tools: Vec<Tool>,
        client: ServerSink,
    ) -> Result<usize, McpClientError> {
        let mut managed = self.managed_tool_names.write().await;
        for name in managed.drain(..) {
            self.tool_server.remove_tool(&name).await?;
        }

        for tool in tools {
            let name = tool.name.to_string();
            if !self.tool_allowed(&name) {
                continue;
            }
            let mcp_tool =
                McpTool::from_mcp_server(tool, client.clone()).with_timeout(self.timeout);
            self.tool_server.add_tool(mcp_tool).await?;
            managed.push(name);
        }
        Ok(managed.len())
    }

    async fn connect<T, E, A>(self, transport: T) -> Result<McpService, McpClientError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let service = ServiceExt::serve(self, transport)
            .await
            .map_err(|err| McpClientError::ConnectionError(err.to_string()))?;
        let tools = service.peer().list_all_tools().await?;
        let count = service
            .service()
            .replace_tools(tools, service.peer().clone())
            .await?;
        info!(server = %service.service().server_name, tool_count = count, "registered MCP tools");
        Ok(service)
    }
}

impl ClientHandler for McpToolHandler {
    fn get_info(&self) -> ClientInfo {
        self.client_info.clone()
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        let tools = match context.peer.list_all_tools().await {
            Ok(tools) => tools,
            Err(err) => {
                warn!(server = %self.server_name, "failed to refresh MCP tools: {err}");
                return;
            }
        };
        match self.replace_tools(tools, context.peer.clone()).await {
            Ok(count) => {
                info!(server = %self.server_name, tool_count = count, "refreshed MCP tools")
            }
            Err(err) => {
                warn!(server = %self.server_name, "failed to register refreshed MCP tools: {err}")
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
    use rig_core::tool::server::ToolServer;

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
        assert!(!is_legacy_sse_url(
            "http://homeassistant.local/api/webhook/mcp_secret"
        ));
    }

    #[test]
    fn filters_tools_when_allowlist_is_present() {
        let handler = McpToolHandler::new(
            "test".to_string(),
            ClientInfo::default(),
            ToolServer::new().run(),
            &["ha_search".to_string()],
            Duration::from_secs(1),
        );
        assert!(handler.tool_allowed("ha_search"));
        assert!(!handler.tool_allowed("ha_restart"));
    }
}
