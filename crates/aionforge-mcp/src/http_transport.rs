//! Streamable HTTP helpers for mounting the MCP server in host applications.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use aionforge_domain::contracts::Embedder;
use aionforge_engine::Memory;
use bytes::Bytes;
use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Serialize;
use tower_service::Service;

use crate::AionforgeMcp;

/// The path hosts should mount the Streamable HTTP service under.
pub const STREAMABLE_HTTP_ENDPOINT: &str = "/mcp";
/// The RFC 9728 well-known prefix for OAuth Protected Resource Metadata.
pub const OAUTH_PROTECTED_RESOURCE_WELL_KNOWN_PREFIX: &str =
    "/.well-known/oauth-protected-resource";

/// The boxed HTTP response body used by rmcp's Streamable HTTP service.
pub type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

/// The rmcp Streamable HTTP service for Aionforge Memory.
pub type AionforgeStreamableHttpService<E> =
    StreamableHttpService<AionforgeMcp<E>, LocalSessionManager>;

/// The bearer-authenticated Streamable HTTP service for Aionforge Memory.
pub type AionforgeAuthenticatedStreamableHttpService<E> =
    BearerAuthService<AionforgeStreamableHttpService<E>>;

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

    /// Convert these options to rmcp's Streamable HTTP config.
    ///
    /// # Errors
    /// Returns [`StreamableHttpConfigError`] if a host/origin entry is blank or
    /// if host validation would be disabled.
    pub fn into_rmcp_config(self) -> Result<StreamableHttpServerConfig, StreamableHttpConfigError> {
        validate_non_empty_entries(&self.allowed_hosts, EntryKind::Host)?;
        if self.allowed_hosts.is_empty() {
            return Err(StreamableHttpConfigError::EmptyAllowedHosts);
        }
        validate_non_empty_entries(&self.allowed_origins, EntryKind::Origin)?;
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
    /// The configured bearer token is empty or whitespace.
    EmptyBearerToken,
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
            Self::EmptyBearerToken => f.write_str("streamable HTTP bearer token cannot be empty"),
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

/// A bearer token for Streamable HTTP authentication.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    /// Build a bearer token from host configuration.
    ///
    /// # Errors
    /// Returns [`StreamableHttpConfigError::EmptyBearerToken`] when `raw` is empty
    /// or whitespace.
    pub fn new(raw: impl Into<String>) -> Result<Self, StreamableHttpConfigError> {
        let raw = raw.into();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(StreamableHttpConfigError::EmptyBearerToken);
        }
        Ok(Self(trimmed.to_string()))
    }

    fn matches_authorization_header(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers.get(AUTHORIZATION) else {
            return false;
        };
        let Ok(raw) = value.to_str() else {
            return false;
        };
        let Some((scheme, token)) = raw.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("Bearer") && constant_time_eq(token.trim(), &self.0)
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

/// `WWW-Authenticate: Bearer` challenge metadata for protected HTTP deployments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerAuthChallenge {
    realm: String,
    resource_metadata_url: Option<String>,
    scope: Option<String>,
}

impl Default for BearerAuthChallenge {
    fn default() -> Self {
        Self {
            realm: "aionforge-mcp".to_string(),
            resource_metadata_url: None,
            scope: None,
        }
    }
}

impl BearerAuthChallenge {
    /// Set the bearer realm.
    #[must_use]
    pub fn with_realm(mut self, realm: impl Into<String>) -> Self {
        self.realm = realm.into();
        self
    }

    /// Advertise the RFC 9728 Protected Resource Metadata URL.
    #[must_use]
    pub fn with_resource_metadata_url(mut self, url: impl Into<String>) -> Self {
        self.resource_metadata_url = Some(url.into());
        self
    }

    /// Advertise the minimum scopes required for the current protected resource.
    #[must_use]
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Render the value for a `WWW-Authenticate` response header.
    #[must_use]
    pub fn header_value(&self) -> String {
        let mut out = format!("Bearer realm={}", quoted_auth_param(&self.realm));
        if let Some(url) = &self.resource_metadata_url {
            out.push_str(", resource_metadata=");
            out.push_str(&quoted_auth_param(url));
        }
        if let Some(scope) = &self.scope {
            out.push_str(", scope=");
            out.push_str(&quoted_auth_param(scope));
        }
        out
    }
}

/// Tower service wrapper that requires `Authorization: Bearer <token>`.
#[derive(Clone)]
pub struct BearerAuthService<S> {
    inner: S,
    token: BearerToken,
    challenge: BearerAuthChallenge,
}

impl<S> BearerAuthService<S> {
    /// Wrap an HTTP service with static bearer-token authentication.
    #[must_use]
    pub fn new(inner: S, token: BearerToken) -> Self {
        Self::with_challenge(inner, token, BearerAuthChallenge::default())
    }

    /// Wrap an HTTP service with static bearer-token authentication and a custom challenge.
    #[must_use]
    pub fn with_challenge(inner: S, token: BearerToken, challenge: BearerAuthChallenge) -> Self {
        Self {
            inner,
            token,
            challenge,
        }
    }

    /// Consume the wrapper and return the inner service.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Check whether a request header map carries the configured bearer token.
    #[must_use]
    pub fn is_authorized_headers(&self, headers: &HeaderMap) -> bool {
        self.token.matches_authorization_header(headers)
    }
}

impl<S, B> Service<Request<B>> for BearerAuthService<S>
where
    S: Service<Request<B>, Response = HttpResponse, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = HttpResponse;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        if !self.token.matches_authorization_header(request.headers()) {
            let challenge = self.challenge.clone();
            return Box::pin(async move { Ok(unauthorized_response(&challenge)) });
        }
        Box::pin(self.inner.call(request))
    }
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

/// Build an unauthenticated Streamable HTTP service.
///
/// Mount the returned service under [`STREAMABLE_HTTP_ENDPOINT`].
///
/// # Errors
/// Returns [`StreamableHttpConfigError`] when the options are invalid.
pub fn streamable_http_service<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    options: StreamableHttpOptions,
) -> Result<AionforgeStreamableHttpService<E>, StreamableHttpConfigError> {
    let config = streamable_http_config(options)?;
    Ok(StreamableHttpService::new(
        move || Ok(AionforgeMcp::new(Arc::clone(&memory))),
        Arc::new(LocalSessionManager::default()),
        config,
    ))
}

/// Build a bearer-authenticated Streamable HTTP service.
///
/// Mount the returned service under [`STREAMABLE_HTTP_ENDPOINT`].
///
/// # Errors
/// Returns [`StreamableHttpConfigError`] when the options are invalid.
pub fn streamable_http_service_with_auth<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    options: StreamableHttpOptions,
    token: BearerToken,
) -> Result<AionforgeAuthenticatedStreamableHttpService<E>, StreamableHttpConfigError> {
    streamable_http_service_with_auth_challenge(
        memory,
        options,
        token,
        BearerAuthChallenge::default(),
    )
}

/// Build a bearer-authenticated Streamable HTTP service with a custom challenge.
///
/// Mount the returned service under [`STREAMABLE_HTTP_ENDPOINT`].
///
/// # Errors
/// Returns [`StreamableHttpConfigError`] when the options are invalid.
pub fn streamable_http_service_with_auth_challenge<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    options: StreamableHttpOptions,
    token: BearerToken,
    challenge: BearerAuthChallenge,
) -> Result<AionforgeAuthenticatedStreamableHttpService<E>, StreamableHttpConfigError> {
    Ok(BearerAuthService::with_challenge(
        streamable_http_service(memory, options)?,
        token,
        challenge,
    ))
}

fn unauthorized_response(challenge: &BearerAuthChallenge) -> HttpResponse {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(WWW_AUTHENTICATE, challenge.header_value())
        .body(Full::new(Bytes::from_static(b"Unauthorized: bearer token required")).boxed())
        .expect("valid unauthorized response")
}

fn quoted_auth_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars().filter(|ch| !ch.is_ascii_control()) {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for i in 0..len {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(i).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
}
