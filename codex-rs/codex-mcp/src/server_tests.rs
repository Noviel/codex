use codex_config::McpServerConfig;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::EffectiveMcpServer;
use super::RuntimeBearerTokenError;

fn config(value: serde_json::Value) -> McpServerConfig {
    serde_json::from_value(value).expect("valid MCP server config")
}

#[test]
fn runtime_bearer_token_requires_unambiguous_http_configuration() {
    assert_eq!(
        EffectiveMcpServer::configured_with_runtime_bearer_token(
            config(json!({"command": "echo"})),
            "secret".to_string(),
        )
        .expect_err("stdio must reject HTTP bearer tokens"),
        RuntimeBearerTokenError::UnsupportedTransport
    );
    assert_eq!(
        EffectiveMcpServer::configured_with_runtime_bearer_token(
            config(json!({"url": "http://127.0.0.1/mcp"})),
            String::new(),
        )
        .expect_err("empty bearer token must be rejected"),
        RuntimeBearerTokenError::EmptyToken
    );
    assert_eq!(
        EffectiveMcpServer::configured_with_runtime_bearer_token(
            config(json!({
                "url": "http://127.0.0.1/mcp",
                "http_headers": {"Authorization": "Bearer configured"},
            })),
            "runtime-secret".to_string(),
        )
        .expect_err("configured authorization must not compete with runtime auth"),
        RuntimeBearerTokenError::ConflictingAuthorization
    );

    let server = EffectiveMcpServer::configured_with_runtime_bearer_token(
        config(json!({"url": "http://127.0.0.1/mcp"})),
        "runtime-secret".to_string(),
    )
    .expect("valid runtime bearer token");
    let debug = format!("{server:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("runtime-secret"));
}
