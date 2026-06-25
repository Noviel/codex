use std::collections::HashSet;
use std::io;
use std::sync::Mutex;
use std::time::Duration;

use codex_config::Constrained;
use codex_config::McpServerTransportConfig;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::EnvironmentManager;
use codex_mcp::McpConnectionManager;
use codex_mcp::McpRuntimeContext;
use codex_mcp::ToolPluginProvenance;
use codex_mcp::codex_apps_tools_cache_key;
use codex_protocol::ToolName;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_rmcp_client::InProcessTransportFactory;
use futures::FutureExt;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use reqwest::StatusCode;
use reqwest::header::ORIGIN;
use rmcp::ServiceExt;
use rmcp::model::ClientCapabilities;
use rmcp::model::Content;
use rmcp::model::ElicitationAction;
use rmcp::model::InitializeRequestParams;
use rmcp::model::JsonObject;
use rmcp::model::Meta;
use rmcp::model::ProtocolVersion;
use rmcp::model::Tool;
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::*;

#[derive(Clone, Debug, PartialEq)]
struct RecordedCall {
    name: String,
    arguments: Option<serde_json::Value>,
    meta: serde_json::Value,
}

#[derive(Clone)]
struct TestServer {
    tools: Arc<[Tool]>,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    call_gate: Option<CallGate>,
}

#[derive(Clone, Default)]
struct CallGate {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedCall {
                name: request.name.to_string(),
                arguments: request.arguments.map(serde_json::Value::Object),
                meta: serde_json::Value::Object(context.meta.0),
            });
        if let Some(call_gate) = &self.call_gate {
            call_gate.started.notify_one();
            call_gate.release.notified().await;
        }
        Ok(CallToolResult::success(vec![Content::text("forwarded")]))
    }
}

#[derive(Clone)]
struct TestFactory(TestServer);

impl InProcessTransportFactory for TestFactory {
    fn open(&self) -> BoxFuture<'static, io::Result<tokio::io::DuplexStream>> {
        let server = self.0.clone();
        async move {
            let (client, service) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                if let Ok(running) = server.serve(service).await {
                    let _ = running.waiting().await;
                }
            });
            Ok(client)
        }
        .boxed()
    }
}

fn connector_tool(connector_id: Option<&str>, connector_name: Option<&str>, name: &str) -> Tool {
    let mut tool = Tool::new(name.to_string(), "test tool", Arc::new(JsonObject::new()));
    let mut meta = JsonObject::new();
    if let Some(connector_id) = connector_id {
        meta.insert("connector_id".to_string(), json!(connector_id));
    }
    if let Some(connector_name) = connector_name {
        meta.insert("connector_name".to_string(), json!(connector_name));
        tool.title = Some(format!("{connector_name}_Title"));
    }
    tool.meta = Some(Meta(meta));
    tool
}

async fn initialized_client(factory: Arc<dyn InProcessTransportFactory>) -> Arc<RmcpClient> {
    let client = Arc::new(
        RmcpClient::new_in_process_client(factory)
            .await
            .expect("in-process client"),
    );
    client
        .initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("codex-apps-test", "1"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_06_18),
            /*timeout*/ None,
            Box::new(|_, _| {
                async {
                    Ok(codex_rmcp_client::ElicitationResponse {
                        action: ElicitationAction::Cancel,
                        content: None,
                        meta: None,
                    })
                }
                .boxed()
            }),
        )
        .await
        .expect("initialize client");
    client
}

async fn apps_with_tools(tools: Vec<Tool>) -> (CodexApps, Arc<Mutex<Vec<RecordedCall>>>) {
    apps_with_tools_and_gate(tools, /*call_gate*/ None).await
}

async fn apps_with_tools_and_gate(
    tools: Vec<Tool>,
    call_gate: Option<CallGate>,
) -> (CodexApps, Arc<Mutex<Vec<RecordedCall>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let upstream = initialized_client(Arc::new(TestFactory(TestServer {
        tools: Arc::from(tools),
        calls: Arc::clone(&calls),
        call_gate,
    })))
    .await;
    (CodexApps::start(upstream).await.expect("start apps"), calls)
}

#[tokio::test]
async fn shutdown_cancels_an_active_tool_call() {
    let call_gate = CallGate::default();
    let (apps, _) = apps_with_tools_and_gate(
        vec![connector_tool(
            Some("gmail"),
            Some("Gmail"),
            "GmailSearchMessages",
        )],
        Some(call_gate.clone()),
    )
    .await;
    let manager = Arc::new(mcp_manager(&apps).await);
    let call_task = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .call_tool(
                    "codex_apps__gmail",
                    "searchmessages",
                    /*arguments*/ None,
                    /*meta*/ None,
                )
                .await
        })
    };
    call_gate.started.notified().await;

    tokio::time::timeout(Duration::from_secs(1), apps.shutdown())
        .await
        .expect("shutdown should stop the active HTTP request");
    let call_result = tokio::time::timeout(Duration::from_secs(1), call_task)
        .await
        .expect("tool call should stop after shutdown")
        .expect("tool call task should not panic");
    assert!(call_result.is_err());

    call_gate.release.notify_waiters();
    manager.shutdown().await;
}

async fn mcp_manager(apps: &CodexApps) -> McpConnectionManager {
    let servers = apps.mcp_servers();
    let (tx_event, rx_event) = async_channel::unbounded();
    drop(rx_event);
    let codex_home = tempdir().expect("tempdir");
    McpConnectionManager::new(
        &servers,
        OAuthCredentialsStoreMode::default(),
        AuthKeyringBackendKind::default(),
        HashMap::new(),
        &Constrained::allow_any(AskForApproval::OnRequest),
        String::new(),
        tx_event,
        CancellationToken::new(),
        PermissionProfile::default(),
        McpRuntimeContext::new(
            Arc::new(EnvironmentManager::without_environments()),
            std::env::temp_dir(),
        ),
        codex_home.path().to_path_buf(),
        codex_apps_tools_cache_key(/*auth*/ None),
        /*prefix_mcp_tool_names*/ true,
        Default::default(),
        /*supports_openai_form_elicitation*/ false,
        ToolPluginProvenance::default(),
        /*auth*/ None,
        /*elicitation_reviewer*/ None,
    )
    .await
}

#[tokio::test]
async fn zero_connectors_ignores_tools_without_complete_identity() {
    let (apps, _) = apps_with_tools(vec![connector_tool(Some("mail"), None, "Search")]).await;
    assert!(apps.servers().is_empty());
    assert!(apps.mcp_servers().is_empty());
    apps.shutdown().await;
}

#[tokio::test]
async fn one_connector_is_an_ordinary_mcp_server_with_legacy_model_name() {
    let (apps, calls) = apps_with_tools(vec![connector_tool(
        Some("gmail"),
        Some("Gmail"),
        "GmailSearchMessages",
    )])
    .await;
    let server = &apps.servers()[0];
    assert_eq!(server.server_name(), "codex_apps__gmail");
    let server = apps
        .mcp_servers()
        .remove("codex_apps__gmail")
        .expect("Gmail MCP server");
    assert!(format!("{server:?}").contains("[REDACTED]"));
    let config = server.configured_config().expect("configured Gmail server");
    let McpServerTransportConfig::StreamableHttp {
        url, http_headers, ..
    } = &config.transport
    else {
        panic!("virtual app server should use streamable HTTP");
    };
    assert!(url.starts_with("http://127.0.0.1:"));
    assert!(url.ends_with("/mcp/codex_apps__gmail"));
    assert_eq!(http_headers, &None);
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP client");
    assert_eq!(
        client
            .post(url.as_str())
            .send()
            .await
            .expect("missing auth")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(url.as_str())
            .header("Authorization", "Bearer wrong")
            .send()
            .await
            .expect("wrong auth")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(url.as_str())
            .header(ORIGIN, "https://example.com")
            .send()
            .await
            .expect("browser origin")
            .status(),
        StatusCode::FORBIDDEN
    );
    let manager = mcp_manager(&apps).await;
    let listed = manager.list_all_tools().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].server_name, "codex_apps__gmail");
    assert_eq!(
        listed[0].canonical_tool_name(),
        ToolName::namespaced("mcp__codex_apps__gmail", "searchmessages")
    );
    assert_eq!(listed[0].tool.name.as_ref(), "searchmessages");
    assert_eq!(listed[0].tool.title.as_deref(), Some("Title"));
    manager
        .call_tool(
            "codex_apps__gmail",
            "searchmessages",
            Some(json!({"query": "rust"})),
            Some(json!({"threadId": "thread-1"})),
        )
        .await
        .expect("call virtual app tool");
    {
        let calls = calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            calls.as_slice(),
            &[RecordedCall {
                name: "GmailSearchMessages".to_string(),
                arguments: Some(json!({"query": "rust"})),
                meta: json!({"progressToken": 1, "threadId": "thread-1"}),
            }]
        );
    }
    manager.shutdown().await;

    let manager = mcp_manager(&apps).await;
    assert_eq!(manager.list_all_tools().await.len(), 1);
    manager.shutdown().await;

    let addr = reqwest::Url::parse(url)
        .expect("virtual server URL")
        .socket_addrs(|| None)
        .expect("virtual server address")[0];
    apps.shutdown().await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn collisions_use_legacy_identity_hashes() {
    let (apps, _) = apps_with_tools(vec![
        connector_tool(Some("drive-one"), Some("Drive!"), "DriveList"),
        connector_tool(Some("drive-two"), Some("Drive?"), "DriveGet"),
        connector_tool(Some("gmail"), Some("Gmail"), "GmailFoo-Bar"),
        connector_tool(Some("gmail"), Some("Gmail"), "GmailFoo_Bar"),
    ])
    .await;
    assert_eq!(apps.servers().len(), 3);
    assert_eq!(
        apps.servers()[0].server_name(),
        "codex_apps__drive_99a0d4a4035d"
    );
    assert_eq!(
        apps.servers()[1].server_name(),
        "codex_apps__drive_b469ba67a2f2"
    );
    let manager = mcp_manager(&apps).await;
    let names = manager
        .list_all_tools()
        .await
        .into_iter()
        .map(|tool| tool.canonical_tool_name())
        .collect::<HashSet<_>>();
    assert_eq!(
        names,
        HashSet::from([
            ToolName::namespaced("mcp__codex_apps__drive_99a0d4a4035d", "list"),
            ToolName::namespaced("mcp__codex_apps__drive_b469ba67a2f2", "get"),
            ToolName::namespaced("mcp__codex_apps__gmail", "foo_bar_7362b7bd5a54"),
            ToolName::namespaced("mcp__codex_apps__gmail", "foo_bar_8919b3893acb"),
        ])
    );
    manager.shutdown().await;
    apps.shutdown().await;
}
