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
use tower_service::Service;

use crate::AionforgeMcp;

/// The path hosts should mount the Streamable HTTP service under.
pub const STREAMABLE_HTTP_ENDPOINT: &str = "/mcp";

/// The boxed HTTP response body used by rmcp's Streamable HTTP service.
pub type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

/// The rmcp Streamable HTTP service for Aionforge Memory.
pub type AionforgeStreamableHttpService<E> =
    StreamableHttpService<AionforgeMcp<E>, LocalSessionManager>;

/// The bearer-authenticated Streamable HTTP service for Aionforge Memory.
pub type AionforgeAuthenticatedStreamableHttpService<E> =
    BearerAuthService<AionforgeStreamableHttpService<E>>;

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

/// Tower service wrapper that requires `Authorization: Bearer <token>`.
#[derive(Clone)]
pub struct BearerAuthService<S> {
    inner: S,
    token: BearerToken,
}

impl<S> BearerAuthService<S> {
    /// Wrap an HTTP service with static bearer-token authentication.
    #[must_use]
    pub fn new(inner: S, token: BearerToken) -> Self {
        Self { inner, token }
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
            return Box::pin(async { Ok(unauthorized_response()) });
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
    Ok(BearerAuthService::new(
        streamable_http_service(memory, options)?,
        token,
    ))
}

fn unauthorized_response() -> HttpResponse {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(WWW_AUTHENTICATE, r#"Bearer realm="aionforge-mcp""#)
        .body(Full::new(Bytes::from_static(b"Unauthorized: bearer token required")).boxed())
        .expect("valid unauthorized response")
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
