//! MCP server command execution.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aionforge::{
    ConsolidationHandle, Embedder, Memory, RuleExtractor, RuleInducer, RuleSummarizer,
};
use aionforge_config::{AuthConfig, Config, ServerHttpConfig};
use aionforge_mcp::{
    AionforgeStreamableHttpService, AuthPosture, AuthValidators, STREAMABLE_HTTP_ENDPOINT,
    StreamableHttpOptions, serve_stdio_with_consolidation,
    streamable_http_service_with_consolidation,
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
use crate::console;
use crate::error::CliError;
use crate::health::{self, VersionInfo};
use crate::host::{
    HostOptions, RuntimeEmbedder, StartupEmbedderStatus, check_startup_embedder, load_config,
    open_memory, render_startup_embedder_status,
};

type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

pub(crate) async fn run(options: &HostOptions, args: ServeArgs) -> Result<(), CliError> {
    let config = load_config(options)?;
    let memory = open_memory(&config)?;
    let consolidation_handle = start_background_consolidation(&memory, &config);
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
                serve_stdio_with_consolidation(
                    memory,
                    config.auth.enabled,
                    config.consolidation.enabled,
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

/// Environment variable overriding the traffic-heartbeat cadence, in whole seconds. `0` disables
/// the heartbeat; unset uses [`aionforge_mcp::DEFAULT_TRAFFIC_HEARTBEAT_INTERVAL`]. An env knob
/// (not a config field) keeps it operationally tunable without a schema change, mirroring the
/// `RUST_LOG` / `AIONFORGE_LOG_FORMAT` logging controls.
const TRAFFIC_HEARTBEAT_ENV: &str = "AIONFORGE_TRAFFIC_HEARTBEAT_SECS";

/// Resolve the heartbeat cadence from the env override, falling back to the compiled-in default.
/// An unparseable value falls back too — observability setup must never fail the server. Pure, so
/// the precedence is unit-testable.
fn resolve_heartbeat_interval(env: Option<&str>) -> std::time::Duration {
    match env.and_then(|value| value.trim().parse::<u64>().ok()) {
        Some(seconds) => std::time::Duration::from_secs(seconds),
        None => aionforge_mcp::DEFAULT_TRAFFIC_HEARTBEAT_INTERVAL,
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

    let service = streamable_http_service_with_consolidation(
        memory,
        options,
        auth_posture,
        config.consolidation.enabled,
    )?;
    let state = HttpMcpState {
        inner: service,
        validators: validators.map(Arc::new),
        version: Arc::new(VersionInfo::from_config(config)),
    };
    let console_dist = console::resolve_dist_dir();
    console::report_startup(console_dist.as_deref());

    let listener = TcpListener::bind(resolved.listen).await?;
    let app = http_router(state, console_dist);
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
/// well-known metadata route. When a built console asset directory is present, `/console` serves
/// the SvelteKit static SPA from that directory without letting the SPA fallback catch `/mcp` or
/// the OAuth well-known path. The `/mcp` handler is the PR5 validator producer: it extracts and
/// validates the Bearer token, maps the claims to a principal, and inserts the
/// [`ValidatedPrincipal`](aionforge_mcp::ValidatedPrincipal) into the request's
/// `http::request::Parts.extensions` — the two-level nesting PR4 reads back downstream. When
/// `validators` is `None` (the DEFAULT-OFF path), `/mcp` delegates straight to the inner service
/// and every other path 404s, with no validation, no extension insert, and no well-known route.
fn http_router(state: HttpMcpState, console_dist: Option<PathBuf>) -> Router {
    let mut router = console::mount(
        Router::new()
            .route("/livez", get(health::livez_handler))
            .route("/version", get(version_handler))
            .route(STREAMABLE_HTTP_ENDPOINT, any(mcp_handler))
            .fallback(not_found_handler),
        console_dist,
    );
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
mod tests {
    use std::future::Future;

    use aionforge::{
        CaptureRequest, CaptureVerdict, EmbedderModel, Embedding, Id, MemoryConfig, Role,
        Timestamp, WriterContext,
    };
    use aionforge_mcp::streamable_http_config;
    use aionforge_store::{BoundQuery, QueryResult};
    use axum::http::header::CONTENT_TYPE;
    use tower::ServiceExt;

    use super::*;
    use crate::cli::SessionPostureArgs;
    use crate::host::open_memory;

    #[derive(Clone)]
    struct FakeEmbedder {
        model: EmbedderModel,
    }

    impl FakeEmbedder {
        fn new() -> Self {
            Self {
                model: EmbedderModel {
                    family: "fake".to_string(),
                    version: "1".to_string(),
                    dimension: 4,
                },
            }
        }
    }

    #[derive(Debug)]
    struct NeverFails;

    impl std::fmt::Display for NeverFails {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unreachable")
        }
    }

    impl std::error::Error for NeverFails {}

    impl Embedder for FakeEmbedder {
        type Error = NeverFails;

        fn embed(
            &self,
            inputs: &[String],
        ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
            let out = inputs
                .iter()
                .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
                .collect();
            async move { Ok(out) }
        }

        fn model(&self) -> &EmbedderModel {
            &self.model
        }
    }

    fn now() -> Timestamp {
        "2026-06-06T09:30:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime")
    }

    fn test_memory_with_config(config: &Config) -> Arc<Memory<FakeEmbedder>> {
        let (consolidation, pass) = crate::consolidation_config::consolidation_settings(config);
        let memory_config = MemoryConfig {
            consolidation,
            pass,
            ..MemoryConfig::default()
        };
        Arc::new(
            Memory::open_in_memory(FakeEmbedder::new(), &now(), memory_config)
                .expect("open memory"),
        )
    }

    fn fact_count(memory: &Memory<FakeEmbedder>) -> usize {
        let query = BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id");
        match memory.store().execute(&query).expect("fact count query") {
            QueryResult::Rows(rows) => rows.row_count(),
            _ => 0,
        }
    }

    async fn capture_svo(memory: &Memory<FakeEmbedder>) {
        let receipt = memory
            .capture(CaptureRequest {
                content: "Alice uses Rust.".to_string(),
                role: Role::User,
                agent_id: Id::generate(),
                teams: Vec::new(),
                session_id: None,
                captured_at: now(),
                ingested_at: now(),
                writer: WriterContext {
                    model_family: "host".to_string(),
                    model_version: None,
                    transport: None,
                    request_id: None,
                    trust: 0.9,
                    signed: None,
                },
                trusted: false,
                namespace: None,
                supersedes: None,
            })
            .await
            .expect("capture");
        assert_eq!(receipt.verdict, CaptureVerdict::New);
    }

    #[tokio::test]
    async fn background_consolidation_default_off_does_not_start() {
        let config = Config::default();
        let memory = test_memory_with_config(&config);
        capture_svo(memory.as_ref()).await;

        let handle = start_background_consolidation(&memory, &config);

        assert!(
            handle.is_none(),
            "default consolidation.enabled=false must not start a background loop"
        );
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        assert_eq!(
            fact_count(memory.as_ref()),
            0,
            "without the background loop, raw episodes wait for an explicit consolidate call"
        );
    }

    #[tokio::test]
    async fn background_consolidation_enabled_derives_fact_and_shuts_down() {
        let mut config = Config::default();
        config.consolidation.enabled = true;
        config.consolidation.tick_interval_secs = 1;
        let memory = test_memory_with_config(&config);
        capture_svo(memory.as_ref()).await;

        let handle = start_background_consolidation(&memory, &config)
            .expect("enabled config starts the background loop");

        let mut derived = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if fact_count(memory.as_ref()) >= 1 {
                derived = true;
                break;
            }
        }
        handle.shutdown().await;

        assert!(
            derived,
            "the serve-owned background consolidator derived a fact without a tool call"
        );
    }

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

    fn unique_dir(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create test data dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("restrict test data dir");
        }
        path
    }

    fn runtime_http_state(config: &Config) -> HttpMcpState {
        let memory = open_memory(config).expect("open runtime memory");
        let service = streamable_http_service_with_consolidation(
            memory,
            StreamableHttpOptions::default(),
            AuthPosture::disabled(),
            config.consolidation.enabled,
        )
        .expect("build streamable HTTP service");
        HttpMcpState {
            inner: service,
            validators: None,
            version: Arc::new(VersionInfo::from_config(config)),
        }
    }

    async fn router_get(router: Router, uri: &str) -> (StatusCode, Option<String>, String) {
        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| value.to_str().expect("ascii content type").to_string());
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).expect("utf-8 body");
        (status, content_type, body)
    }

    #[tokio::test]
    async fn health_routes_are_registered_outside_mcp() {
        let mut config = Config::default();
        config.persistence.data_dir = unique_dir("aionforge-health-router");
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();
        config.embedder.dimension = 4;
        config.embedder.native_dimension = Some(8);

        let router = http_router(runtime_http_state(&config), None);
        let (status, content_type, body) = router_get(router.clone(), "/livez").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("text/plain"));
        assert_eq!(body, "ok");

        let (status, content_type, body) = router_get(router, "/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        let version: serde_json::Value = serde_json::from_str(&body).expect("version JSON");
        assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(version["build_sha"], aionforge_mcp::build_sha());
        assert_eq!(version["build_status"], aionforge_mcp::build_status());
        assert_eq!(version["built_at"], aionforge_mcp::build_timestamp());
        assert_eq!(version["embedder_dimension"], 4);
        assert_eq!(version["native_dimension"], 8);

        let _ = std::fs::remove_dir_all(&config.persistence.data_dir);
    }

    #[test]
    fn stdio_auth_warning_fires_only_when_auth_is_enabled() {
        // DEFAULT-OFF: auth disabled (the default) draws no stdio advisory, so today's stdio path
        // is byte-for-byte unchanged.
        assert!(
            stdio_auth_unsupported_warning(false).is_none(),
            "auth-off stdio is silent"
        );
        // Auth-on stdio warns loudly with the actionable root cause and the actionable remedy.
        let warning = stdio_auth_unsupported_warning(true).expect("auth-on stdio warns");
        assert!(warning.contains("ERR_PRINCIPAL_REQUIRED"), "{warning}");
        assert!(warning.contains("serve http"), "{warning}");
        // Never leaks a secret — it is a fixed advisory.
        assert!(!warning.to_lowercase().contains("token="), "{warning}");
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

    #[test]
    fn heartbeat_interval_resolves_env_override_default_and_disable() {
        use std::time::Duration;
        // Unset: the compiled-in default.
        assert_eq!(
            resolve_heartbeat_interval(None),
            aionforge_mcp::DEFAULT_TRAFFIC_HEARTBEAT_INTERVAL
        );
        // A valid override (whitespace-tolerant) wins.
        assert_eq!(
            resolve_heartbeat_interval(Some(" 60 ")),
            Duration::from_secs(60)
        );
        // Zero disables (the caller checks is_zero before spawning).
        assert_eq!(resolve_heartbeat_interval(Some("0")), Duration::ZERO);
        // Garbage falls back to the default rather than failing the server.
        assert_eq!(
            resolve_heartbeat_interval(Some("soon")),
            aionforge_mcp::DEFAULT_TRAFFIC_HEARTBEAT_INTERVAL
        );
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
