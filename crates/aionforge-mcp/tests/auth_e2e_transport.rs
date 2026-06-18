//! End-to-end transport test for the PR5 auth-on path through the REAL stack.
//!
//! This is the true through-line the LOCKED CONTRACT demands (item 1 "PROVE the through-line",
//! item 4 "Confirm it composes with the four may_surface_system sites"). It drives the real
//! transport — [`streamable_http_service`] is [`RequestBodyLimitService`] wrapping rmcp's
//! `StreamableHttpService` — by:
//!
//! 1. building a [`Memory`] with the INSTALLED [`OperatorAwareAuthorizer`] (the auth-on authorizer
//!    `open_memory` wires when `auth.enabled`), and seeding a `Role::System` episode;
//! 2. constructing the service with [`AuthPosture::enabled`] (so the resolver posture is auth-ON,
//!    requiring the validated extension — the reject-on-absent path);
//! 3. inserting a [`ValidatedPrincipal`] into the HTTP request's `http::request::Parts.extensions`
//!    EXACTLY as the CLI Axum `/mcp` producer does (`request.extensions_mut().insert(validated)`),
//!    then calling `.handle(request)`;
//! 4. asserting the `read_memory` tool — reached only through rmcp carrying the whole `Parts` into
//!    its `model::Extensions` bag and PR4's two-level `validated_principal_from_extensions` reading
//!    it back — resolves the inserted identity AND that the operator surfaces `system` content
//!    while a non-operator does not.
//!
//! It also pins the auth-ON / extension-ABSENT outage shape: with no validated extension every
//! identity-resolving tool fails closed with `ERR_PRINCIPAL_REQUIRED` (fail-closed, not bypass).
//!
//! A future rmcp bump that changes the carry shape — the exact class of regression that bit PR4 —
//! breaks this test instead of silently bricking auth-on at runtime. No secret or key is involved:
//! the validated principal is constructed directly (token validation has its own fixtures in
//! `auth_validator.rs`); this test isolates the transport carry + authorizer composition.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::authz::{DefaultAuthorizer, OperatorAwareAuthorizer, Principal};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{
    AuthPosture, StreamableHttpOptions, TokenClass, ValidatedPrincipal, WritePosture,
    streamable_http_service,
};
use aionforge_store::{Store, StoreConfig};
use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE, HOST};
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use serde_json::{Value, json};

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
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

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

/// A memory whose authority is the auth-ON [`OperatorAwareAuthorizer`] wrapping the default — the
/// exact authorizer the CLI's `open_memory` installs when `auth.enabled`. Built via the
/// `Memory::with_authorizer` seam (the same seam `open_memory` uses), so this proves the INSTALLED
/// authorizer, not a stand-in.
fn operator_aware_memory() -> Arc<Memory<FakeEmbedder>> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&now()).expect("migrate store");
    Arc::new(
        Memory::with_authorizer(
            Arc::new(store),
            FakeEmbedder::new(),
            MemoryConfig::default(),
            Arc::new(OperatorAwareAuthorizer::new(DefaultAuthorizer)),
            &now(),
        )
        .expect("open memory with the operator-aware authority"),
    )
}

/// Seed one `Role::System` episode straight into the store (the Capturer refuses system writes), in
/// the given agent's private namespace. Returns its id.
fn seed_system(memory: &Memory<FakeEmbedder>, content: &str, agent: Id) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace: Namespace::Agent(agent.to_string()),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: now(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::System,
        captured_at: now(),
        agent_id: agent,
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("finite")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    memory
        .store()
        .insert_episode(&episode)
        .expect("seed system episode");
    id
}

/// The auth-ON streamable-http service over `memory`: stateless + json so `.handle(..)` returns the
/// tool response inline, and [`AuthPosture::enabled`] so the resolver posture requires the validated
/// extension (the PR5 behavior flip).
fn auth_on_service(
    memory: Arc<Memory<FakeEmbedder>>,
) -> aionforge_mcp::AionforgeStreamableHttpService<FakeEmbedder> {
    let options = StreamableHttpOptions::default()
        .with_stateful_mode(false)
        .with_json_response(true);
    let posture = AuthPosture::enabled(vec!["https://issuer.example/".to_string()]);
    streamable_http_service(memory, options, posture).expect("auth-on service builds")
}

/// A `tools/call read_memory` request for one id, carrying NO body identity (the validated
/// extension is authoritative under auth-ON). Mirrors the wire shape the transport tests use.
fn read_memory_request(id: Id, include_system: bool) -> Request<Full<Bytes>> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "read_memory",
            "arguments": {
                "memory_ids": [id.to_string()],
                "include_system": include_system,
            }
        }
    });
    Request::builder()
        .method(Method::POST)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .header(HOST, "localhost:3918")
        .header("MCP-Protocol-Version", "2025-03-26")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("valid read_memory request")
}

/// Insert the `ValidatedPrincipal` into the request's `http::request::Parts.extensions` EXACTLY as
/// the CLI Axum `/mcp` producer does (`request.extensions_mut().insert(validated)`), so the rest
/// of the real transport stack carries it. This is the producer's load-bearing line, reproduced.
fn insert_validated(request: &mut Request<Full<Bytes>>, validated: ValidatedPrincipal) {
    request.extensions_mut().insert(validated);
}

/// Extract the `read_memory` tool text out of the JSON-RPC tools/call response body. A tool `Err`
/// (e.g. `ERR_PRINCIPAL_REQUIRED`) rides the same `result.content[0].text` with `result.isError`
/// set, so this resolves both the success and the fail-closed shapes.
async fn tool_text(
    response: http::Response<http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>>,
) -> String {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let parsed: Value = serde_json::from_slice(&body).expect("json-rpc body");
    parsed["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool text present in {parsed}"))
        .to_string()
}

#[tokio::test]
async fn an_operator_validated_principal_surfaces_system_through_the_real_transport() {
    let memory = operator_aware_memory();
    let agent = Id::generate();
    let id = seed_system(&memory, "a privileged system directive", agent);

    let service = auth_on_service(Arc::clone(&memory));

    // An OPERATOR validated principal, inserted at the producer's exact site. The operator bit is
    // set via the server-only constructor, exactly as the claims mapper would for a token carrying
    // the issuer's operator_permission.
    let operator = ValidatedPrincipal::new(
        Principal::with_operator(agent, Vec::new()),
        WritePosture::ReadOnly,
        TokenClass::Spa,
    );
    let mut request = read_memory_request(id, true);
    insert_validated(&mut request, operator);

    let response = service.handle(request).await;
    let text = tool_text(response).await;

    // The whole chain fired: producer insert -> RequestBodyLimitService -> StreamableHttpService
    // carries Parts into the rmcp bag -> read_memory reads it back two-level -> resolve_reader
    // keeps the operator bit -> the INSTALLED OperatorAwareAuthorizer.may_surface_system lifts the
    // gate (include_system AND operator) -> the system turn surfaces.
    assert!(
        text.starts_with("[read_memory] requested=1 found=1"),
        "the operator surfaces the system episode through the real transport: {text}"
    );
    assert!(
        text.contains("a privileged system directive"),
        "the system content is revealed to the operator: {text}"
    );
}

#[tokio::test]
async fn a_non_operator_validated_principal_does_not_surface_system_through_the_real_transport() {
    let memory = operator_aware_memory();
    let agent = Id::generate();
    let id = seed_system(&memory, "a privileged system directive", agent);

    let service = auth_on_service(Arc::clone(&memory));

    // A NON-operator validated principal: same identity, no operator bit. Even with
    // include_system=true the AND gate at the read site stays closed (may_surface_system is false),
    // so the system turn is hidden. Identical request shape — only the operator bit differs.
    let regular = ValidatedPrincipal::new(
        Principal::agent(agent),
        WritePosture::ReadOnly,
        TokenClass::Spa,
    );
    let mut request = read_memory_request(id, true);
    insert_validated(&mut request, regular);

    let response = service.handle(request).await;
    let text = tool_text(response).await;

    assert!(
        text.starts_with("[read_memory] requested=1 found=0"),
        "a non-operator does NOT surface the system episode: {text}"
    );
    assert!(
        !text.contains("a privileged system directive"),
        "the system content stays hidden from a non-operator: {text}"
    );
}

#[tokio::test]
async fn auth_on_with_no_validated_extension_rejects_every_tool_through_the_real_transport() {
    // The auth-ON / extension-ABSENT outage shape, end-to-end: with the posture enabled and NO
    // ValidatedPrincipal inserted (e.g. a request that somehow bypassed the producer), the resolver
    // fails CLOSED with ERR_PRINCIPAL_REQUIRED — never a silent downgrade to a body identity.
    let memory = operator_aware_memory();
    let agent = Id::generate();
    let id = seed_system(&memory, "a privileged system directive", agent);

    let service = auth_on_service(memory);
    let request = read_memory_request(id, true); // no extension inserted

    let response = service.handle(request).await;
    let text = tool_text(response).await;
    assert!(
        text.contains("ERR_PRINCIPAL_REQUIRED"),
        "auth-on with no validated extension fails closed: {text}"
    );
}
