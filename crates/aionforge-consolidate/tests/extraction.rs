//! Acceptance tests for fact extraction and entity resolution (M2.T04, write-and-
//! consolidation §2): derived facts carry provenance and source spans, many surface
//! forms resolve to one entity with audited decisions, and replaying a cursor position
//! writes nothing new (idempotent extraction).
//!
//! Hermetic: a deterministic [`RuleExtractor`] reads the episode text and a controllable
//! fake embedder makes resolution decidable — name variants that should coref or cluster
//! map to the same vector, while genuinely distinct entities map apart.

use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, ConsolidationProfile, Consolidator, FactExtractionPass, PassConfig,
    RuleExtractor, RuleSummarizer, STAGE_EXTRACTION,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::Extraction;
use aionforge_domain::time::Timestamp;
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

const DIM: usize = 16;

/// A controllable embedder: it maps each surface to a one-hot vector chosen by a small
/// fixed table, so the resolution test is fully deterministic — `Bob`/`Robert` share an
/// axis (so they cluster), every other entity gets its own axis (so they stay distinct),
/// and free text spreads across the remaining axes.
#[derive(Clone)]
struct ClusterEmbedder {
    model: EmbedderModel,
}

impl ClusterEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "cluster-fake".to_string(),
                version: "1".to_string(),
                dimension: DIM as u32,
            },
        }
    }
}

fn axis(text: &str) -> usize {
    match text.trim().to_lowercase().as_str() {
        "bob" | "robert" => 0,
        "aionforge" => 2,
        "selene" => 3,
        "helios" => 4,
        "alice" | "alice smith" => 5,
        "rust" => 6,
        other => 7 + (other.bytes().map(usize::from).sum::<usize>() % (DIM - 7)),
    }
}

fn one_hot(index: usize) -> Embedding {
    let mut components = vec![0.0f32; DIM];
    components[index] = 1.0;
    Embedding::new(components).expect("non-empty finite vector")
}

impl Embedder for ClusterEmbedder {
    type Error = std::convert::Infallible;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out: Vec<Embedding> = inputs.iter().map(|s| one_hot(axis(s))).collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIM as u32,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

/// Insert a `raw` episode. `minute` makes `ingested_at` strictly increasing so discovery
/// (and therefore cross-episode resolution) runs in a deterministic, documented order.
fn insert_raw_episode(store: &Store, content: &str, namespace: &Namespace, minute: u32) {
    insert_raw_episode_as(store, content, namespace, minute, Role::User);
}

/// Insert a `raw` episode with a chosen role (for the system-role extraction gate).
fn insert_raw_episode_as(
    store: &Store,
    content: &str,
    namespace: &Namespace,
    minute: u32,
    role: Role,
) {
    let at = ts(&format!(
        "2026-06-06T09:{minute:02}:00-05:00[America/Chicago]"
    ));
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: at.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.9,
            last_access: at.clone(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        role,
        captured_at: at,
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
}

/// Drain every pending episode by ticking until the backlog is empty, returning the per-stage
/// profile accumulated across every tick (folded exactly as the scheduler folds it). Lets a test
/// assert the counts a real run produced — e.g. the extraction stage's `derived` tracking the
/// actual rule yield. Callers that only need the side effect ignore the return value.
async fn drain(consolidator: &Consolidator) -> ConsolidationProfile {
    let mut profile = ConsolidationProfile::new();
    loop {
        let report = consolidator.tick_once().await.expect("tick");
        profile.merge_profile(&report.profile);
        if report.pending_after == 0 {
            break;
        }
        // Guard against a stuck queue: stop if a tick made no progress at all.
        assert!(
            report.consolidated + report.retried + report.failed > 0,
            "a tick made no progress but work remains: {report:?}"
        );
    }
    profile
}

fn count(store: &Store, query: BoundQuery) -> usize {
    match store.execute(&query).expect("count query") {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The extraction provenance of the first fact with the given predicate.
fn fact_extraction(store: &Store, predicate: &str) -> Extraction {
    let query = BoundQuery::new(
        "MATCH (f:Fact) WHERE f.predicate = $p RETURN f.extraction AS extraction LIMIT 1",
    )
    .bind_str("p", predicate)
    .expect("bind predicate");
    match store.execute(&query).expect("extraction query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Json(json)) => {
                serde_json::from_value(json.as_serde().clone()).expect("decode extraction")
            }
            other => panic!("expected a JSON extraction, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Every Person entity as `(canonical_name, aliases)`.
fn person_entities(store: &Store) -> Vec<(String, Vec<String>)> {
    let query = BoundQuery::new(
        "MATCH (e:Entity) WHERE e.type = $t RETURN e.canonical_name AS name, e.aliases AS aliases",
    )
    .bind_str("t", "Person")
    .expect("bind type");
    let QueryResult::Rows(rows) = store.execute(&query).expect("person query") else {
        panic!("expected rows");
    };
    let mut out = Vec::new();
    for row in 0..rows.row_count() {
        let Some(Value::String(name)) = rows.value(row, 0) else {
            continue;
        };
        let aliases = match rows.value(row, 1) {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|value| match value {
                    Value::String(alias) => Some(alias.as_str().to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        out.push((name.as_str().to_string(), aliases));
    }
    out
}

fn reset_to_raw(store: &Store) {
    let query =
        BoundQuery::new("MATCH (e:Episode) SET e.consolidation_state = $raw RETURN e.id AS id")
            .bind_str("raw", "raw")
            .expect("bind raw");
    store.execute(&query).expect("reset episodes to raw");
}

#[tokio::test]
async fn extraction_resolves_surfaces_records_provenance_and_is_idempotent() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    // Episode 1 exercises intra-episode coref ("Alice" / "Alice Smith") and in-batch
    // triple dedup (the same triple twice). Episode 2 reuses "Alice" (exact-alias gate
    // across episodes). Episodes 3 and 4 exercise embedding clustering: "Robert" clusters
    // into "Bob", and the repeated triple collapses with a second supporting episode.
    insert_raw_episode(
        &store,
        "Alice works on Aionforge. Alice Smith works on Aionforge.",
        &namespace,
        0,
    );
    insert_raw_episode(&store, "Alice prefers Rust.", &namespace, 1);
    insert_raw_episode(&store, "Bob works on Helios.", &namespace, 2);
    insert_raw_episode(&store, "Robert works on Helios.", &namespace, 3);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // 1. Derived facts carry extraction provenance and source spans (02 §6.2).
    let extraction = fact_extraction(&store, "prefers");
    assert_eq!(
        extraction.extraction_rule_version.as_deref(),
        Some("rule-v2"),
        "the fact records the extraction rule version"
    );
    assert!(
        !extraction.source_spans.is_empty(),
        "the fact carries the source spans it was drawn from"
    );

    // 2. Many surface forms resolve to one entity, with audited decisions (02 §4.3).
    let persons = person_entities(&store);
    assert_eq!(
        persons.len(),
        2,
        "Alice/Alice Smith and Bob/Robert collapse to two people: {persons:?}"
    );
    let alice = persons
        .iter()
        .find(|(name, _)| name == "Alice")
        .expect("Alice is canonical");
    assert!(
        alice.1.iter().any(|alias| alias == "Alice Smith"),
        "Alice Smith folds in as an alias: {alice:?}"
    );
    assert!(
        persons.iter().any(|(name, _)| name == "Bob"),
        "Bob is canonical (processed before Robert)"
    );
    assert!(
        !persons
            .iter()
            .any(|(name, _)| name == "Robert" || name == "Alice Smith"),
        "a folded surface is never its own entity: {persons:?}"
    );
    // One canonicalize decision is audited per distinct surface per episode: ep1 audits
    // Alice/Aionforge/Alice Smith (3), ep2 Alice/Rust (2), ep3 Bob/Helios (2), ep4
    // Robert/Helios (2) = 9. An exact count catches a regression that drops auditing for
    // whole episodes as well as one that over-audits.
    let canonicalize_audits = count(
        &store,
        BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN a.id AS id")
            .bind_str("k", "canonicalize")
            .expect("bind kind"),
    );
    assert_eq!(
        canonicalize_audits, 9,
        "every resolution decision is audited, once per surface per episode"
    );

    let facts_before = count(&store, BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id"));
    let entities_before = count(
        &store,
        BoundQuery::new("MATCH (e:Entity) RETURN e.id AS id"),
    );
    assert_eq!(
        facts_before, 3,
        "duplicate triples across surfaces and episodes collapse to three facts"
    );
    assert_eq!(
        entities_before, 5,
        "five distinct entities: Alice, Aionforge, Rust, Bob, Helios"
    );

    // Audit events are content-addressed too (consolidate `audit` module), so a replay must
    // reuse them rather than write second copies — the same idempotency the fact and entity
    // ids give. (With the old random audit ids this count would double on replay; and without
    // the dedup-aware audit write the replay below would fail the AuditEvent UNIQUE constraint.)
    let audits_before = count(
        &store,
        BoundQuery::new("MATCH (a:AuditEvent) RETURN a.id AS id"),
    );

    // 3. Extraction is idempotent per cursor position: replay every episode and assert
    // nothing new is written (content-derived ids plus value dedup make it a no-op).
    reset_to_raw(&store);
    drain(&consolidator).await;
    assert_eq!(
        count(&store, BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id")),
        facts_before,
        "re-extraction adds no facts"
    );
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (e:Entity) RETURN e.id AS id")
        ),
        entities_before,
        "re-extraction adds no entities"
    );
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (a:AuditEvent) RETURN a.id AS id")
        ),
        audits_before,
        "re-extraction writes no duplicate audit events (content-derived ids + dedup-aware write)"
    );
}

#[tokio::test]
async fn audit_actor_id_is_stable_across_pass_construction() {
    // The pass's audit actor id is content-derived from its configuration, not a per-process
    // random value, so two freshly-built passes over the same configuration stamp the same
    // actor id — forensic attribution survives a restart.
    async fn actor_for_a_run() -> String {
        let store = store();
        let namespace = Namespace::Agent("alice".to_string());
        insert_raw_episode(&store, "Alice prefers Rust.", &namespace, 0);
        let mut consolidator =
            Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
        consolidator.register(Box::new(FactExtractionPass::new(
            Arc::new(RuleExtractor::with_default_rules()),
            Arc::new(ClusterEmbedder::new()),
            Arc::new(RuleSummarizer::with_default_rules()),
            PassConfig::default(),
        )));
        drain(&consolidator).await;
        let query = BoundQuery::new(
            "MATCH (a:AuditEvent) WHERE a.kind = $k RETURN a.actor_id AS actor LIMIT 1",
        )
        .bind_str("k", "canonicalize")
        .expect("bind kind");
        let QueryResult::Rows(rows) = store.execute(&query).expect("actor query") else {
            panic!("expected rows");
        };
        match rows.value(0, 0) {
            // `actor_id` is a UUID-typed column, so the row carries a `Value::Uuid`; render it
            // to a string for the cross-run stability comparison below.
            Some(Value::Uuid(actor)) => actor.to_string(),
            other => panic!("expected an actor id uuid, got {other:?}"),
        }
    }

    let first = actor_for_a_run().await;
    let second = actor_for_a_run().await;
    assert!(!first.is_empty(), "the audit records a non-empty actor id");
    assert_eq!(
        first, second,
        "the same pass configuration stamps the same actor id across separate constructions"
    );
}

#[tokio::test]
async fn consolidation_keeps_namespaces_isolated() {
    let store = store();
    let alice = Namespace::Agent("alice".to_string());
    let bob = Namespace::Agent("bob".to_string());
    // The same subject surface in two namespaces, with conflicting objects. Entity (subject)
    // ids are namespace-scoped and the summarize/detect reads are explicitly filtered to the
    // episode's namespace, so the two "Alice" facts stay distinct: consolidation must not merge
    // them, roll one namespace's facts into the other's summary, or bridge them with a
    // supersession/contradiction edge.
    insert_raw_episode(&store, "Alice prefers Rust.", &alice, 0);
    insert_raw_episode(&store, "Alice prefers Go.", &bob, 1);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    drain(&consolidator).await;

    // Two independent facts, one per namespace — never merged across the boundary.
    assert_eq!(
        count(&store, BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id")),
        2,
        "each namespace keeps its own fact; namespace-scoped ids prevent a cross-namespace merge"
    );
    let alice_facts = count(
        &store,
        BoundQuery::new("MATCH (f:Fact) WHERE f.namespace = $n RETURN f.id AS id")
            .bind_str("n", "agent:alice")
            .expect("bind namespace"),
    );
    assert_eq!(
        alice_facts, 1,
        "alice's namespace holds exactly its own fact"
    );

    // No detection edge bridges the namespaces: a foreign-namespace fact is never a detection
    // candidate, so the cross-namespace "prefers" objects neither supersede nor contradict.
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (a:Fact)-[:SUPERSEDED_BY]->(b:Fact) RETURN a.id AS id"),
        ),
        0,
        "no supersession edge crosses a namespace"
    );
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (a:Fact)-[:CONTRADICTS]->(b:Fact) RETURN a.id AS id"),
        ),
        0,
        "no contradiction edge crosses a namespace"
    );
}

#[tokio::test]
async fn whitespace_and_case_variant_surfaces_resolve_to_one_entity() {
    // Two episodes name the same person with different internal spacing and case. The exact
    // name gate normalizes both, so the second resolves to the first rather than minting a
    // duplicate — the end-to-end payoff of routing the id derivation and the resolution gate
    // through one `normalize` (the rule extractor preserves internal whitespace, so the variant
    // surface reaches resolution intact).
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    insert_raw_episode(&store, "Mary Jane prefers Rust.", &namespace, 0);
    insert_raw_episode(&store, "mary  jane prefers Rust.", &namespace, 1);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    drain(&consolidator).await;

    let persons = person_entities(&store);
    assert_eq!(
        persons.len(),
        1,
        "spacing/case variants of one name resolve to a single person: {persons:?}"
    );
    assert_eq!(
        persons[0].0, "Mary Jane",
        "the first surface seen is canonical"
    );
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = $p RETURN f.id AS id")
                .bind_str("p", "prefers")
                .expect("bind predicate"),
        ),
        1,
        "the same triple under a variant surface collapses to one fact"
    );
}

#[tokio::test]
async fn a_system_role_episode_produces_no_facts() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    // A user turn yields facts; a system directive in the same namespace yields none —
    // a fact carries no role and inherits the episode namespace, so without this gate the
    // directive would launder into a role-less fact in a visible namespace (07 §4).
    insert_raw_episode(&store, "Alice prefers Rust.", &namespace, 0);
    insert_raw_episode_as(&store, "Bob works on Helios.", &namespace, 1, Role::System);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = $p RETURN f.id AS id")
                .bind_str("p", "prefers")
                .expect("bind predicate"),
        ),
        1,
        "the user turn still produces its fact"
    );
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = $p RETURN f.id AS id")
                .bind_str("p", "works_on")
                .expect("bind predicate"),
        ),
        0,
        "the system-role episode produces no fact"
    );
}

/// The stored trust of the first fact with the given predicate.
fn fact_trust(store: &Store, predicate: &str) -> f64 {
    let query =
        BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = $p RETURN f.trust AS trust LIMIT 1")
            .bind_str("p", predicate)
            .expect("bind predicate");
    match store.execute(&query).expect("trust query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            // A stored trust column reads back as a double; tolerate a single-precision return
            // too so the assertion is robust to the metric encoding.
            Some(Value::Float(trust)) => *trust,
            Some(Value::Float32(trust)) => f64::from(*trust),
            other => panic!("expected a float trust, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn an_is_a_dependent_clause_yields_no_fact() {
    // The removed `is a`→`is_a` catch-all used to turn any `"X is a Y"` clause — including a
    // dependent fragment a sentence split off — into a junk fact. With that rule gone and the
    // subject-plausibility gate live, this episode yields no fact at all (no typed marker
    // matches, and the dependent clause is not a subject).
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    insert_raw_episode(
        &store,
        "The migration ran, which means it is a breaking change.",
        &namespace,
        0,
    );

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    drain(&consolidator).await;

    assert_eq!(
        count(&store, BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id")),
        0,
        "the is_a dependent-clause exemplar produces no fact"
    );
    // And it minted no junk entity from the clause fragment either.
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (e:Entity) RETURN e.id AS id")
        ),
        0,
        "no entity is minted from a rejected clause"
    );
}

#[tokio::test]
async fn genuine_typed_entity_facts_still_extract() {
    // Recall is preserved: every shipped typed rule (works_on/based_in/prefers/uses) still
    // fires on clean prose, so the precision gates do not regress legitimate extraction.
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    insert_raw_episode(&store, "Alice works on Aionforge.", &namespace, 0);
    insert_raw_episode(&store, "Alice is based in Helios.", &namespace, 1);
    insert_raw_episode(&store, "Alice prefers Rust.", &namespace, 2);
    insert_raw_episode(&store, "Alice uses Selene.", &namespace, 3);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    drain(&consolidator).await;

    for predicate in ["works_on", "based_in", "prefers", "uses"] {
        assert_eq!(
            count(
                &store,
                BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = $p RETURN f.id AS id")
                    .bind_str("p", predicate)
                    .expect("bind predicate"),
            ),
            1,
            "the genuine `{predicate}` typed-entity fact still extracts"
        );
    }
}

#[tokio::test]
async fn a_derived_fact_trust_is_below_its_source_episode() {
    // A derived fact is weaker evidence than its source episode, so its stored trust is the
    // episode trust discounted by `derived_trust_factor` (< 1): with the inserted episode
    // trust 0.9 and the default factor 0.9, the fact's trust is 0.81 — strictly below 0.9.
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    insert_raw_episode(&store, "Alice prefers Rust.", &namespace, 0);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    drain(&consolidator).await;

    let episode_trust = 0.9_f64; // the trust `insert_raw_episode` stamps
    let trust = fact_trust(&store, "prefers");
    assert!(
        trust < episode_trust,
        "the derived fact ranks strictly below its source episode (fact {trust} < episode \
         {episode_trust})"
    );
    let expected = episode_trust * 0.9; // the default derived_trust_factor
    assert!(
        (trust - expected).abs() < 1e-9,
        "the derived fact trust is the episode trust times the discount: {trust} vs {expected}"
    );
}

#[tokio::test]
async fn svo_derives_a_fact_but_narrative_prose_does_not() {
    // The P0b determination, as an executable control. The deterministic SVO ruleset DERIVES a
    // fact from a clean subject-verb-object episode and DERIVES NOTHING from narrative
    // decision/design prose with no marker. This isolates "the extractor and the #262 subject
    // gate work" from "the content carries no triple to pull": the 0-facts-from-prose observed on
    // real captures is the latter (content shape), NOT a gating regression. Rich-prose extraction
    // is a model-backed-extractor job (M4), not a looser deterministic gate — loosening the gate
    // is exactly what #262 tightened to stop junk facts.
    let store = store();
    let namespace = Namespace::Agent("control".to_string());

    // A clean SVO episode (the "works on" marker, Person -> Project) — known-good shape.
    insert_raw_episode(&store, "Bob works on Helios.", &namespace, 1);
    // Narrative decision prose — the shape of most real agent captures. No marker fires.
    insert_raw_episode(
        &store,
        "We decided the retrieval layer should fuse signals by rank, for robustness to scale \
         mismatch.",
        &namespace,
        2,
    );
    // A near-miss: "causes" contains the bare substring "uses" but not the space-padded " uses "
    // marker (the match is `sentence.find(" uses ")`), so the padding guard must NOT false-fire.
    insert_raw_episode(&store, "The retry causes a refresh.", &namespace, 3);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    let profile = drain(&consolidator).await;

    // The SVO episode derived its fact — the extractor and subject gate work on a clean triple.
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = 'works_on' RETURN f.id AS id"),
        ),
        1,
        "a clean SVO episode derives its fact",
    );
    // The narrative-prose AND near-miss episodes added NOTHING: the only fact in the store is the
    // SVO one, so the 0-facts-from-prose outcome is by-design content shape, not a gating bug, and
    // the padding guard did not false-fire on "ca[uses]".
    assert_eq!(
        count(&store, BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id")),
        1,
        "only the SVO triple derives a fact; prose and the near-miss derive nothing",
    );

    // The clarity payload at the profile level: the extraction stage's `derived` reflects the
    // REAL rule yield — it scanned all three episodes and pulled exactly the one SVO fact, not a
    // hardcoded zero (the P0b confusion this stage removes) nor a false-fire on the near-miss.
    let extraction = profile
        .stages()
        .iter()
        .find(|s| s.stage == STAGE_EXTRACTION)
        .expect("extraction stage profiled");
    assert_eq!(
        (extraction.candidates_considered, extraction.derived),
        (3, 1),
        "extraction scanned 3 episodes and derived exactly the one SVO fact",
    );
}
