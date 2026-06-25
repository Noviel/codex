//! Connector-scoped virtual MCP servers backed by one Codex Apps connection.
//!
//! [`CodexApps::start`] reads one immutable tool snapshot from an already initialized
//! upstream client, then serves each connector as a distinct MCP endpoint on one authenticated
//! loopback HTTP listener. Every virtual server forwards calls through the same upstream
//! connection.
//!
//! This prototype intentionally does not own upstream authentication, disk caching, refresh or
//! tool-list-change notifications, app policy, approvals, resources, progress or server
//! notifications, long-name compatibility, upstream lifecycle, or shutdown of the shared
//! upstream client. Tools without complete connector identity are also unsupported and omitted.
//! This crate is not integrated into the production path yet and must not replace the legacy Apps
//! path until those behaviors move behind shared Apps-owned APIs.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_config::McpServerAuth;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_connectors::metadata::CODEX_APPS_MCP_SERVER_NAME;
use codex_connectors::metadata::connector_mcp_server_name;
use codex_connectors::metadata::connector_tool_name;
use codex_connectors::metadata::connector_tool_title;
use codex_mcp::EffectiveMcpServer;
use codex_rmcp_client::RmcpClient;
use codex_utils_string::sha1_12_hex_suffix;
use rmcp::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::Implementation;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use tokio_util::sync::CancellationToken;

const CODEX_APPS_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

mod http;

use self::http::AppsHttpServer;

/// An immutable connector inventory backed by one initialized Apps MCP client.
pub struct CodexApps {
    servers: Vec<CodexAppServer>,
    mcp_servers: HashMap<String, EffectiveMcpServer>,
    http_server: Option<AppsHttpServer>,
}

impl CodexApps {
    /// Loads one complete upstream `tools/list` page and starts its connector MCP endpoints.
    pub async fn start(upstream: Arc<RmcpClient>) -> Result<Self> {
        let listed = upstream
            .list_tools_with_connector_ids(/*params*/ None, Some(CODEX_APPS_LOAD_TIMEOUT))
            .await
            .context("failed to list Codex Apps tools")?;
        if listed.next_cursor.is_some() {
            bail!("Codex Apps virtual servers do not yet support paginated tool snapshots");
        }

        let mut builders = BTreeMap::<String, ConnectorServerBuilder>::new();
        for listed_tool in listed.tools {
            let (Some(connector_id), Some(connector_name)) =
                (listed_tool.connector_id, listed_tool.connector_name)
            else {
                continue;
            };
            let connector_id = connector_id.trim();
            let connector_name = connector_name.trim();
            if connector_id.is_empty() || connector_name.is_empty() {
                continue;
            }

            let builder = builders.entry(connector_id.to_string()).or_insert_with(|| {
                ConnectorServerBuilder {
                    connector_name: connector_name.to_string(),
                    connector_description: listed_tool.connector_description.clone(),
                    tools: Vec::new(),
                }
            });
            if builder.connector_name != connector_name {
                bail!("connector `{connector_id}` has inconsistent names in one tool snapshot");
            }
            if builder.connector_description.is_none() {
                builder.connector_description = listed_tool.connector_description;
            }
            builder.tools.push(listed_tool.tool);
        }

        let mut connector_counts_by_name = HashMap::<String, usize>::new();
        for builder in builders.values() {
            *connector_counts_by_name
                .entry(connector_mcp_server_name(&builder.connector_name))
                .or_default() += 1;
        }
        let colliding_server_names = connector_counts_by_name
            .into_iter()
            .filter_map(|(name, count)| (count > 1).then_some(name))
            .collect::<HashSet<_>>();
        let shutdown = CancellationToken::new();
        let mut servers = Vec::with_capacity(builders.len());
        for (connector_id, builder) in builders {
            let base_server_name = connector_mcp_server_name(&builder.connector_name);
            let raw_namespace_identity =
                format!("{CODEX_APPS_MCP_SERVER_NAME}\0{base_server_name}\0{connector_id}");
            let server_name = if colliding_server_names.contains(&base_server_name) {
                format!(
                    "{base_server_name}{}",
                    sha1_12_hex_suffix(&raw_namespace_identity)
                )
            } else {
                base_server_name
            };
            let server = CodexAppServer::new(
                connector_id,
                builder,
                server_name,
                raw_namespace_identity,
                Arc::clone(&upstream),
                shutdown.clone(),
            );
            servers.push(server);
        }
        let http_server = AppsHttpServer::start(&servers, shutdown).await?;
        let mut mcp_servers = HashMap::with_capacity(servers.len());
        if let Some(http_server) = &http_server {
            for server in &servers {
                let config = McpServerConfig {
                    transport: McpServerTransportConfig::StreamableHttp {
                        url: http_server.url(server.server_name()),
                        bearer_token_env_var: None,
                        http_headers: None,
                        env_http_headers: None,
                    },
                    auth: McpServerAuth::default(),
                    environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                    enabled: true,
                    required: false,
                    supports_parallel_tool_calls: false,
                    disabled_reason: None,
                    startup_timeout_sec: None,
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    scopes: None,
                    oauth: None,
                    oauth_resource: None,
                    tools: HashMap::new(),
                };
                let bearer_token = http_server
                    .bearer_token(server.server_name())
                    .with_context(|| {
                        format!(
                            "missing runtime bearer token for connector MCP server `{}`",
                            server.server_name()
                        )
                    })?
                    .to_string();
                let effective =
                    EffectiveMcpServer::configured_with_runtime_bearer_token(config, bearer_token)
                        .context("failed to configure connector MCP runtime authentication")?;
                mcp_servers.insert(server.server_name().to_string(), effective);
            }
        }
        Ok(Self {
            servers,
            mcp_servers,
            http_server,
        })
    }

    pub fn servers(&self) -> &[CodexAppServer] {
        &self.servers
    }

    /// Returns ordinary configured HTTP MCP servers for the connector inventory.
    ///
    /// Each server's process capability lives in its non-serializable effective runtime state.
    pub fn mcp_servers(&self) -> HashMap<String, EffectiveMcpServer> {
        self.mcp_servers.clone()
    }

    /// Stops the loopback listener and all active connector MCP sessions.
    pub async fn shutdown(mut self) {
        if let Some(http_server) = self.http_server.take() {
            http_server.shutdown().await;
        }
    }
}

struct ConnectorServerBuilder {
    connector_name: String,
    connector_description: Option<String>,
    tools: Vec<Tool>,
}

/// One connector's virtual MCP registration.
pub struct CodexAppServer {
    service: ConnectorMcpServer,
}

impl CodexAppServer {
    fn new(
        connector_id: String,
        builder: ConnectorServerBuilder,
        server_name: String,
        raw_namespace_identity: String,
        upstream: Arc<RmcpClient>,
        shutdown: CancellationToken,
    ) -> Self {
        let mut candidates = Vec::with_capacity(builder.tools.len());
        let mut seen_raw_identities = HashSet::new();
        for tool in builder.tools {
            let upstream_name = tool.name.to_string();
            let base_callable = connector_tool_name(
                &upstream_name,
                Some(&connector_id),
                Some(&builder.connector_name),
            );
            let raw_tool_identity =
                format!("{raw_namespace_identity}\0{base_callable}\0{upstream_name}");
            if seen_raw_identities.insert(raw_tool_identity.clone()) {
                candidates.push(ToolCandidate {
                    tool,
                    upstream_name,
                    base_callable,
                    raw_tool_identity,
                });
            }
        }
        let mut tool_counts_by_name = HashMap::<String, usize>::new();
        for candidate in &candidates {
            *tool_counts_by_name
                .entry(candidate.base_callable.clone())
                .or_default() += 1;
        }
        let colliding_tool_names = tool_counts_by_name
            .into_iter()
            .filter_map(|(name, count)| (count > 1).then_some(name))
            .collect::<HashSet<_>>();

        let mut tools = Vec::with_capacity(candidates.len());
        let mut upstream_names = HashMap::with_capacity(candidates.len());
        for mut candidate in candidates {
            let exposed_name = if colliding_tool_names.contains(candidate.base_callable.as_str()) {
                format!(
                    "{}{}",
                    candidate.base_callable,
                    sha1_12_hex_suffix(&candidate.raw_tool_identity)
                )
            } else {
                candidate.base_callable
            };
            candidate.tool.name = Cow::Owned(exposed_name.clone());
            if let Some(title) = candidate.tool.title.take() {
                candidate.tool.title =
                    Some(connector_tool_title(Some(&builder.connector_name), &title));
            }
            upstream_names.insert(exposed_name, candidate.upstream_name);
            tools.push(candidate.tool);
        }

        let service = ConnectorMcpServer {
            connector_id,
            server_name,
            connector_name: builder.connector_name,
            connector_description: builder.connector_description,
            tools: Arc::from(tools),
            upstream_names: Arc::new(upstream_names),
            upstream,
            shutdown,
        };
        Self { service }
    }

    pub fn connector_id(&self) -> &str {
        &self.service.connector_id
    }

    pub fn connector_name(&self) -> &str {
        &self.service.connector_name
    }

    pub fn connector_description(&self) -> Option<&str> {
        self.service.connector_description.as_deref()
    }

    /// Model-visible logical server name used for routing and registration.
    pub fn server_name(&self) -> &str {
        &self.service.server_name
    }
}

struct ToolCandidate {
    tool: Tool,
    upstream_name: String,
    base_callable: String,
    raw_tool_identity: String,
}

#[derive(Clone)]
struct ConnectorMcpServer {
    connector_id: String,
    server_name: String,
    connector_name: String,
    connector_description: Option<String>,
    tools: Arc<[Tool]>,
    upstream_names: Arc<HashMap<String, String>>,
    upstream: Arc<RmcpClient>,
    shutdown: CancellationToken,
}

impl ServerHandler for ConnectorMcpServer {
    fn get_info(&self) -> ServerInfo {
        let implementation =
            Implementation::new(self.server_name.clone(), env!("CARGO_PKG_VERSION"))
                .with_title(self.connector_name.clone());
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation);
        info.instructions = self.connector_description.clone();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: self.tools.to_vec(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(upstream_name) = self.upstream_names.get(request.name.as_ref()) else {
            return Err(rmcp::ErrorData::invalid_params(
                format!("unknown tool `{}`", request.name),
                None,
            ));
        };
        let mut meta = context.meta.0;
        if let Some(request_meta) = request.meta {
            meta.extend(request_meta.0);
        }
        let call = self.upstream.call_tool(
            upstream_name.clone(),
            request.arguments.map(serde_json::Value::Object),
            (!meta.is_empty()).then_some(serde_json::Value::Object(meta)),
            /*timeout*/ None,
        );
        tokio::select! {
            result = call => result
                .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None)),
            _ = self.shutdown.cancelled() => Err(rmcp::ErrorData::internal_error(
                "Codex Apps MCP server is shutting down",
                None,
            )),
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
