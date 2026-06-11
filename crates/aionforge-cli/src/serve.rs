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
    let oauth_challenge = oauth_metadata.as_ref().map(oauth_challenge);
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
    scopes: Vec<String>,
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
    let oauth_issuers = oauth_issuers(&args.oauth_issuers)?;
    let oauth_scopes = oauth_scopes(&args.oauth_scopes)?;
    if oauth_issuers.is_empty() {
        if !oauth_scopes.is_empty() {
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
    let metadata = OAuthProtectedResourceMetadata::new(endpoint_url.as_str(), &oauth_issuers)
        .with_scopes(&oauth_scopes);
    Ok(Some(OAuthMetadataRoute {
        path: metadata_path,
        resource: metadata_url,
        body: Arc::from(metadata.to_json()),
        scopes: oauth_scopes,
    }))
}

fn oauth_challenge(metadata: &OAuthMetadataRoute) -> BearerAuthChallenge {
    let mut challenge =
        BearerAuthChallenge::default().with_resource_metadata_url(metadata.resource.clone());
    if !metadata.scopes.is_empty() {
        challenge = challenge.with_scope(metadata.scopes.join(" "));
    }
    challenge
}

fn oauth_issuers(raw: &[String]) -> Result<Vec<String>, CliError> {
    let issuers = normalized_non_blank("--oauth-issuer", raw)?;
    for (index, issuer) in issuers.iter().enumerate() {
        let url = Url::parse(issuer).map_err(|error| {
            CliError::Serve(format!(
                "--oauth-issuer[{index}] is not a valid URL: {error}"
            ))
        })?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err(CliError::Serve(format!(
                "--oauth-issuer[{index}] must not include a query string or fragment"
            )));
        }
        if url.scheme() == "https" {
            continue;
        }
        if url.scheme() == "http" && is_loopback_url(&url) {
            continue;
        }
        return Err(CliError::Serve(format!(
            "--oauth-issuer[{index}] must use https unless it is an http loopback URL"
        )));
    }
    Ok(issuers)
}

fn oauth_scopes(raw: &[String]) -> Result<Vec<String>, CliError> {
    let scopes = normalized_non_blank("--oauth-scope", raw)?;
    for (index, scope) in scopes.iter().enumerate() {
        if scope
            .chars()
            .any(|ch| ch.is_ascii_whitespace() || ch.is_ascii_control() || ch == '"' || ch == '\\')
        {
            return Err(CliError::Serve(format!(
                "--oauth-scope[{index}] must be one scope token without whitespace, quotes, or backslashes"
            )));
        }
    }
    Ok(scopes)
}

fn normalized_non_blank(flag: &str, raw: &[String]) -> Result<Vec<String>, CliError> {
    raw.iter()
        .enumerate()
        .map(|(index, value)| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(CliError::Serve(format!("{flag}[{index}] cannot be blank")));
            }
            Ok(trimmed.to_string())
        })
        .collect()
}

fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let ip_host = host
        .strip_prefix('[')
        .and_then(|stripped| stripped.strip_suffix(']'))
        .unwrap_or(host);
    ip_host
        .parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
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
    fn oauth_metadata_normalizes_inputs_for_metadata_and_challenge() {
        let mut args = http_args();
        args.public_url = Some("https://memory.example.com/mcp".to_string());
        args.oauth_issuers = vec![" https://auth.example.com/issuer ".to_string()];
        args.oauth_scopes = vec![" memory.read ".to_string()];

        let route = oauth_metadata(&args)
            .expect("metadata builds")
            .expect("metadata enabled");

        assert!(
            route
                .body
                .contains("\"authorization_servers\":[\"https://auth.example.com/issuer\"]")
        );
        assert!(
            route
                .body
                .contains("\"scopes_supported\":[\"memory.read\"]")
        );
        assert_eq!(route.scopes, vec!["memory.read"]);
        assert!(
            oauth_challenge(&route)
                .header_value()
                .contains(r#"scope="memory.read""#)
        );
    }

    #[test]
    fn oauth_issuer_allows_loopback_http_for_local_development() {
        let mut args = http_args();
        args.oauth_issuers = vec![
            "http://localhost:3000/issuer".to_string(),
            "http://127.0.0.1:3000/issuer".to_string(),
            "http://[::1]:3000/issuer".to_string(),
        ];

        let route = oauth_metadata(&args)
            .expect("metadata builds")
            .expect("metadata enabled");

        assert!(route.body.contains("\"http://localhost:3000/issuer\""));
        assert!(route.body.contains("\"http://127.0.0.1:3000/issuer\""));
        assert!(route.body.contains("\"http://[::1]:3000/issuer\""));
    }

    #[test]
    fn oauth_issuer_rejects_blank_or_insecure_remote_values() {
        for (issuer, message) in [
            (" ", "--oauth-issuer[0] cannot be blank"),
            ("not a url", "--oauth-issuer[0] is not a valid URL"),
            (
                "http://auth.example.com",
                "--oauth-issuer[0] must use https unless it is an http loopback URL",
            ),
            (
                "https://auth.example.com?tenant=bad",
                "--oauth-issuer[0] must not include a query string or fragment",
            ),
        ] {
            let mut args = http_args();
            args.oauth_issuers = vec![issuer.to_string()];

            let error = oauth_metadata(&args).expect_err("issuer rejected");

            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn oauth_scope_rejects_blank_or_multi_token_values() {
        for (scope, message) in [
            (" ", "--oauth-scope[0] cannot be blank"),
            (
                "memory.read memory.write",
                "--oauth-scope[0] must be one scope token",
            ),
            ("memory\"read", "--oauth-scope[0] must be one scope token"),
            ("memory\\read", "--oauth-scope[0] must be one scope token"),
        ] {
            let mut args = http_args();
            args.oauth_issuers = vec!["https://auth.example.com".to_string()];
            args.oauth_scopes = vec![scope.to_string()];

            let error = oauth_metadata(&args).expect_err("scope rejected");

            assert!(error.to_string().contains(message), "{error}");
        }
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
