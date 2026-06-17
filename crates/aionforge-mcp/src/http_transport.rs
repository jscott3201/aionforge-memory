//! Streamable HTTP helpers for mounting the MCP server in host applications.

use std::convert::Infallible;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_engine::Memory;
use bytes::Bytes;
use http::Response;
use http_body_util::combinators::BoxBody;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Serialize;

use crate::AionforgeMcp;
use crate::http_body_limit::{
    DEFAULT_MAX_REQUEST_BODY_BYTES, RequestBodyLimitService, validate_max_request_body_bytes,
};
use crate::status::AuthPosture;

/// The path hosts should mount the Streamable HTTP service under.
pub const STREAMABLE_HTTP_ENDPOINT: &str = "/mcp";
/// The RFC 9728 well-known prefix for OAuth Protected Resource Metadata.
pub const OAUTH_PROTECTED_RESOURCE_WELL_KNOWN_PREFIX: &str =
    "/.well-known/oauth-protected-resource";

/// The boxed HTTP response body used by rmcp's Streamable HTTP service.
pub type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

type AionforgeRawStreamableHttpService<E> =
    StreamableHttpService<AionforgeMcp<E>, LocalSessionManager>;

/// The rmcp Streamable HTTP service for Aionforge Memory.
pub type AionforgeStreamableHttpService<E> =
    RequestBodyLimitService<AionforgeRawStreamableHttpService<E>>;

/// Return the RFC 9728 well-known path for an MCP endpoint path.
///
/// For the default `/mcp` endpoint this returns
/// `/.well-known/oauth-protected-resource/mcp`.
#[must_use]
pub fn oauth_protected_resource_well_known_path(endpoint_path: &str) -> String {
    let path = endpoint_path.trim_start_matches('/');
    if path.is_empty() {
        OAUTH_PROTECTED_RESOURCE_WELL_KNOWN_PREFIX.to_string()
    } else {
        format!("{OAUTH_PROTECTED_RESOURCE_WELL_KNOWN_PREFIX}/{path}")
    }
}

/// RFC 9728 Protected Resource Metadata for OAuth-protected HTTP deployments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OAuthProtectedResourceMetadata {
    /// Canonical resource identifier clients bind into the OAuth `resource` parameter.
    pub resource: String,
    /// OAuth authorization server issuer identifiers accepted by this protected resource.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authorization_servers: Vec<String>,
    /// Scope values clients may request for this protected resource.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
    /// Bearer token presentation methods supported by this protected resource.
    pub bearer_methods_supported: Vec<String>,
    /// Human-readable protected resource name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    /// Human-readable documentation URL for the protected resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_documentation: Option<String>,
    /// Human-readable policy URL for protected resource use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_policy_uri: Option<String>,
}

impl OAuthProtectedResourceMetadata {
    /// Build metadata for one MCP resource identifier and its authorization servers.
    #[must_use]
    pub fn new(
        resource: impl Into<String>,
        authorization_servers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            resource: resource.into(),
            authorization_servers: authorization_servers.into_iter().map(Into::into).collect(),
            scopes_supported: Vec::new(),
            bearer_methods_supported: vec!["header".to_string()],
            resource_name: Some("Aionforge Memory MCP".to_string()),
            resource_documentation: None,
            resource_policy_uri: None,
        }
    }

    /// Set supported OAuth scopes for this protected resource.
    #[must_use]
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes_supported = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Set the display name for this protected resource.
    #[must_use]
    pub fn with_resource_name(mut self, name: impl Into<String>) -> Self {
        self.resource_name = Some(name.into());
        self
    }

    /// Set the documentation URL for this protected resource.
    #[must_use]
    pub fn with_resource_documentation(mut self, url: impl Into<String>) -> Self {
        self.resource_documentation = Some(url.into());
        self
    }

    /// Set the policy URL for this protected resource.
    #[must_use]
    pub fn with_resource_policy_uri(mut self, url: impl Into<String>) -> Self {
        self.resource_policy_uri = Some(url.into());
        self
    }

    /// Serialize as compact JSON suitable for a well-known metadata response.
    ///
    /// # Panics
    /// Panics only if serializing this fixed metadata shape fails.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("OAuth protected resource metadata serializes")
    }
}

/// Streamable HTTP transport options with secure local defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamableHttpOptions {
    /// Accepted inbound `Host` authorities. Empty lists are rejected.
    pub allowed_hosts: Vec<String>,
    /// Accepted browser `Origin` values. Empty disables Origin validation.
    pub allowed_origins: Vec<String>,
    /// Whether rmcp should keep stateful MCP sessions.
    pub stateful_mode: bool,
    /// Whether stateless request-response calls should use JSON instead of SSE framing.
    pub json_response: bool,
    /// Maximum request body bytes accepted by the Streamable HTTP service.
    pub max_request_body_bytes: usize,
}

impl Default for StreamableHttpOptions {
    fn default() -> Self {
        Self {
            allowed_hosts: ["localhost", "127.0.0.1", "::1"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            allowed_origins: ["http://localhost", "http://127.0.0.1", "http://[::1]"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            stateful_mode: true,
            json_response: false,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
        }
    }
}

impl StreamableHttpOptions {
    /// Replace the allowed host list.
    #[must_use]
    pub fn with_allowed_hosts<I, S>(mut self, allowed_hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_hosts = allowed_hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Replace the allowed browser Origin list.
    #[must_use]
    pub fn with_allowed_origins<I, S>(mut self, allowed_origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_origins = allowed_origins.into_iter().map(Into::into).collect();
        self
    }

    /// Set whether rmcp should keep stateful sessions.
    #[must_use]
    pub fn with_stateful_mode(mut self, stateful_mode: bool) -> Self {
        self.stateful_mode = stateful_mode;
        self
    }

    /// Set whether stateless calls should prefer JSON responses.
    #[must_use]
    pub fn with_json_response(mut self, json_response: bool) -> Self {
        self.json_response = json_response;
        self
    }

    /// Set the maximum request body bytes accepted by Streamable HTTP.
    #[must_use]
    pub fn with_max_request_body_bytes(mut self, max_request_body_bytes: usize) -> Self {
        self.max_request_body_bytes = max_request_body_bytes;
        self
    }

    /// Convert these options to rmcp's Streamable HTTP config.
    ///
    /// # Errors
    /// Returns [`StreamableHttpConfigError`] if a host/origin entry is blank or
    /// if host validation would be disabled, or if the request body limit is zero.
    pub fn into_rmcp_config(self) -> Result<StreamableHttpServerConfig, StreamableHttpConfigError> {
        validate_non_empty_entries(&self.allowed_hosts, EntryKind::Host)?;
        if self.allowed_hosts.is_empty() {
            return Err(StreamableHttpConfigError::EmptyAllowedHosts);
        }
        validate_non_empty_entries(&self.allowed_origins, EntryKind::Origin)?;
        validate_max_request_body_bytes(self.max_request_body_bytes)?;
        Ok(StreamableHttpServerConfig::default()
            .with_allowed_hosts(self.allowed_hosts)
            .with_allowed_origins(self.allowed_origins)
            .with_stateful_mode(self.stateful_mode)
            .with_json_response(self.json_response))
    }
}

/// Configuration errors for Streamable HTTP setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamableHttpConfigError {
    /// Host validation cannot be disabled through [`StreamableHttpOptions`].
    EmptyAllowedHosts,
    /// An allowed host entry is empty or whitespace.
    BlankAllowedHost {
        /// The zero-based index of the bad host entry.
        index: usize,
    },
    /// An allowed origin entry is empty or whitespace.
    BlankAllowedOrigin {
        /// The zero-based index of the bad origin entry.
        index: usize,
    },
    /// The configured request body limit is zero.
    ZeroMaxRequestBodyBytes,
}

impl std::fmt::Display for StreamableHttpConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyAllowedHosts => f.write_str("streamable HTTP allowed_hosts cannot be empty"),
            Self::BlankAllowedHost { index } => {
                write!(f, "streamable HTTP allowed_hosts[{index}] cannot be blank")
            }
            Self::BlankAllowedOrigin { index } => {
                write!(
                    f,
                    "streamable HTTP allowed_origins[{index}] cannot be blank"
                )
            }
            Self::ZeroMaxRequestBodyBytes => {
                f.write_str("streamable HTTP max_request_body_bytes cannot be zero")
            }
        }
    }
}

impl std::error::Error for StreamableHttpConfigError {}

#[derive(Debug, Clone, Copy)]
enum EntryKind {
    Host,
    Origin,
}

fn validate_non_empty_entries(
    entries: &[String],
    kind: EntryKind,
) -> Result<(), StreamableHttpConfigError> {
    for (index, entry) in entries.iter().enumerate() {
        if entry.trim().is_empty() {
            return Err(match kind {
                EntryKind::Host => StreamableHttpConfigError::BlankAllowedHost { index },
                EntryKind::Origin => StreamableHttpConfigError::BlankAllowedOrigin { index },
            });
        }
    }
    Ok(())
}

/// Build rmcp's Streamable HTTP config from Aionforge options.
///
/// # Errors
/// Returns [`StreamableHttpConfigError`] when the options would disable host
/// validation or contain blank allow-list entries.
pub fn streamable_http_config(
    options: StreamableHttpOptions,
) -> Result<StreamableHttpServerConfig, StreamableHttpConfigError> {
    options.into_rmcp_config()
}

/// Build the Streamable HTTP service, selecting the OAuth resource-server posture.
///
/// Mount the returned service under [`STREAMABLE_HTTP_ENDPOINT`]. The `auth` posture's `enabled`
/// flag drives the handler: [`AuthPosture::disabled`] (the default-off path) reproduces today's
/// body-only behavior exactly; an enabled posture requires a validated request extension on every
/// identity-resolving tool (which a Tower validator inserts upstream — see
/// [`AuthValidators`](crate::AuthValidators)). The posture's issuer origins ride `server_status`.
///
/// # Errors
/// Returns [`StreamableHttpConfigError`] when the options are invalid.
pub fn streamable_http_service<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    options: StreamableHttpOptions,
    auth: AuthPosture,
) -> Result<AionforgeStreamableHttpService<E>, StreamableHttpConfigError> {
    streamable_http_service_with_consolidation(memory, options, auth, false)
}

/// Build the Streamable HTTP service, selecting auth and background consolidation posture.
///
/// `background_managed` must be true only when the host has started a serve-owned consolidation
/// loop for the same store. In that posture the MCP handler returns `ERR_CONSOLIDATE_MANAGED` for
/// foreground consolidation requests before they can race the background cursor writer.
///
/// # Errors
/// Returns [`StreamableHttpConfigError`] when the options are invalid.
pub fn streamable_http_service_with_consolidation<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    options: StreamableHttpOptions,
    auth: AuthPosture,
    background_managed: bool,
) -> Result<AionforgeStreamableHttpService<E>, StreamableHttpConfigError> {
    let max_request_body_bytes = options.max_request_body_bytes;
    let config = streamable_http_config(options)?;
    let service = StreamableHttpService::new(
        move || {
            Ok(AionforgeMcp::new_with_auth_posture_and_consolidation(
                Arc::clone(&memory),
                auth.clone(),
                background_managed,
            ))
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );
    RequestBodyLimitService::new(service, max_request_body_bytes)
}
