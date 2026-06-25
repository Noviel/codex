use std::collections::HashMap;
use std::fmt;

use codex_config::AppToolApproval;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use thiserror::Error;

/// The runtime launch strategy for an effective MCP server.
#[derive(Debug, Clone)]
pub(crate) enum McpServerLaunch {
    Configured(Box<McpServerConfig>),
}

/// MCP server after runtime additions have been applied.
#[derive(Debug, Clone)]
pub struct EffectiveMcpServer {
    launch: McpServerLaunch,
    runtime_bearer_token: Option<RuntimeBearerToken>,
}

#[derive(Clone)]
struct RuntimeBearerToken(String);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeBearerTokenError {
    #[error("runtime bearer tokens require a streamable HTTP MCP server")]
    UnsupportedTransport,
    #[error("runtime bearer token must not be empty")]
    EmptyToken,
    #[error("runtime bearer token conflicts with configured HTTP authorization")]
    ConflictingAuthorization,
}

impl fmt::Debug for RuntimeBearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl EffectiveMcpServer {
    pub fn configured(config: McpServerConfig) -> Self {
        Self {
            launch: McpServerLaunch::Configured(Box::new(config)),
            runtime_bearer_token: None,
        }
    }

    /// Creates an HTTP MCP server with a process-owned bearer token that is
    /// intentionally absent from the serializable server configuration.
    pub fn configured_with_runtime_bearer_token(
        config: McpServerConfig,
        bearer_token: String,
    ) -> Result<Self, RuntimeBearerTokenError> {
        let McpServerTransportConfig::StreamableHttp {
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            ..
        } = &config.transport
        else {
            return Err(RuntimeBearerTokenError::UnsupportedTransport);
        };
        if bearer_token.trim().is_empty() {
            return Err(RuntimeBearerTokenError::EmptyToken);
        }
        let has_authorization_header = |headers: &Option<HashMap<String, String>>| {
            headers.as_ref().is_some_and(|headers| {
                headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
            })
        };
        if bearer_token_env_var.is_some()
            || has_authorization_header(http_headers)
            || has_authorization_header(env_http_headers)
        {
            return Err(RuntimeBearerTokenError::ConflictingAuthorization);
        }
        Ok(Self {
            launch: McpServerLaunch::Configured(Box::new(config)),
            runtime_bearer_token: Some(RuntimeBearerToken(bearer_token)),
        })
    }

    pub(crate) fn launch(&self) -> &McpServerLaunch {
        &self.launch
    }

    pub(crate) fn runtime_bearer_token(&self) -> Option<&str> {
        self.runtime_bearer_token
            .as_ref()
            .map(|token| token.0.as_str())
    }

    pub fn configured_config(&self) -> Option<&McpServerConfig> {
        match &self.launch {
            McpServerLaunch::Configured(config) => Some(config.as_ref()),
        }
    }

    pub fn enabled(&self) -> bool {
        match &self.launch {
            McpServerLaunch::Configured(config) => config.enabled,
        }
    }

    pub fn required(&self) -> bool {
        match &self.launch {
            McpServerLaunch::Configured(config) => config.required,
        }
    }
}

/// Transport origin retained for metrics and diagnostics after server launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpServerOrigin {
    Stdio,
    StreamableHttp(String),
}

impl McpServerOrigin {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp(origin) => origin,
        }
    }

    fn from_transport(transport: &McpServerTransportConfig) -> Option<Self> {
        match transport {
            McpServerTransportConfig::StreamableHttp { url, .. } => {
                let parsed = url::Url::parse(url).ok()?;
                Some(Self::StreamableHttp(parsed.origin().ascii_serialization()))
            }
            McpServerTransportConfig::Stdio { .. } => Some(Self::Stdio),
        }
    }
}

/// Semantic metadata that must survive after the server is launched.
#[derive(Debug, Clone)]
pub(crate) struct McpServerMetadata {
    pub environment_id: String,
    pub pollutes_memory: bool,
    pub origin: Option<McpServerOrigin>,
    pub supports_parallel_tool_calls: bool,
    pub default_tools_approval_mode: Option<AppToolApproval>,
    pub tool_approval_modes: HashMap<String, AppToolApproval>,
}

impl McpServerMetadata {
    pub fn tool_approval_mode(&self, tool_name: &str) -> AppToolApproval {
        self.tool_approval_modes
            .get(tool_name)
            .copied()
            .or(self.default_tools_approval_mode)
            .unwrap_or_default()
    }
}

impl From<&EffectiveMcpServer> for McpServerMetadata {
    fn from(server: &EffectiveMcpServer) -> Self {
        match server.launch() {
            McpServerLaunch::Configured(config) => Self {
                environment_id: config.environment_id.clone(),
                pollutes_memory: true,
                origin: McpServerOrigin::from_transport(&config.transport),
                supports_parallel_tool_calls: config.supports_parallel_tool_calls,
                default_tools_approval_mode: config.default_tools_approval_mode,
                tool_approval_modes: config
                    .tools
                    .iter()
                    .filter_map(|(name, config)| {
                        config
                            .approval_mode
                            .map(|approval_mode| (name.clone(), approval_mode))
                    })
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
