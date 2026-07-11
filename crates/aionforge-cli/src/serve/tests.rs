use std::future::Future;
use std::path::PathBuf;

use aionforge::{
    CaptureRequest, CaptureVerdict, EmbedderModel, Embedding, Id, MemoryConfig, Role, Timestamp,
    WriterContext,
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
        Memory::open_in_memory(FakeEmbedder::new(), &now(), memory_config).expect("open memory"),
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
    let service = aionforge_mcp::streamable_http_service_with_consolidation(
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

    let router = http_router(runtime_http_state(&config));
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
