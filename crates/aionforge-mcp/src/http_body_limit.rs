//! Request body limiting for Streamable HTTP hosts.

use std::convert::Infallible;
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::{BodyExt, Full, LengthLimitError};
use tower_service::Service;

use crate::http_transport::{HttpResponse, StreamableHttpConfigError};

/// Default maximum Streamable HTTP request body size: 1 MiB.
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

pub(crate) fn validate_max_request_body_bytes(max: usize) -> Result<(), StreamableHttpConfigError> {
    if max == 0 {
        return Err(StreamableHttpConfigError::ZeroMaxRequestBodyBytes);
    }
    Ok(())
}

/// Tower service wrapper that bounds request bodies before they reach rmcp.
#[derive(Clone)]
pub struct RequestBodyLimitService<S> {
    inner: S,
    max_request_body_bytes: usize,
}

impl<S> RequestBodyLimitService<S> {
    /// Wrap an HTTP service with a request body size limit.
    ///
    /// # Errors
    /// Returns [`StreamableHttpConfigError::ZeroMaxRequestBodyBytes`] when `max_request_body_bytes`
    /// is zero.
    pub fn new(inner: S, max_request_body_bytes: usize) -> Result<Self, StreamableHttpConfigError> {
        validate_max_request_body_bytes(max_request_body_bytes)?;
        Ok(Self {
            inner,
            max_request_body_bytes,
        })
    }

    /// Return the configured request body limit in bytes.
    #[must_use]
    pub fn max_request_body_bytes(&self) -> usize {
        self.max_request_body_bytes
    }

    /// Consume the wrapper and return the inner service.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Handle one request, mirroring rmcp's Streamable HTTP convenience API.
    pub async fn handle<B>(&self, request: Request<B>) -> HttpResponse
    where
        S: Clone
            + Service<Request<Full<Bytes>>, Response = HttpResponse, Error = Infallible>
            + Send
            + 'static,
        S::Future: Send + 'static,
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        let mut service = self.clone();
        match service.call(request).await {
            Ok(response) => response,
            Err(error) => match error {},
        }
    }
}

impl<S, B> Service<Request<B>> for RequestBodyLimitService<S>
where
    S: Clone
        + Service<Request<Full<Bytes>>, Response = HttpResponse, Error = Infallible>
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    type Response = HttpResponse;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let max = self.max_request_body_bytes;
        if let Some(response) = validate_content_length(request.headers(), max) {
            return Box::pin(async move { Ok(response) });
        }

        let mut inner = self.inner.clone();
        let (parts, body) = request.into_parts();
        Box::pin(async move {
            let body = match read_limited_body(body, max).await {
                Ok(bytes) => Full::new(bytes),
                Err(response) => return Ok(response),
            };
            inner.call(Request::from_parts(parts, body)).await
        })
    }
}

fn validate_content_length(headers: &HeaderMap, max: usize) -> Option<HttpResponse> {
    let value = headers.get(CONTENT_LENGTH)?;
    let Ok(raw) = value.to_str() else {
        return Some(bad_request_response(
            "Bad Request: invalid Content-Length header",
        ));
    };
    let Ok(size) = raw.parse::<u64>() else {
        return Some(bad_request_response(
            "Bad Request: invalid Content-Length header",
        ));
    };
    if size > max as u64 {
        return Some(payload_too_large_response(max));
    }
    None
}

async fn read_limited_body<B>(body: B, max: usize) -> Result<Bytes, HttpResponse>
where
    B: Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    match http_body_util::Limited::new(body, max).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => {
            Err(payload_too_large_response(max))
        }
        Err(error) => Err(bad_request_response(&format!(
            "Bad Request: failed to read request body: {error}"
        ))),
    }
}

fn payload_too_large_response(max: usize) -> HttpResponse {
    plain_text_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("Payload Too Large: request body exceeds {max} bytes"),
    )
}

fn bad_request_response(message: &str) -> HttpResponse {
    plain_text_response(StatusCode::BAD_REQUEST, message)
}

fn plain_text_response(status: StatusCode, message: impl Into<String>) -> HttpResponse {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(message.into())).boxed())
        .expect("valid plain-text response")
}
