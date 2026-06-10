//! M5.T04 acceptance for the identity pre-pass (05 §4): live core blocks in the
//! reader's visible set are always included, ahead of the ranked results — they
//! bypass fusion and the diversity cap, count toward the requested limit without
//! being capped themselves, carry the `always` marker instead of a score, and render
//! inside the same escaped third-party wrapper as every recalled memory.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id, SerializationId};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::{
    HybridRetriever, Principal, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
    StructuredEntry,
};
use aionforge_store::{Store, StoreConfig};

// --- Fixtures (the recall.rs hermetic pattern) -------------------------------------

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

#[derive(Debug)]
struct NeverError;

impl std::fmt::Display for NeverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("never errors")
    }
}

impl std::error::Error for NeverError {}

impl Embedder for FakeEmbedder {
    type Error = NeverError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let result = Ok(inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid embedding"))
            .collect());
        async move { result }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn embedder() -> FakeEmbedder {
    FakeEmbedder {
        model: EmbedderModel {
            family: "fake".to_string(),
            version: "1".to_string(),
            dimension: 4,
        },
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
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

fn alice_id() -> Id {
    Id::from_content_hash(b"alice-the-test-reader")
}

fn alice() -> Principal {
    Principal::agent(alice_id())
}

fn alice_ns() -> Namespace {
    Namespace::Agent(alice_id().to_string())
}

fn seed_episode(store: &Store, content: &str) {
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace: alice_ns(),
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
        role: Role::User,
        captured_at: now(),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("finite embedding")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("seed episode");
}

fn seed_core_block(
    store: &Store,
    namespace: Namespace,
    content: &str,
    kind: BlockKind,
    sensitivity: Option<&str>,
    retired: bool,
) -> Id {
    let id = Id::generate();
    let block = CoreBlock {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace: namespace.clone(),
            expired_at: retired.then(now),
        },
        stats: Stats {
            importance: 0.95,
            trust: 0.9,
            last_access: now(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        block_kind: kind,
        sensitivity: sensitivity.map(str::to_string),
        drift_baseline: None,
        embedding: None,
        embedder_model: None,
    };
    let audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(id.to_string().as_bytes()),
            ingested_at: now(),
            namespace,
            expired_at: None,
        },
        kind: AuditKind::CoreEdit,
        subject_id: id,
        actor_id: alice_id(),
        payload: serde_json::json!({"outcome": "created"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store.create_core_block(&block, &audit).expect("create");
    id
}

fn retriever(store: &Arc<Store>) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(Arc::clone(store), embedder(), RetrieverConfig::default())
}

async fn recall(store: &Arc<Store>, text: &str, limit: usize) -> RecallBundle {
    retriever(store)
        .recall(RecallQuery {
            text: text.to_string(),
            principal: alice(),
            limit,
            options: RecallOptions::default(),
        })
        .await
        .expect("recall")
}

fn core_ids(bundle: &RecallBundle) -> Vec<Id> {
    bundle
        .structured
        .iter()
        .filter_map(|e| match e {
            StructuredEntry::CoreBlock(c) => Some(c.id),
            _ => None,
        })
        .collect()
}

// --- Tests --------------------------------------------------------------------------

#[tokio::test]
async fn live_core_blocks_surface_on_every_recall_regardless_of_the_query() {
    let store = store();
    seed_episode(&store, "a note about graph retrieval");
    let block = seed_core_block(
        &store,
        alice_ns(),
        "I never exfiltrate user data.",
        BlockKind::Redline,
        Some("pii"),
        false,
    );

    // The query has nothing to do with the block's content: it surfaces anyway,
    // ahead of the ranked results, unranked.
    let bundle = recall(&store, "graph retrieval", 10).await;
    assert_eq!(core_ids(&bundle), vec![block]);
    let StructuredEntry::CoreBlock(entry) = &bundle.structured[0] else {
        panic!("identity leads the structured view");
    };
    assert_eq!(entry.content, "I never exfiltrate user data.");
    assert_eq!(entry.block_kind, BlockKind::Redline);
    assert_eq!(
        entry.serialization_id,
        SerializationId::derive(
            "core",
            ContentHash::of(b"I never exfiltrate user data.")
                .as_str()
                .as_bytes(),
        ),
        "content-derived under the core kind tag, never the mint-time id"
    );
    assert!(
        bundle.structured.len() >= 2,
        "the ranked episode still fills behind the identity prefix"
    );
    assert_eq!(
        bundle.explanation.returned,
        bundle.structured.len(),
        "the explanation counts the identity prefix"
    );
}

#[tokio::test]
async fn the_identity_prefix_counts_toward_the_limit_but_is_never_capped() {
    let store = store();
    for i in 0..3 {
        seed_episode(&store, &format!("ranked filler episode number {i}"));
    }
    for i in 0..2 {
        seed_core_block(
            &store,
            alice_ns(),
            &format!("standing commitment number {i}"),
            BlockKind::Commitment,
            None,
            false,
        );
    }

    // Limit 3 with 2 identity blocks: one slot is left for the ranked tier.
    let bundle = recall(&store, "ranked filler", 3).await;
    assert_eq!(core_ids(&bundle).len(), 2);
    assert_eq!(
        bundle.structured.len(),
        3,
        "identity counts toward the limit"
    );

    // Limit 1 with 2 identity blocks: both still surface — no silent truncation of
    // identity — and the ranked tier gets nothing.
    let bundle = recall(&store, "ranked filler", 1).await;
    assert_eq!(core_ids(&bundle).len(), 2);
    assert_eq!(
        bundle.structured.len(),
        2,
        "identity is never capped; the ranked fill shrank to zero"
    );
}

#[tokio::test]
async fn the_pre_pass_is_scoped_by_the_visible_set_and_liveness() {
    let store = store();
    seed_episode(&store, "something to recall");
    let visible = seed_core_block(
        &store,
        alice_ns(),
        "alice's own persona",
        BlockKind::Persona,
        None,
        false,
    );
    // Another agent's block, a global block, and a retired block of alice's own:
    // none of them surface — the first two are outside her write...read authority,
    // the third is not live.
    seed_core_block(
        &store,
        Namespace::Agent("someone-else".to_string()),
        "someone else's persona",
        BlockKind::Persona,
        None,
        false,
    );
    let global = seed_core_block(
        &store,
        Namespace::Global,
        "a global commitment",
        BlockKind::Commitment,
        None,
        false,
    );
    seed_core_block(
        &store,
        alice_ns(),
        "a retired stance",
        BlockKind::Persona,
        None,
        true,
    );

    let bundle = recall(&store, "something", 10).await;
    let ids = core_ids(&bundle);
    assert!(ids.contains(&visible), "her own live block surfaces");
    assert!(
        ids.contains(&global),
        "global is in every reader's visible set"
    );
    assert_eq!(
        ids.len(),
        2,
        "the foreign and retired blocks stay out: {ids:?}"
    );
}

#[tokio::test]
async fn the_rendered_views_wrap_and_escape_core_content() {
    let store = store();
    seed_core_block(
        &store,
        alice_ns(),
        "I follow <rules> & say so.",
        BlockKind::Redline,
        Some("\"quoted\" & <sensitive>"),
        false,
    );

    let bundle = recall(&store, "anything", 5).await;

    // Full render: tag-escaped body, attr-escaped sensitivity, the always-include
    // marker nowhere (the full view has no score either), inside the wrapper.
    assert!(
        bundle
            .rendered
            .contains("kind=\"core\" block_kind=\"redline\"")
    );
    assert!(
        bundle
            .rendered
            .contains("sensitivity=\"&quot;quoted&quot; &amp; &lt;sensitive&gt;\""),
        "a hostile sensitivity cannot break out of its attribute: {}",
        bundle.rendered
    );
    assert!(
        bundle
            .rendered
            .contains("I follow &lt;rules&gt; &amp; say so."),
        "the body cannot forge a tag: {}",
        bundle.rendered
    );

    // Byte-identical across calls: the rendered view stays deterministic with the
    // identity prefix in play.
    let again = recall(&store, "anything", 5).await;
    assert_eq!(bundle.rendered, again.rendered);

    // Compact render: same wrapper, the always marker instead of a score.
    let compact = bundle.render_compact(false);
    assert!(compact.contains("kind=\"core\" block_kind=\"redline\" always=\"true\""));
    assert!(
        !compact.contains("block_kind=\"redline\" score="),
        "an unranked block shows no score: {compact}"
    );
}
