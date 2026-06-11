//! MCP server command execution.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use aionforge::Id;
use aionforge_mcp::{
    AionforgeAuthenticatedStreamableHttpService, BearerAuthChallenge, BearerToken,
    BearerTokenCredential, BearerTokenSet, OAuthProtectedResourceMetadata,
    STREAMABLE_HTTP_ENDPOINT, StreamableHttpOptions, oauth_protected_resource_well_known_path,
    serve_stdio, streamable_http_service_with_auth_challenge,
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

    let token_set = bearer_token_set(&args.bearer_token_agent_env)?;
    let service = streamable_http_service_with_auth(memory, options, token_set, oauth_challenge)?;
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
struct HttpMcpRouter {
    inner: AionforgeAuthenticatedStreamableHttpService<RuntimeEmbedder>,
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
        self.inner.call(request).await
    }
}

fn streamable_http_service_with_auth(
    memory: Arc<aionforge::Memory<RuntimeEmbedder>>,
    options: StreamableHttpOptions,
    tokens: BearerTokenSet,
    oauth_challenge: Option<BearerAuthChallenge>,
) -> Result<AionforgeAuthenticatedStreamableHttpService<RuntimeEmbedder>, CliError> {
    Ok(streamable_http_service_with_auth_challenge(
        memory,
        options,
        tokens,
        oauth_challenge.unwrap_or_default(),
    )?)
}

fn bearer_token_set(raw_specs: &[String]) -> Result<BearerTokenSet, CliError> {
    bearer_token_set_with_env(raw_specs, |name| std::env::var(name))
}

fn bearer_token_set_with_env(
    raw_specs: &[String],
    mut env: impl FnMut(&str) -> Result<String, std::env::VarError>,
) -> Result<BearerTokenSet, CliError> {
    if raw_specs.is_empty() {
        return Err(CliError::Serve(
            "serve http requires at least one --bearer-token-agent-env AGENT_ID_ENV=TOKEN_ENV"
                .to_string(),
        ));
    }
    let mut credentials = Vec::with_capacity(raw_specs.len());
    for (index, raw) in raw_specs.iter().enumerate() {
        let (raw_agent_env, raw_token_env) = raw.split_once('=').ok_or_else(|| {
            CliError::Serve(format!(
                "--bearer-token-agent-env[{index}] must be AGENT_ID_ENV=TOKEN_ENV"
            ))
        })?;
        let agent_env = raw_agent_env.trim();
        if agent_env.is_empty() {
            return Err(CliError::Serve(format!(
                "--bearer-token-agent-env[{index}] agent env name cannot be blank"
            )));
        }
        let token_env = raw_token_env.trim();
        if token_env.is_empty() {
            return Err(CliError::Serve(format!(
                "--bearer-token-agent-env[{index}] token env name cannot be blank"
            )));
        }
        let raw_agent_id = env(agent_env).map_err(|_| {
            CliError::Serve(format!(
                "agent id environment variable {agent_env} is not set"
            ))
        })?;
        let agent_id = Id::parse(raw_agent_id.trim()).map_err(|_| {
            CliError::Serve(format!(
                "agent id environment variable {agent_env} must contain a UUID"
            ))
        })?;
        let token = env(token_env).map_err(|_| {
            CliError::Serve(format!(
                "bearer token environment variable {token_env} is not set"
            ))
        })?;
        credentials.push(BearerTokenCredential::agent(
            BearerToken::new(token).map_err(|error| CliError::Serve(error.to_string()))?,
            agent_id,
        ));
    }
    Ok(BearerTokenSet::new(credentials)?)
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
            bearer_token_agent_env: vec!["AIONFORGE_AGENT_ID=AIONFORGE_MCP_TOKEN".to_string()],
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

    #[test]
    fn bearer_token_set_requires_at_least_one_principal_spec() {
        let error = bearer_token_set(&[]).expect_err("token specs required");

        assert!(
            error
                .to_string()
                .contains("serve http requires at least one --bearer-token-agent-env")
        );
    }

    #[test]
    fn bearer_token_set_rejects_malformed_principal_specs() {
        for (spec, message) in [
            (
                "AIONFORGE_AGENT_ID",
                "--bearer-token-agent-env[0] must be AGENT_ID_ENV=TOKEN_ENV",
            ),
            (
                " =AIONFORGE_MCP_TOKEN",
                "--bearer-token-agent-env[0] agent env name cannot be blank",
            ),
            (
                "AIONFORGE_AGENT_ID= ",
                "--bearer-token-agent-env[0] token env name cannot be blank",
            ),
        ] {
            let error = bearer_token_set(&[spec.to_string()]).expect_err("spec rejected");

            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn bearer_token_set_reports_missing_agent_env_before_token_env() {
        let error = bearer_token_set(&[
            "AIONFORGE_TEST_MISSING_AGENT=AIONFORGE_TEST_MISSING_TOKEN".to_string(),
        ])
        .expect_err("missing agent env rejected");

        assert!(
            error
                .to_string()
                .contains("agent id environment variable AIONFORGE_TEST_MISSING_AGENT is not set")
        );
    }

    #[test]
    fn bearer_token_set_reads_agent_and_token_envs() {
        let set = bearer_token_set_with_env(
            &["AIONFORGE_TEST_AGENT_ID=AIONFORGE_TEST_AGENT_TOKEN".to_string()],
            |name| match name {
                "AIONFORGE_TEST_AGENT_ID" => Ok("018f0cc0-40f3-7cc4-b8b4-9ca41f88d012".to_string()),
                "AIONFORGE_TEST_AGENT_TOKEN" => Ok("test-secret".to_string()),
                _ => Err(std::env::VarError::NotPresent),
            },
        )
        .expect("token set");

        assert_eq!(set.len(), 1);
    }

    #[test]
    fn bearer_token_set_rejects_agent_env_with_non_uuid_value() {
        let error = bearer_token_set_with_env(
            &["AIONFORGE_TEST_BAD_AGENT_ID=AIONFORGE_TEST_AGENT_TOKEN".to_string()],
            |name| match name {
                "AIONFORGE_TEST_BAD_AGENT_ID" => Ok("not-a-uuid".to_string()),
                "AIONFORGE_TEST_AGENT_TOKEN" => Ok("test-secret".to_string()),
                _ => Err(std::env::VarError::NotPresent),
            },
        )
        .expect_err("bad agent id rejected");

        assert!(error.to_string().contains(
            "agent id environment variable AIONFORGE_TEST_BAD_AGENT_ID must contain a UUID"
        ));
    }
}
