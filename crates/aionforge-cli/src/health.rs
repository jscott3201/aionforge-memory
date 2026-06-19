//! Secret-free HTTP liveness and version responses for `aionforge serve http`.

use std::convert::Infallible;

use aionforge_config::Config;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Response, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};

/// Boxed HTTP response body used by the serve router.
pub(crate) type HealthResponse = Response<BoxBody<Bytes, Infallible>>;

/// Startup-captured, secret-free build/config snapshot served by `/version`.
#[derive(Clone)]
pub(crate) struct VersionInfo {
    version: &'static str,
    build_sha: &'static str,
    build_status: &'static str,
    built_at: &'static str,
    embedder_dimension: u32,
    native_dimension: Option<u32>,
}

impl VersionInfo {
    /// Capture the version snapshot from process build metadata and startup config.
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            build_sha: aionforge_mcp::build_sha(),
            build_status: aionforge_mcp::build_status(),
            built_at: aionforge_mcp::build_timestamp(),
            embedder_dimension: config.embedder.dimension,
            native_dimension: config.embedder.native_dimension,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "version": self.version,
            "build_sha": self.build_sha,
            "build_status": self.build_status,
            "built_at": self.built_at,
            "embedder_dimension": self.embedder_dimension,
            "native_dimension": self.native_dimension,
        })
    }
}

/// Pure process liveness response. This does not touch the store or embedder.
pub(crate) async fn livez_handler() -> HealthResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/plain")
        .body(Full::new(Bytes::from_static(b"ok")).boxed())
        .expect("valid livez response")
}

/// Render the startup-captured version snapshot as JSON.
pub(crate) fn version_response(version: &VersionInfo) -> HealthResponse {
    let body = serde_json::to_vec(&version.to_json()).expect("version JSON serializes");
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)).boxed())
        .expect("valid version response")
}
