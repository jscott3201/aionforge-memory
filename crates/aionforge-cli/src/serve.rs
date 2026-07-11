//! MCP server command execution.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use aionforge::{
    ConsolidationHandle, Embedder, Memory, RuleExtractor, RuleInducer, RuleSummarizer,
};
use aionforge_config::{AuthConfig, Config, ServerHttpConfig};
use aionforge_mcp::{
    AionforgeStreamableHttpService, AuthPosture, AuthValidators, MessageWaitBounds,
    RoomSubscribeBounds, STREAMABLE_HTTP_ENDPOINT, StreamableHttpOptions,
    serve_stdio_with_consolidation_and_message_wait,
    streamable_http_service_with_consolidation_and_message_wait,
};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{Method, Request, Response, StatusCode};
use axum::routing::{any, get};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use tokio::net::TcpListener;

use crate::cli::{ServeArgs, ServeTransport};
use crate::error::CliError;
use crate::health::{self, VersionInfo};
use crate::host::{
    HostOptions, RuntimeEmbedder, StartupEmbedderStatus, check_startup_embedder, load_config,
    open_memory, render_startup_embedder_status,
};
use crate::observability::{TRAFFIC_HEARTBEAT_ENV, resolve_heartbeat_interval};

type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

pub(crate) async fn run(options: &HostOptions, args: ServeArgs) -> Result<(), CliError> {
    let config = load_config(options)?;
    let memory = open_memory(&config)?;
    let consolidation_handle = start_background_consolidation(&memory, &config);
    let message_retention_handle =
        crate::message_retention::start(Arc::clone(&memory), &config.messages);
    // Periodic in/out traffic heartbeat for the server's lifetime (logging-foundation, task #9):
    // a `tracing` line every few minutes with cumulative + delta bytes/tokens in and out. Covers
    // both transports. A zero cadence disables it. Spawned BEFORE the (blocking) transport dispatch
    // so it ticks for the whole serve, and explicitly aborted on the way out (below) so shutdown is
    // deterministic rather than dependent on runtime-drop timing.
    let heartbeat =
        resolve_heartbeat_interval(std::env::var(TRAFFIC_HEARTBEAT_ENV).ok().as_deref());
    let heartbeat_task = (!heartbeat.is_zero())
        .then(|| tokio::spawn(aionforge_mcp::run_traffic_heartbeat(heartbeat)));
    // The OAuth resource-server posture is DEFAULT-OFF: `config.auth.enabled` is `false` unless a
    // deployment opts in, so the stdio and HTTP transports below reproduce today's behavior exactly.
    let result = match args.transport {
        ServeTransport::Stdio => match check_startup_embedder(memory.as_ref()).await {
            Ok(startup) => {
                report_startup_embedder(&startup);
                // stdio carries no HTTP request, so no HTTP validator can run over it; the flag is
                // threaded only for posture parity (an enabled stdio server has no producer yet).
                // Auth-on over stdio therefore rejects EVERY identity-bearing tool with
                // ERR_PRINCIPAL_REQUIRED (fail-closed, never a bypass). Warn LOUDLY at startup so the
                // operator sees the root cause as a single visible signal, not a stream of per-tool 403s.
                report_stdio_auth_unsupported(config.auth.enabled);
                serve_stdio_with_consolidation_and_message_wait(
                    memory,
                    config.auth.enabled,
                    config.consolidation.enabled,
                    MessageWaitBounds::from(&config.messages),
                    RoomSubscribeBounds::from(&config.messages),
                )
                .await
                .map_err(|error| CliError::Serve(error.to_string()))
            }
            Err(error) => Err(error),
        },
        ServeTransport::Http => serve_http(memory, args, &config).await,
    };
    // Stop the heartbeat deterministically before exit, then log the final cumulative summary.
    if let Some(task) = heartbeat_task {
        task.abort();
    }
    if let Some(handle) = message_retention_handle {
        handle.shutdown().await;
    }
    if let Some(handle) = consolidation_handle {
        handle.shutdown().await;
    }
    aionforge_mcp::log_traffic_totals("shutdown");
    result
}

fn start_background_consolidation<E: Embedder + 'static>(
    memory: &Arc<Memory<E>>,
    config: &Config,
) -> Option<ConsolidationHandle> {
    if !config.consolidation.enabled {
        tracing::info!(
            target: "aionforge::serve",
            background_managed = false,
            "background consolidation disabled; foreground consolidate tool remains available",
        );
        if let Some(advisory) = config.consolidation.master_switch_advisory() {
            tracing::warn!(target: "aionforge::serve", "{advisory}");
        }
        return None;
    }

    tracing::info!(
        target: "aionforge::serve",
        background_managed = true,
        tick_interval_secs = config.consolidation.tick_interval_secs,
        batch_size = config.consolidation.batch_size,
        "background consolidation enabled; foreground consolidate tool will return ERR_CONSOLIDATE_MANAGED",
    );
    Some(memory.start_consolidation(
        RuleExtractor::with_default_rules_and_config(memory.pass_config().extraction),
        RuleSummarizer::with_default_rules(),
        RuleInducer::with_default_rules(),
        memory.consolidation_config(),
        memory.pass_config(),
    ))
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
    memory: Arc<Memory<RuntimeEmbedder>>,
    args: ServeArgs,
    config: &Config,
) -> Result<(), CliError> {
    let http: &ServerHttpConfig = &config.server;
    let resolved = resolve_http_settings(&args, http);
    let options = build_http_options(&resolved, args.json_response, args.max_request_body_bytes);

    let startup = check_startup_embedder(memory.as_ref()).await?;
    report_startup_embedder(&startup);

    // Build the OAuth resource-server producer ONCE, at startup. DEFAULT-OFF: `build` returns
    // `None` when `config.auth.enabled` is `false`, so the router below runs no validator, serves
    // no well-known route, and inserts no extension — byte-for-byte today's behavior. When enabled,
    // each issuer's JWKS is fetched here so a broken issuer fails fast at startup, not per-request.
    let validators = AuthValidators::build(&config.auth)
        .await
        .map_err(|error| CliError::Serve(error.to_string()))?;
    report_auth_startup(&config.auth, &validators);
    let auth_posture = match &validators {
        Some(validators) => AuthPosture::enabled(validators.issuer_origins().to_vec()),
        None => AuthPosture::disabled(),
    };

    let service = streamable_http_service_with_consolidation_and_message_wait(
        memory,
        options,
        auth_posture,
        config.consolidation.enabled,
        MessageWaitBounds::from(&config.messages),
        RoomSubscribeBounds::from(&config.messages),
    )?;
    let state = HttpMcpState {
        inner: service,
        validators: validators.map(Arc::new),
        version: Arc::new(VersionInfo::from_config(config)),
    };
    let listener = TcpListener::bind(resolved.listen).await?;
    let app = http_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            if let Err(error) = shutdown_signal().await {
                tracing::error!(
                    target: "aionforge::serve",
                    error = %error,
                    "shutdown signal listener failed",
                );
            }
            report_shutdown_signal();
        })
        .await?;
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
    tracing::info!(
        target: "aionforge::serve",
        status = %render_startup_embedder_status(status),
        "startup embedder check",
    );
}

fn report_shutdown_signal() {
    tracing::info!(target: "aionforge::serve", "shutdown signal received");
}

/// Warn loudly when auth is enabled on the stdio transport, where there is no HTTP producer to
/// insert a validated principal: every identity-bearing tool then fails closed with
/// `ERR_PRINCIPAL_REQUIRED`. A no-op when auth is disabled (the default), so the warning never
/// fires on today's path. Emitted at `warn` through the global tracing subscriber installed in
/// `main`; the advisory text is fixed and secret-free.
fn report_stdio_auth_unsupported(auth_enabled: bool) {
    if let Some(warning) = stdio_auth_unsupported_warning(auth_enabled) {
        tracing::warn!(target: "aionforge::serve", "{warning}");
    }
}

/// The (non-secret) stdio-auth-unsupported advisory, or `None` when auth is disabled. Pure, so the
/// no-op-when-disabled invariant and the wording are directly testable.
fn stdio_auth_unsupported_warning(auth_enabled: bool) -> Option<String> {
    if !auth_enabled {
        return None;
    }
    Some(
        "aionforge serve: WARNING auth is enabled but the stdio transport has no token-validator \
         producer; every identity-bearing tool will be rejected with ERR_PRINCIPAL_REQUIRED. Use \
         the HTTP transport (serve http) for an auth-enabled deployment."
            .to_string(),
    )
}

/// Report the OAuth resource-server posture at startup (posture only, never a secret).
///
/// Default-off prints a single "auth disabled" line; an enabled server prints the issuer count and
/// each soft config advisory (`AuthConfig::startup_warnings`, which names issuers by index, never
/// by value). No token, key, or JWKS is ever logged.
fn report_auth_startup(auth: &AuthConfig, validators: &Option<AuthValidators>) {
    match validators {
        None => {
            tracing::info!(target: "aionforge::serve", "auth disabled (default)");
        }
        Some(validators) => {
            tracing::info!(
                target: "aionforge::serve",
                issuers = validators.issuer_origins().len(),
                "auth enabled",
            );
            for warning in auth.startup_warnings() {
                tracing::warn!(target: "aionforge::serve", "auth warning: {warning}");
            }
        }
    }
}

/// Build the Axum router for MCP Streamable HTTP.
///
/// Routes `/mcp` to rmcp's Streamable HTTP service and, when auth is enabled, mounts the RFC 9728
/// well-known metadata route. The `/mcp` handler is the PR5 validator producer: it extracts and
/// validates the Bearer token, maps the claims to a principal, and inserts the
/// [`ValidatedPrincipal`](aionforge_mcp::ValidatedPrincipal) into the request's
/// `http::request::Parts.extensions` — the two-level nesting PR4 reads back downstream. When
/// `validators` is `None` (the DEFAULT-OFF path), `/mcp` delegates straight to the inner service
/// and every other path 404s, with no validation, no extension insert, and no well-known route.
fn http_router(state: HttpMcpState) -> Router {
    let mut router = Router::new()
        .route("/livez", get(health::livez_handler))
        .route("/version", get(version_handler))
        .route(STREAMABLE_HTTP_ENDPOINT, any(mcp_handler))
        .fallback(not_found_handler);
    if let Some(validators) = state.validators.as_ref() {
        router = router.route(validators.well_known_path(), any(well_known_handler));
    }
    router.with_state(state)
}

#[derive(Clone)]
struct HttpMcpState {
    inner: AionforgeStreamableHttpService<RuntimeEmbedder>,
    /// The OAuth resource-server producer, present only when `auth.enabled`. `None` is the
    /// default-off path: no validator runs, no extension is inserted, no well-known route exists.
    validators: Option<Arc<AuthValidators>>,
    /// Startup-captured, secret-free build/config snapshot served by `/version`.
    version: Arc<VersionInfo>,
}

async fn version_handler(State(state): State<HttpMcpState>) -> HttpResponse {
    health::version_response(&state.version)
}

async fn mcp_handler(
    State(state): State<HttpMcpState>,
    mut request: Request<Body>,
) -> HttpResponse {
    if let Some(validators) = state.validators {
        // Authenticate the `/mcp` request. On any failure the producer returns the secret-free
        // 401/403 response (with the WWW-Authenticate challenge) to send verbatim.
        let validated = match validators
            .authenticate(request.headers().get(AUTHORIZATION))
            .await
        {
            Ok(validated) => validated,
            Err(response) => return *response,
        };

        // THE CRUX: insert the ValidatedPrincipal into the http::request::Parts.extensions (one
        // level below the rmcp model::Extensions bag). The rmcp streamable-http transport carries
        // the WHOLE Parts into its bag as a single entry, so PR4's two-level read
        // (`extensions.get::<http::request::Parts>()` then `parts.extensions.get::<ValidatedPrincipal>()`)
        // finds it here and nowhere else — inserting into any other bag would yield None at the
        // handler and a total auth-on outage.
        request.extensions_mut().insert(validated);
    }
    state.inner.handle(request).await
}

async fn well_known_handler(
    State(state): State<HttpMcpState>,
    request: Request<Body>,
) -> HttpResponse {
    if request.method() == Method::GET
        && let Some(validators) = state.validators
    {
        validators.oauth_metadata_response()
    } else {
        not_found_response()
    }
}

async fn not_found_handler() -> HttpResponse {
    not_found_response()
}

fn not_found_response() -> HttpResponse {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from_static(b"Not Found")).boxed())
        .expect("valid not found response")
}

#[cfg(test)]
mod tests;
