use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::header::ORIGIN;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use constant_time_eq::constant_time_eq;
use rand::RngCore;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::CodexAppServer;

pub(super) struct AppsHttpServer {
    addr: SocketAddr,
    bearer_tokens: HashMap<String, String>,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl AppsHttpServer {
    pub(super) async fn start(
        servers: &[CodexAppServer],
        shutdown: CancellationToken,
    ) -> Result<Option<Self>> {
        if servers.is_empty() {
            return Ok(None);
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind Codex Apps loopback MCP server")?;
        let addr = listener
            .local_addr()
            .context("failed to read Codex Apps loopback MCP address")?;
        let mut router = Router::new();
        let mut bearer_tokens = HashMap::with_capacity(servers.len());
        for server in servers {
            let bearer_token = generate_bearer_token();
            let expected_authorization = Arc::<str>::from(format!("Bearer {bearer_token}"));
            bearer_tokens.insert(server.server_name().to_string(), bearer_token);
            let service = server.service.clone();
            let mcp_service = StreamableHttpService::new(
                move || Ok(service.clone()),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default()
                    .with_stateful_mode(false)
                    .with_json_response(true)
                    .with_cancellation_token(shutdown.clone()),
            );
            let connector_router = Router::new()
                .nest_service(&format!("/mcp/{}", server.server_name()), mcp_service)
                .layer(middleware::from_fn_with_state(
                    expected_authorization,
                    authorize_request,
                ));
            router = router.merge(connector_router);
        }
        let server_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                server_shutdown.cancelled().await;
            });
            if let Err(error) = server.await {
                tracing::warn!(%error, "Codex Apps loopback MCP server failed");
            }
        });

        Ok(Some(Self {
            addr,
            bearer_tokens,
            shutdown,
            task: Some(task),
        }))
    }

    pub(super) fn url(&self, server_name: &str) -> String {
        format!("http://{}/mcp/{server_name}", self.addr)
    }

    pub(super) fn bearer_token(&self, server_name: &str) -> Option<&str> {
        self.bearer_tokens.get(server_name).map(String::as_str)
    }

    pub(super) async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "failed to join Codex Apps loopback MCP server");
        }
    }
}

impl Drop for AppsHttpServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn generate_bearer_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn authorize_request(
    State(expected_authorization): State<Arc<str>>,
    request: Request<Body>,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    if request.headers().contains_key(ORIGIN) {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(authorization) = request.headers().get(AUTHORIZATION) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !constant_time_eq(authorization.as_bytes(), expected_authorization.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}
