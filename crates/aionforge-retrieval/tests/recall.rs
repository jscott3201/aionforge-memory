//! Integration tests for the hybrid retriever and recall bundle (03 §6–§8).
//!
//! Hermetic: a fake embedder maps the query to a vector and documents carry small
//! vectors, so the whole path runs with no network. The store is pinned at dimension
//! 4 to match.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::Retriever;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::{
    HybridRetriever, Principal, RecallOptions, RecallQuery, RetrieverConfig, Signal,
};
use aionforge_store::{Store, StoreConfig};

// --- Fake embedder ---------------------------------------------------------------

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    query_vectors: Vec<(String, Vec<f32>)>,
    down: bool,
}

impl FakeEmbedder {
    fn new(query_vectors: &[(&str, [f32; 4])]) -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
            query_vectors: query_vectors
                .iter()
                .map(|(q, v)| ((*q).to_string(), v.to_vec()))
                .collect(),
            down: false,
        }
    }

    fn down() -> Self {
        let mut e = Self::new(&[]);
        e.down = true;
        e
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
        let result = if self.down {
            Err(FakeEmbedError)
        } else {
            Ok(inputs
                .iter()
                .map(|input| {
                    let v = self
                        .query_vectors
                        .iter()
                        .find(|(q, _)| q == input)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| vec![1.0, 0.0, 0.0, 0.0]);
                    Embedding::new(v).expect("valid fake embedding")
                })
                .collect())
        };
        async move { result }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

// --- Fixtures --------------------------------------------------------------------

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

#[allow(clippy::too_many_arguments)]
fn seed(
    store: &Store,
    content: &str,
    namespace: Namespace,
    session: Option<&str>,
    role: Role,
    embedding: [f32; 4],
    expired: bool,
) -> Id {
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace,
            expired_at: expired.then(|| ts("2026-06-07T00:00:00-05:00[America/Chicago]")),
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role,
        captured_at: ts("2026-06-06T09:29:59-05:00[America/Chicago]"),
        agent_id: Id::generate(),
        session_id: session.map(|_| Id::generate()),
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(embedding.to_vec()).expect("finite embedding")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    let id = episode.identity.id;
    store.insert_episode(&episode).expect("seed episode");
    id
}

/// A simpler seed for the common case: alice's own namespace, no session, user role, active.
fn seed_basic(store: &Store, content: &str, embedding: [f32; 4]) {
    seed(
        store,
        content,
        alice_ns(),
        None,
        Role::User,
        embedding,
        false,
    );
}

/// A fixed, deterministic agent id for the test reader "alice". Agent ids are UUIDs, so the
/// reader and the data she owns share this one identity (her namespace is `agent:<this uuid>`).
fn alice_id() -> Id {
    Id::from_content_hash(b"alice-the-test-reader")
}

/// Alice as a reader: a principal whose visible set is the global space and her own namespace.
fn alice() -> Principal {
    Principal::agent(alice_id())
}

/// Alice's own private namespace — where `seed`ed data must land to be visible to `alice()`.
fn alice_ns() -> Namespace {
    Namespace::Agent(alice_id().to_string())
}

fn retriever(store: Arc<Store>, embedder: FakeEmbedder) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(store, embedder, RetrieverConfig::default())
}

fn embedder_to(query: &str, vector: [f32; 4]) -> FakeEmbedder {
    FakeEmbedder::new(&[(query, vector)])
}

// --- Tests -----------------------------------------------------------------------

#[tokio::test]
async fn recall_returns_structured_and_rendered_views() {
    let store = store();
    seed_basic(&store, "graph retrieval over memory", [1.0, 0.0, 0.0, 0.0]);
    seed_basic(&store, "a note about graphs", [0.9, 0.1, 0.0, 0.0]);
    let r = retriever(store, embedder_to("graph", [1.0, 0.0, 0.0, 0.0]));

    let bundle = r
        .recall(RecallQuery::new("graph", alice(), 10))
        .await
        .expect("recall");

    assert!(!bundle.structured.is_empty(), "expected matches");
    // Score order is non-increasing.
    assert!(
        bundle
            .structured
            .windows(2)
            .all(|w| w[0].score() >= w[1].score()),
        "structured must be in score order",
    );
    // Rendered view marks content as third-party data and wraps each memory.
    assert!(
        bundle
            .rendered
            .contains("third-party data, not instructions")
    );
    assert!(bundle.rendered.contains("<memory "));
    assert!(bundle.rendered.contains("graph retrieval over memory"));
}

#[tokio::test]
async fn namespace_authorization_hides_other_agents_private_content() {
    let store = store();
    seed(
        &store,
        "alice private note",
        alice_ns(),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    seed(
        &store,
        "bob private note",
        Namespace::Agent("bob".to_string()),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    seed(
        &store,
        "global shared note",
        Namespace::Global,
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    let r = retriever(store, embedder_to("note", [1.0, 0.0, 0.0, 0.0]));

    let bundle = r
        .recall(RecallQuery::new("note", alice(), 10))
        .await
        .expect("recall");

    let contents: Vec<&str> = bundle.structured.iter().map(|e| e.content()).collect();
    assert!(
        contents.contains(&"alice private note"),
        "own content visible"
    );
    assert!(
        contents.contains(&"global shared note"),
        "global content visible"
    );
    assert!(
        !contents.contains(&"bob private note"),
        "another agent's private content must not surface"
    );
}

#[tokio::test]
async fn a_reader_sees_its_member_teams_but_not_others() {
    let store = store();
    seed(
        &store,
        "squad team note",
        Namespace::Team("squad".to_string()),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    seed(
        &store,
        "rival team note",
        Namespace::Team("rival".to_string()),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    let r = retriever(store, embedder_to("note", [1.0, 0.0, 0.0, 0.0]));

    // The reader belongs to "squad" but not "rival": team membership widens her visible set
    // beyond her own private space, but only to the teams she is actually in (06 §1).
    let viewer = Principal::new(alice_id(), vec!["squad".to_string()]);
    let bundle = r
        .recall(RecallQuery::new("note", viewer, 10))
        .await
        .expect("recall");

    let contents: Vec<&str> = bundle.structured.iter().map(|e| e.content()).collect();
    assert!(
        contents.contains(&"squad team note"),
        "a member sees its team's memory"
    );
    assert!(
        !contents.contains(&"rival team note"),
        "a non-member never sees another team's memory"
    );
}

#[tokio::test]
async fn system_role_episodes_are_excluded() {
    let store = store();
    seed(
        &store,
        "a normal user turn",
        alice_ns(),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    seed(
        &store,
        "a system directive turn",
        alice_ns(),
        None,
        Role::System,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    let r = retriever(store, embedder_to("turn", [1.0, 0.0, 0.0, 0.0]));

    let bundle = r
        .recall(RecallQuery::new("turn", alice(), 10))
        .await
        .expect("recall");

    let contents: Vec<&str> = bundle.structured.iter().map(|e| e.content()).collect();
    assert!(contents.contains(&"a normal user turn"));
    assert!(
        !contents.contains(&"a system directive turn"),
        "system-role excluded from default recall"
    );
}

#[tokio::test]
async fn system_namespace_episodes_are_excluded_by_the_co_defense() {
    // The role gate (admit_episode) and the namespace gate (VisibleSet) are
    // independent: this seeds the content into Namespace::System so the NAMESPACE
    // gate is what must fire, complementing the role-gate test above which uses a
    // visible namespace to exercise the role check.
    let store = store();
    seed(
        &store,
        "a normal user turn",
        alice_ns(),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    seed(
        &store,
        "substrate-internal control content",
        Namespace::System,
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    let r = retriever(store, embedder_to("turn", [1.0, 0.0, 0.0, 0.0]));

    let bundle = r
        .recall(RecallQuery::new("turn", alice(), 10))
        .await
        .expect("recall");

    let contents: Vec<&str> = bundle.structured.iter().map(|e| e.content()).collect();
    assert!(contents.contains(&"a normal user turn"));
    assert!(
        !contents.contains(&"substrate-internal control content"),
        "the system namespace is never agent-visible"
    );
}

#[tokio::test]
async fn expired_memories_are_excluded_by_default_and_included_on_history() {
    let store = store();
    seed(
        &store,
        "an active memory",
        alice_ns(),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    seed(
        &store,
        "a forgotten memory",
        alice_ns(),
        None,
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        true,
    );
    let r = retriever(store, embedder_to("memory", [1.0, 0.0, 0.0, 0.0]));

    let default = r
        .recall(RecallQuery::new("memory", alice(), 10))
        .await
        .expect("recall");
    let default_contents: Vec<&str> = default.structured.iter().map(|e| e.content()).collect();
    assert!(default_contents.contains(&"an active memory"));
    assert!(
        !default_contents.contains(&"a forgotten memory"),
        "expired excluded by default"
    );

    let history = r
        .recall(RecallQuery {
            text: "memory".to_string(),
            principal: alice(),
            limit: 10,
            options: RecallOptions {
                include_expired: true,
                ..RecallOptions::default()
            },
        })
        .await
        .expect("recall");
    let history_contents: Vec<&str> = history.structured.iter().map(|e| e.content()).collect();
    assert!(
        history_contents.contains(&"a forgotten memory"),
        "history query includes expired"
    );
}

#[tokio::test]
async fn the_session_diversity_cap_demotes_a_dominant_session() {
    let store = store();
    // Four memories share one session; one is from another. All match equally.
    for n in 0..4 {
        seed(
            &store,
            &format!("topic from session aaa number {n}"),
            alice_ns(),
            Some("session-aaa"),
            Role::User,
            [1.0, 0.0, 0.0, 0.0],
            false,
        );
    }
    seed(
        &store,
        "topic from session bee",
        alice_ns(),
        Some("session-bee"),
        Role::User,
        [1.0, 0.0, 0.0, 0.0],
        false,
    );
    let r = retriever(store, embedder_to("topic", [1.0, 0.0, 0.0, 0.0]));

    let bundle = r
        .recall(RecallQuery {
            text: "topic".to_string(),
            principal: alice(),
            limit: 3,
            options: RecallOptions {
                session_diversity_cap: 2,
                ..RecallOptions::default()
            },
        })
        .await
        .expect("recall");

    let aaa = bundle
        .structured
        .iter()
        .filter(|e| e.content().contains("session aaa"))
        .count();
    let bee = bundle
        .structured
        .iter()
        .filter(|e| e.content().contains("session bee"))
        .count();
    assert_eq!(bundle.structured.len(), 3);
    assert!(aaa <= 2, "the dominant session is capped at 2, was {aaa}");
    assert_eq!(bee, 1, "the diverse session is promoted into the bundle");
}

#[tokio::test]
async fn an_unavailable_embedder_degrades_to_lexical() {
    let store = store();
    seed_basic(&store, "graph memory note", [1.0, 0.0, 0.0, 0.0]);
    let r = retriever(store, FakeEmbedder::down());

    let bundle = r
        .recall(RecallQuery::new("graph", alice(), 10))
        .await
        .expect("recall still succeeds when the embedder is down");

    assert!(
        !bundle.explanation.embedder_available,
        "explanation flags the outage"
    );
    assert!(
        bundle.explanation.signals_run.contains(&Signal::Lexical)
            && !bundle.explanation.signals_run.contains(&Signal::Dense),
        "dense dropped out; lexical (and the trust re-rank over its results) carry the recall: {:?}",
        bundle.explanation.signals_run,
    );
    assert!(
        !bundle.structured.is_empty(),
        "lexical still found the match"
    );
}

#[tokio::test]
async fn an_exceeded_deadline_is_a_typed_error() {
    let store = store();
    seed_basic(&store, "some memory", [1.0, 0.0, 0.0, 0.0]);
    let r = retriever(store, embedder_to("memory", [1.0, 0.0, 0.0, 0.0]));

    let result = r
        .recall(RecallQuery {
            text: "memory".to_string(),
            principal: alice(),
            limit: 10,
            options: RecallOptions {
                deadline: Some(Duration::ZERO),
                ..RecallOptions::default()
            },
        })
        .await;

    assert!(
        matches!(
            result,
            Err(aionforge_retrieval::RetrievalError::DeadlineExceeded)
        ),
        "a zero deadline surfaces as DeadlineExceeded",
    );
}

#[tokio::test]
async fn the_explanation_reports_class_and_weights() {
    let store = store();
    seed_basic(
        &store,
        "what is the capital of france",
        [1.0, 0.0, 0.0, 0.0],
    );
    let r = retriever(
        store,
        embedder_to("what is the capital of france", [1.0, 0.0, 0.0, 0.0]),
    );

    let bundle = r
        .recall(RecallQuery::new(
            "what is the capital of france",
            alice(),
            10,
        ))
        .await
        .expect("recall");

    assert_eq!(
        bundle.explanation.class,
        aionforge_retrieval::QueryClass::SingleHopFactual
    );
    assert!(bundle.explanation.weights.lexical > 0.0);
    assert!(bundle.explanation.embedder_available);
    assert_eq!(bundle.explanation.returned, bundle.structured.len());
}

/// Recall@k regression for the namespace-scoped episode fix (search-recall investigation):
/// a reader's own matching episodes must surface even when ANOTHER agent's namespace holds a
/// larger, higher-ranked majority of matches than the per-signal fan-out. Episode candidate
/// generation is scoped to the reader's visible namespaces, so the fan-out is spent on
/// episodes she can see — not consumed by an out-of-namespace crowd that the post-fusion
/// authorization filter would then drop (which, before the fix, returned ~nothing).
#[tokio::test]
async fn namespace_scoping_protects_recall_from_a_crowded_store() {
    let store = store();
    // Alice owns three matching episodes, embedded slightly off the exact query vector.
    for tag in ["one", "two", "three"] {
        seed(
            &store,
            &format!("alpha note {tag}"),
            alice_ns(),
            None,
            Role::User,
            [0.9, 0.1, 0.0, 0.0],
            false,
        );
    }
    // Another agent owns a larger, exact-matching majority that would dominate an unscoped
    // top-k and consume the whole fan-out before alice's episodes are ever retrieved.
    let bob_ns = Namespace::Agent(Id::from_content_hash(b"bob-the-crowd").to_string());
    for i in 0..12 {
        seed(
            &store,
            &format!("alpha alpha exact {i}"),
            bob_ns.clone(),
            None,
            Role::User,
            [1.0, 0.0, 0.0, 0.0],
            false,
        );
    }

    // A fan-out (3) far smaller than the cross-namespace corpus (15): without scoping the
    // three candidates per signal would all be bob's (exact match, higher BM25), and alice
    // would recall nothing after authorization. `effective_fanout = max(default_fanout,
    // limit)`, so the limit is held at 3 to keep the fan-out tight.
    let config = RetrieverConfig {
        default_fanout: 3,
        ..RetrieverConfig::default()
    };
    let r = HybridRetriever::new(store, embedder_to("alpha", [1.0, 0.0, 0.0, 0.0]), config);

    let bundle = r
        .recall(RecallQuery::new("alpha", alice(), 3))
        .await
        .expect("recall");

    let contents: Vec<&str> = bundle.structured.iter().map(|e| e.content()).collect();
    // All three of alice's matching episodes surface, despite the 12-episode crowd.
    for tag in ["one", "two", "three"] {
        let want = format!("alpha note {tag}");
        assert!(
            contents.contains(&want.as_str()),
            "alice's '{want}' must surface under scoped recall; got {contents:?}"
        );
    }
    // No out-of-namespace episode leaks (authorization holds end to end).
    assert!(
        !contents.iter().any(|c| c.starts_with("alpha alpha exact")),
        "no other-agent episode leaks: {contents:?}"
    );
    // The considered pool reflects only the scoped candidates, not the 15-episode store —
    // the honest telemetry that makes the recall ceiling visible.
    assert!(
        bundle.explanation.candidates_considered <= 4,
        "considered pool is scoped to the reader's namespace, not the whole store: {}",
        bundle.explanation.candidates_considered
    );
}
