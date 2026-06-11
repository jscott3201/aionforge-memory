//! MCP server command execution.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use aionforge_mcp::{
    AionforgeAuthenticatedStreamableHttpService, AionforgeStreamableHttpService,
    BearerAuthChallenge, BearerToken, OAuthProtectedResourceMetadata, STREAMABLE_HTTP_ENDPOINT,
    StreamableHttpOptions, oauth_protected_resource_well_known_path, serve_stdio,
    streamable_http_service, streamable_http_service_with_auth_challenge,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoHttpBuilder;
use tokio::net::TcpListener;
use tower_service::Service;
use url::Url;

use crate::cli::{ServeArgs, ServeTransport};
use crate::error::CliError;
use crate::host::{HostOptions, RuntimeEmbedder, load_config, open_memory};

type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

pub(crate) async fn run(options: &HostOptions, args: ServeArgs) -> Result<(), CliError> {
    let config = load_config(options)?;
    let memory = open_memory(&config)?;
    match args.transport {
        ServeTransport::Stdio => serve_stdio(memory)
            .await
            .map_err(|error| CliError::Serve(error.to_string())),
        ServeTransport::Http => serve_http(memory, args).await,
    }
}

async fn serve_http(
    memory: Arc<aionforge::Memory<RuntimeEmbedder>>,
    args: ServeArgs,
) -> Result<(), CliError> {
    let oauth_metadata = oauth_metadata(&args)?;
    let oauth_challenge = oauth_metadata
        .as_ref()
        .map(|metadata| oauth_challenge(metadata, &args.oauth_scopes));
    let mut options = StreamableHttpOptions::default()
        .with_stateful_mode(!args.stateless)
        .with_json_response(args.json_response);
    if let Some(max_request_body_bytes) = args.max_request_body_bytes {
        options = options.with_max_request_body_bytes(max_request_body_bytes);
    }
    if !args.allowed_hosts.is_empty() {
        options = options.with_allowed_hosts(args.allowed_hosts);
    }
    if !args.allowed_origins.is_empty() {
        options = options.with_allowed_origins(args.allowed_origins);
    }

    let service = match args.bearer_token_env {
        Some(env) => {
            let token = std::env::var(&env).map_err(|_| {
                CliError::Serve(format!(
                    "bearer token environment variable {env} is not set"
                ))
            })?;
            HttpMcpService::Authenticated(streamable_http_service_with_auth(
                memory,
                options,
                BearerToken::new(token).map_err(|error| CliError::Serve(error.to_string()))?,
                oauth_challenge,
            )?)
        }
        None => HttpMcpService::Open(streamable_http_service(memory, options)?),
    };
    let service = HttpMcpRouter {
        inner: service,
        oauth_metadata,
    };

    let listener = TcpListener::bind(args.listen).await?;
    let builder = AutoHttpBuilder::new(TokioExecutor::new());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let io = TokioIo::new(stream);
                let builder = builder.clone();
                let service = service.clone();
                tokio::spawn(async move {
                    let hyper_service = service_fn(move |request| {
                        let mut service = service.clone();
                        async move { service.call(request).await }
                    });
                    let _ = builder.serve_connection(io, hyper_service).await;
                });
            }
            interrupted = tokio::signal::ctrl_c() => {
                interrupted?;
                break;
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
enum HttpMcpService {
    Open(AionforgeStreamableHttpService<RuntimeEmbedder>),
    Authenticated(AionforgeAuthenticatedStreamableHttpService<RuntimeEmbedder>),
}

#[derive(Clone)]
struct HttpMcpRouter {
    inner: HttpMcpService,
    oauth_metadata: Option<OAuthMetadataRoute>,
}

#[derive(Clone, Debug)]
struct OAuthMetadataRoute {
    path: String,
    resource: String,
    body: Arc<str>,
}

impl HttpMcpRouter {
    async fn call(&mut self, request: Request<Incoming>) -> Result<HttpResponse, Infallible> {
        if let Some(metadata) = &self.oauth_metadata
            && request.uri().path() == metadata.path
        {
            if request.method() == Method::GET {
                return Ok(json_response(Arc::clone(&metadata.body)));
            }
            return Ok(method_not_allowed_response());
        }
        if request.uri().path() != STREAMABLE_HTTP_ENDPOINT {
            return Ok(not_found_response());
        }
        match &mut self.inner {
            HttpMcpService::Open(service) => service.call(request).await,
            HttpMcpService::Authenticated(service) => service.call(request).await,
        }
    }
}

fn streamable_http_service_with_auth(
    memory: Arc<aionforge::Memory<RuntimeEmbedder>>,
    options: StreamableHttpOptions,
    token: BearerToken,
    oauth_challenge: Option<BearerAuthChallenge>,
) -> Result<AionforgeAuthenticatedStreamableHttpService<RuntimeEmbedder>, CliError> {
    Ok(streamable_http_service_with_auth_challenge(
        memory,
        options,
        token,
        oauth_challenge.unwrap_or_default(),
    )?)
}

fn oauth_metadata(args: &ServeArgs) -> Result<Option<OAuthMetadataRoute>, CliError> {
    if args.oauth_issuers.is_empty() {
        if !args.oauth_scopes.is_empty() {
            return Err(CliError::Serve(
                "--oauth-scope requires at least one --oauth-issuer".to_string(),
            ));
        }
        return Ok(None);
    }
    if args.bearer_token_env.is_none() {
        return Err(CliError::Serve(
            "--oauth-issuer requires --bearer-token-env".to_string(),
        ));
    }
    let endpoint_url = endpoint_url(args)?;
    let metadata_path = oauth_protected_resource_well_known_path(STREAMABLE_HTTP_ENDPOINT);
    let metadata_url = resource_metadata_url(&endpoint_url, &metadata_path)?;
    let metadata = OAuthProtectedResourceMetadata::new(endpoint_url.as_str(), &args.oauth_issuers)
        .with_scopes(&args.oauth_scopes);
    Ok(Some(OAuthMetadataRoute {
        path: metadata_path,
        resource: metadata_url,
        body: Arc::from(metadata.to_json()),
    }))
}

fn oauth_challenge(metadata: &OAuthMetadataRoute, scopes: &[String]) -> BearerAuthChallenge {
    let mut challenge =
        BearerAuthChallenge::default().with_resource_metadata_url(metadata.resource.clone());
    if !scopes.is_empty() {
        challenge = challenge.with_scope(scopes.join(" "));
    }
    challenge
}

fn endpoint_url(args: &ServeArgs) -> Result<Url, CliError> {
    let raw = args
        .public_url
        .clone()
        .unwrap_or_else(|| default_endpoint_url(args.listen));
    let url = Url::parse(&raw)
        .map_err(|error| CliError::Serve(format!("invalid public MCP URL {raw:?}: {error}")))?;
    if url.path() != STREAMABLE_HTTP_ENDPOINT {
        return Err(CliError::Serve(format!(
            "public MCP URL path must be {STREAMABLE_HTTP_ENDPOINT}"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(CliError::Serve(
            "public MCP URL must not include a query string or fragment".to_string(),
        ));
    }
    Ok(url)
}

fn default_endpoint_url(listen: SocketAddr) -> String {
    let host = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    format!("http://{host}:{}{STREAMABLE_HTTP_ENDPOINT}", listen.port())
}

fn resource_metadata_url(endpoint_url: &Url, metadata_path: &str) -> Result<String, CliError> {
    let mut url = endpoint_url.clone();
    url.set_path(metadata_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn json_response(body: Arc<str>) -> HttpResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.as_ref().to_owned())).boxed())
        .expect("valid metadata response")
}

fn method_not_allowed_response() -> HttpResponse {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .body(Full::new(Bytes::from_static(b"Method Not Allowed")).boxed())
        .expect("valid method not allowed response")
}

fn not_found_response() -> HttpResponse {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from_static(b"Not Found")).boxed())
        .expect("valid not found response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ServeTransport;

    fn http_args() -> ServeArgs {
        ServeArgs {
            transport: ServeTransport::Http,
            listen: "0.0.0.0:3918".parse().expect("addr"),
            bearer_token_env: Some("AIONFORGE_MCP_TOKEN".to_string()),
            public_url: None,
            oauth_issuers: Vec::new(),
            oauth_scopes: Vec::new(),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            stateless: false,
            json_response: false,
            max_request_body_bytes: None,
        }
    }

    #[test]
    fn default_public_url_uses_loopback_for_unspecified_bind() {
        let args = http_args();
        let url = endpoint_url(&args).expect("url");

        assert_eq!(url.as_str(), "http://127.0.0.1:3918/mcp");
    }

    #[test]
    fn oauth_metadata_uses_public_url_and_well_known_path() {
        let mut args = http_args();
        args.public_url = Some("https://memory.example.com/mcp".to_string());
        args.oauth_issuers = vec!["https://auth.example.com".to_string()];
        args.oauth_scopes = vec!["memory.read".to_string(), "memory.write".to_string()];

        let route = oauth_metadata(&args)
            .expect("metadata builds")
            .expect("metadata enabled");

        assert_eq!(route.path, "/.well-known/oauth-protected-resource/mcp");
        assert_eq!(
            route.resource,
            "https://memory.example.com/.well-known/oauth-protected-resource/mcp"
        );
        assert!(
            route
                .body
                .contains("\"resource\":\"https://memory.example.com/mcp\"")
        );
        assert!(
            route
                .body
                .contains("\"authorization_servers\":[\"https://auth.example.com\"]")
        );
        assert!(
            route
                .body
                .contains("\"scopes_supported\":[\"memory.read\",\"memory.write\"]")
        );
    }

    #[test]
    fn public_url_must_point_at_mcp_endpoint() {
        let mut args = http_args();
        args.public_url = Some("https://memory.example.com/other".to_string());

        let error = endpoint_url(&args).expect_err("invalid path");

        assert!(
            error
                .to_string()
                .contains("public MCP URL path must be /mcp")
        );
    }

    #[test]
    fn public_url_must_not_include_query_or_fragment() {
        let mut args = http_args();
        args.public_url = Some("https://memory.example.com/mcp?token=bad".to_string());

        let error = endpoint_url(&args).expect_err("query rejected");

        assert!(
            error
                .to_string()
                .contains("public MCP URL must not include a query string or fragment")
        );
    }

    #[test]
    fn oauth_metadata_requires_bearer_auth() {
        let mut args = http_args();
        args.bearer_token_env = None;
        args.oauth_issuers = vec!["https://auth.example.com".to_string()];

        let error = oauth_metadata(&args).expect_err("auth required");

        assert!(
            error
                .to_string()
                .contains("--oauth-issuer requires --bearer-token-env")
        );
    }

    #[test]
    fn oauth_scopes_require_an_issuer() {
        let mut args = http_args();
        args.oauth_scopes = vec!["memory.read".to_string()];

        let error = oauth_metadata(&args).expect_err("issuer required");

        assert!(
            error
                .to_string()
                .contains("--oauth-scope requires at least one --oauth-issuer")
        );
    }
}
