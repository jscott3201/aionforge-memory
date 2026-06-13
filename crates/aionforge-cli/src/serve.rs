//! MCP server command execution.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use aionforge_config::ServerHttpConfig;
use aionforge_mcp::{
    AionforgeStreamableHttpService, STREAMABLE_HTTP_ENDPOINT, StreamableHttpOptions, serve_stdio,
    streamable_http_service,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoHttpBuilder;
use tokio::net::TcpListener;
use tower_service::Service;

use crate::cli::{ServeArgs, ServeTransport};
use crate::error::CliError;
use crate::host::{
    HostOptions, RuntimeEmbedder, StartupEmbedderStatus, check_startup_embedder, load_config,
    open_memory, render_startup_embedder_status,
};

type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

pub(crate) async fn run(options: &HostOptions, args: ServeArgs) -> Result<(), CliError> {
    let config = load_config(options)?;
    let memory = open_memory(&config)?;
    match args.transport {
        ServeTransport::Stdio => {
            let startup = check_startup_embedder(memory.as_ref()).await?;
            report_startup_embedder(&startup);
            serve_stdio(memory)
                .await
                .map_err(|error| CliError::Serve(error.to_string()))
        }
        ServeTransport::Http => serve_http(memory, args, &config.server).await,
    }
}

/// The Streamable HTTP settings after merging the CLI `serve http` flags over the
/// `[server]` config block. A flag wins when present; an absent flag inherits the config.
pub(crate) struct ResolvedHttpSettings {
    /// The resolved bind address.
    pub listen: SocketAddr,
    /// Whether sessions are stateful (the resolved inverse of `--stateless`).
    pub stateful: bool,
    /// The resolved Host allow-list; empty defers to the transport's loopback defaults.
    pub allowed_hosts: Vec<String>,
    /// The resolved Origin allow-list; empty defers to the transport's loopback defaults.
    pub allowed_origins: Vec<String>,
}

/// Merge the CLI `serve http` overrides over the `[server]` config block, the CLI flag
/// winning whenever it is present (fork#6, PR1.5). An absent `--listen` / `--stateless`
/// and an empty allow-list each inherit the corresponding config value, so a flag-free
/// invocation against a default config reproduces today's behavior exactly.
fn resolve_http_settings(args: &ServeArgs, http: &ServerHttpConfig) -> ResolvedHttpSettings {
    ResolvedHttpSettings {
        listen: args.listen.unwrap_or(http.listen),
        // `--stateless` is the inverse of the stored `stateful` flag: a present flag flips
        // it, an absent flag inherits config.
        stateful: match args.session.stateless() {
            Some(stateless) => !stateless,
            None => http.stateful,
        },
        allowed_hosts: if args.allowed_hosts.is_empty() {
            http.allowed_hosts.clone()
        } else {
            args.allowed_hosts.clone()
        },
        allowed_origins: if args.allowed_origins.is_empty() {
            http.allowed_origins.clone()
        } else {
            args.allowed_origins.clone()
        },
    }
}

/// Build the [`StreamableHttpOptions`] handed to the transport from the resolved settings.
///
/// Security invariant (fork#6, PR1.5): an *empty* resolved allow-list must never reach the
/// transport as an empty list. rmcp treats an empty `allowed_origins` as "Origin validation
/// disabled" (fail-open), and `into_rmcp_config` only rejects an empty *host* list — empty
/// origins pass through. So an empty resolved list leaves the corresponding
/// [`StreamableHttpOptions::default`] loopback allow-list in place (the "inherit the secure
/// default" signal) by *not* calling the `with_allowed_*` setter, rather than overwriting it
/// with the empty list. A non-empty resolved list replaces the default wholesale.
fn build_http_options(
    resolved: &ResolvedHttpSettings,
    json_response: bool,
    max_request_body_bytes: Option<usize>,
) -> StreamableHttpOptions {
    let mut options = StreamableHttpOptions::default()
        .with_stateful_mode(resolved.stateful)
        .with_json_response(json_response);
    if let Some(max_request_body_bytes) = max_request_body_bytes {
        options = options.with_max_request_body_bytes(max_request_body_bytes);
    }
    if !resolved.allowed_hosts.is_empty() {
        options = options.with_allowed_hosts(resolved.allowed_hosts.clone());
    }
    if !resolved.allowed_origins.is_empty() {
        options = options.with_allowed_origins(resolved.allowed_origins.clone());
    }
    options
}

async fn serve_http(
    memory: Arc<aionforge::Memory<RuntimeEmbedder>>,
    args: ServeArgs,
    http: &ServerHttpConfig,
) -> Result<(), CliError> {
    let resolved = resolve_http_settings(&args, http);
    let options = build_http_options(&resolved, args.json_response, args.max_request_body_bytes);

    let startup = check_startup_embedder(memory.as_ref()).await?;
    report_startup_embedder(&startup);
    let service = streamable_http_service(memory, options)?;
    let service = HttpMcpRouter { inner: service };

    let listener = TcpListener::bind(resolved.listen).await?;
    let builder = AutoHttpBuilder::new(TokioExecutor::new());
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
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
            shutdown = &mut shutdown => {
                shutdown?;
                report_shutdown_signal();
                break;
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            interrupted = tokio::signal::ctrl_c() => interrupted,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

fn report_startup_embedder(status: &StartupEmbedderStatus) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "aionforge serve: {}",
        render_startup_embedder_status(status),
    );
}

fn report_shutdown_signal() {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "aionforge serve: shutdown signal received");
}

#[derive(Clone)]
struct HttpMcpRouter {
    inner: AionforgeStreamableHttpService<RuntimeEmbedder>,
}

impl HttpMcpRouter {
    async fn call(&mut self, request: Request<Incoming>) -> Result<HttpResponse, Infallible> {
        if request.uri().path() != STREAMABLE_HTTP_ENDPOINT {
            return Ok(not_found_response());
        }
        self.inner.call(request).await
    }
}

fn not_found_response() -> HttpResponse {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from_static(b"Not Found")).boxed())
        .expect("valid not found response")
}

#[cfg(test)]
mod tests {
    use aionforge_mcp::streamable_http_config;

    use super::*;
    use crate::cli::SessionPostureArgs;

    /// A `serve http` invocation with every promoted knob absent: `listen`/`stateless`
    /// `None` and empty allow-lists, the "inherit the config" signal.
    fn empty_http_args() -> ServeArgs {
        ServeArgs {
            transport: ServeTransport::Http,
            listen: None,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            session: SessionPostureArgs::from_stateless(None),
            json_response: false,
            max_request_body_bytes: None,
        }
    }

    #[test]
    fn all_flags_absent_inherit_the_config_block() {
        // A non-default config so an inherited value is distinguishable from a flag
        // default: every resolved field must come straight off the config.
        let config = ServerHttpConfig {
            listen: "0.0.0.0:9000".parse().expect("addr"),
            allowed_hosts: vec!["console.example".into()],
            allowed_origins: vec!["https://console.example".into()],
            stateful: false,
        };

        let resolved = resolve_http_settings(&empty_http_args(), &config);

        assert_eq!(resolved.listen, config.listen, "listen inherits config");
        assert_eq!(
            resolved.stateful, config.stateful,
            "stateful inherits config"
        );
        assert_eq!(
            resolved.allowed_hosts, config.allowed_hosts,
            "hosts inherit config"
        );
        assert_eq!(
            resolved.allowed_origins, config.allowed_origins,
            "origins inherit config"
        );
    }

    #[test]
    fn every_flag_present_overrides_the_config_block() {
        // The config is the default posture; every flag is set to something different,
        // and `--stateless` (Some(true)) must flip the default `stateful: true` off.
        let config = ServerHttpConfig::default();
        let args = ServeArgs {
            listen: Some("127.0.0.1:4927".parse().expect("addr")),
            allowed_hosts: vec!["flag-host".into()],
            allowed_origins: vec!["https://flag-origin".into()],
            session: SessionPostureArgs::from_stateless(Some(true)),
            ..empty_http_args()
        };

        let resolved = resolve_http_settings(&args, &config);

        assert_eq!(
            resolved.listen,
            "127.0.0.1:4927".parse::<SocketAddr>().expect("addr"),
            "the --listen flag wins"
        );
        assert!(
            !resolved.stateful,
            "--stateless flips stateful off, overriding the config default"
        );
        assert_eq!(resolved.allowed_hosts, vec!["flag-host".to_string()]);
        assert_eq!(
            resolved.allowed_origins,
            vec!["https://flag-origin".to_string()]
        );
    }

    #[test]
    fn a_mixed_case_overrides_only_the_listen_flag() {
        // Only `--listen` is set; everything else inherits. `--stateless=false` is *not*
        // tested here — that is the every-flag case — so the absent stateless flag must
        // inherit the config's `stateful: false`.
        let config = ServerHttpConfig {
            listen: "0.0.0.0:9000".parse().expect("addr"),
            allowed_hosts: vec!["console.example".into()],
            allowed_origins: vec!["https://console.example".into()],
            stateful: false,
        };
        let args = ServeArgs {
            listen: Some("127.0.0.1:4927".parse().expect("addr")),
            ..empty_http_args()
        };

        let resolved = resolve_http_settings(&args, &config);

        assert_eq!(
            resolved.listen,
            "127.0.0.1:4927".parse::<SocketAddr>().expect("addr"),
            "the --listen flag overrides config"
        );
        assert_eq!(
            resolved.stateful, config.stateful,
            "the absent --stateless inherits config"
        );
        assert_eq!(
            resolved.allowed_hosts, config.allowed_hosts,
            "the empty host allow-list inherits config"
        );
        assert_eq!(
            resolved.allowed_origins, config.allowed_origins,
            "the empty origin allow-list inherits config"
        );
    }

    /// Security regression (fork#6, PR1.5): when BOTH the CLI flags and the config leave the
    /// allow-lists empty, the options handed to the transport must keep the secure loopback
    /// defaults, NOT an empty list. An empty `allowed_origins` would disable Origin
    /// validation in rmcp (fail-open), and `streamable_http_config` does not reject it — so
    /// `build_http_options` is the guard that must never hand it an empty list.
    #[test]
    fn empty_resolved_allow_lists_keep_the_secure_loopback_defaults() {
        // The all-default config + flag-free invocation: every resolved allow-list is empty.
        let resolved = resolve_http_settings(&empty_http_args(), &ServerHttpConfig::default());
        assert!(
            resolved.allowed_hosts.is_empty(),
            "precondition: empty hosts"
        );
        assert!(
            resolved.allowed_origins.is_empty(),
            "precondition: empty origins"
        );

        let options = build_http_options(&resolved, false, None);
        let defaults = StreamableHttpOptions::default();
        assert_eq!(
            options.allowed_hosts, defaults.allowed_hosts,
            "empty resolved hosts keep the loopback default host allow-list"
        );
        assert_eq!(
            options.allowed_origins, defaults.allowed_origins,
            "empty resolved origins keep the loopback default origin allow-list, not an empty \
             (Origin-validation-disabled) list"
        );
        assert!(
            !options.allowed_origins.is_empty(),
            "the origin allow-list reaching rmcp is never empty (no fail-open)"
        );
        // And the built options must convert into a valid rmcp config (Origin validation on).
        streamable_http_config(options).expect("default loopback options build a valid config");
    }

    /// A non-empty resolved allow-list replaces the loopback default wholesale.
    #[test]
    fn non_empty_resolved_allow_lists_replace_the_defaults() {
        let config = ServerHttpConfig {
            allowed_hosts: vec!["console.example".into()],
            allowed_origins: vec!["https://console.example".into()],
            ..ServerHttpConfig::default()
        };
        let resolved = resolve_http_settings(&empty_http_args(), &config);

        let options = build_http_options(&resolved, false, None);
        assert_eq!(options.allowed_hosts, vec!["console.example".to_string()]);
        assert_eq!(
            options.allowed_origins,
            vec!["https://console.example".to_string()]
        );
    }
}
