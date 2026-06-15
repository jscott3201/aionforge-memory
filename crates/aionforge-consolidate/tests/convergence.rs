//! Property-based convergence and no-silent-loss tests for concurrent merge (M4.T02,
//! 06 §2). Where `detection.rs` pins specific reordering cases by hand, these drive the
//! REAL `detect -> materialize -> current_support` pipeline over randomized inputs and
//! assert the two guarantees the merge has to make:
//!
//! - **Convergence:** the same multiset of assertions resolves to the SAME recall set no
//!   matter what order the episodes are consolidated in. Arrival order is perturbed
//!   independently of event time (`captured_at`), so a run where arrival agrees with event
//!   order and a run where it is reversed or scrambled must land on the same current value.
//! - **No silent loss:** every distinct asserted triple keeps a `Fact` node in the graph
//!   regardless of how the merge resolved it. A superseded loser or a quarantined victim is
//!   retired by status/edge, never deleted, so it stays reachable in `History`
//!   (`TemporalMode::History` admits every fact node — retrieval/temporal.rs).
//! - **The `Stats` block converges too:** the recomputable stats each surviving fact carries
//!   are a function of the value it asserts (inherited from that value's one writer episode),
//!   not of arrival order. So every fact's `Stats` — minus `last_access`, which is the ambient
//!   pass clock (`derived_stats`, fact_extraction.rs) and legitimately differs between runs —
//!   is identical across orders. This is the last-write-wins register half of the merge model
//!   (docs/crdt-model.md): where a stats update is genuinely dropped on a re-assertion the
//!   field is recomputable, so the loss is acceptable.
//!
//! The oracle is computed in the test, not read back from the store: for a functional
//! predicate the current value is the latest event time, ties broken by the smaller
//! canonical object order; for a contradiction the surviving value is the higher-trust
//! side, ties broken the same way. Text objects are used throughout so the object order
//! key (`"string:" + text`) is predictable without reconstructing a resolved entity id.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, DetectionConfig, FactExtractionPass, InductionConfig,
    ObjectRule, PassConfig, PredicateRule, ResolutionConfig, Rule, RuleExtractor, RuleSummarizer,
    SummarizationConfig,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, CandidateSet, QueryResult, Store, StoreConfig, Value};
use proptest::prelude::*;

const DIM: usize = 16;

/// A deterministic one-hot embedder keyed on a stable hash of the surface. The only entity
/// that is resolved in these tests is the single fixed subject (`Widget` / `Server`), so
/// every episode embeds the same surface to the same axis and resolves it to one entity;
/// hash collisions between unrelated surfaces are harmless because no two distinct surfaces
/// ever need to stay separate here (the objects are text literals, never resolved).
#[derive(Clone)]
struct HashEmbedder {
    model: EmbedderModel,
}

impl HashEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "hash-fake".to_string(),
                version: "1".to_string(),
                dimension: DIM as u32,
            },
        }
    }
}

fn one_hot(surface: &str) -> Embedding {
    let axis = surface
        .trim()
        .to_lowercase()
        .bytes()
        .map(usize::from)
        .sum::<usize>()
        % DIM;
    let mut components = vec![0.0f32; DIM];
    components[axis] = 1.0;
    Embedding::new(components).expect("non-empty finite vector")
}

impl Embedder for HashEmbedder {
    type Error = std::convert::Infallible;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out: Vec<Embedding> = inputs.iter().map(|s| one_hot(s)).collect();
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

/// Insert a `raw` episode with independent arrival (`ingested`) and event (`captured`)
/// minutes plus an explicit trust. Decoupling the two minutes is the whole point: arrival
/// order (discovery is by `ingested_at`) can be set against event order (the merge keys on
/// `captured_at`), which is how a run perturbs reordering without touching event time.
fn insert(
    store: &Store,
    content: &str,
    namespace: &Namespace,
    ingested_minute: u32,
    captured_minute: u32,
    trust: f64,
) {
    let ingested = ts(&format!(
        "2026-06-06T09:{ingested_minute:02}:00-05:00[America/Chicago]"
    ));
    let captured = ts(&format!(
        "2026-06-06T09:{captured_minute:02}:00-05:00[America/Chicago]"
    ));
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ingested.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust,
            last_access: ingested,
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at: captured,
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

/// Drain every pending episode by ticking until the backlog is empty.
async fn drain(consolidator: &Consolidator) {
    loop {
        let report = consolidator.tick_once().await.expect("tick");
        if report.pending_after == 0 {
            break;
        }
        assert!(
            report.consolidated + report.retried + report.failed > 0,
            "a tick made no progress but work remains: {report:?}"
        );
    }
}

/// The objects of the current-support facts (the recall set) for a predicate, sorted so the
/// result is comparable across runs regardless of node iteration order.
fn current_support_objects(store: &Store, predicate: &str) -> Vec<ObjectValue> {
    let members = store
        .candidate_state_members(CandidateSet::CurrentSupportFacts)
        .expect("current-support members");
    let mut out = Vec::new();
    for node in members {
        if let Some(fact) = store.fact_by_node_id(node).expect("fact by node")
            && fact.predicate == predicate
        {
            out.push(fact.object);
        }
    }
    out.sort_by_key(|object| format!("{object:?}"));
    out
}

/// Count every `Fact` node carrying a predicate, of any status. `History` admits all fact
/// nodes, so this is the History-reachable count: it must equal the number of distinct
/// asserted triples for the no-silent-loss guarantee to hold.
fn total_facts_with_predicate(store: &Store, predicate: &str) -> u64 {
    let query = BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = $p RETURN count(f) AS n")
        .bind_str("p", predicate)
        .expect("bind predicate");
    match store.execute(&query).expect("count query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            other => panic!("expected an integer count, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Every fact's `Stats` for a predicate, keyed by the canonical object string, over ALL fact
/// nodes — current winners and superseded losers alike. The subject is read off any current
/// fact and then `facts_by_subject` enumerates the rest; that index is unfiltered by status, so
/// a retired loser is still included. All assertions here share one subject (`Widget`/`Server`),
/// so one subject covers every fact.
fn fact_stats_by_object(store: &Store, predicate: &str) -> BTreeMap<String, Stats> {
    let subject = store
        .candidate_state_members(CandidateSet::CurrentSupportFacts)
        .expect("current-support members")
        .into_iter()
        .filter_map(|node| store.fact_by_node_id(node).expect("fact by node"))
        .find(|fact| fact.predicate == predicate)
        .map(|fact| fact.subject_id);
    let Some(subject) = subject else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for node in store.facts_by_subject(&subject).expect("facts by subject") {
        if let Some(fact) = store.fact_by_node_id(node).expect("fact by node")
            && fact.predicate == predicate
        {
            out.insert(format!("{:?}", fact.object), fact.stats);
        }
    }
    out
}

/// `Stats` minus `last_access`, as a comparable key. `last_access` is stamped with the ambient
/// pass clock (`derived_stats` sets it to `now`, fact_extraction.rs), so it legitimately differs
/// between runs and must be excluded from any cross-order equality. The floats are compared by
/// their bit pattern: these fields are constants copied through with no arithmetic, so the bits
/// are exact, and `to_bits` keeps the key `Eq` without NaN surprises.
fn stats_sans_access(stats: &Stats) -> (u64, u64, u64, u64, u64, bool) {
    (
        stats.importance.to_bits(),
        stats.trust.to_bits(),
        stats.access_count_recent,
        stats.referenced_count,
        stats.surprise.to_bits(),
        stats.is_pinned,
    )
}

// ---- functional predicate: one current value, chosen the same way every time ----

/// The fixed grade vocabulary. Distinct lowercase words so each is its own text object and
/// their canonical order (`"string:" + grade`) is plain lexicographic order.
const GRADES: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

/// The extractor and pass config for the functional `rating` predicate. Shared so the
/// convergence proptest and the idempotency test build the pass identically.
fn functional_pass() -> (RuleExtractor, PassConfig) {
    let extractor = RuleExtractor::new(
        "rule-rating",
        vec![Rule {
            marker: "rated".to_string(),
            predicate: "rating".to_string(),
            subject_type: "Item".to_string(),
            object: ObjectRule::Text,
            confidence: 0.9,
        }],
    );
    let mut detection = DetectionConfig::with_default_rules();
    detection.predicates.insert(
        "rating".to_string(),
        PredicateRule {
            functional: true,
            contradicts: Vec::new(),
        },
    );
    let pass_config = PassConfig {
        resolution: ResolutionConfig::default(),
        detection,
        summarization: SummarizationConfig::default(),
        induction: InductionConfig::default(),
        ..PassConfig::default()
    };
    (extractor, pass_config)
}

/// A distinct trust per grade so the cross-order `Stats` assertion is not trivially true: each
/// grade's fact must carry ITS OWN writer's trust regardless of arrival order. Trust does not
/// enter the functional merge (which keys on event time then object), so the winner is
/// unchanged — only the per-object `Stats` becomes discriminating.
fn grade_trust(grade: &str) -> f64 {
    let index = GRADES.iter().position(|g| *g == grade).unwrap_or(0);
    0.5 + f64::from(u8::try_from(index).unwrap_or(0)) * 0.05
}

/// Consolidate a list of `(grade, event_minute)` functional assertions in the given order
/// (arrival minute = position in the slice) and return the recall set plus the total fact
/// count. The predicate `rating` is registered functional, so exactly one grade survives.
fn run_functional(ordered: &[(String, u32)]) -> (Vec<ObjectValue>, u64, BTreeMap<String, Stats>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime");
    rt.block_on(async {
        let store = store();
        let namespace = Namespace::Agent("widget".to_string());
        let (extractor, pass_config) = functional_pass();

        for (arrival, (grade, event_minute)) in ordered.iter().enumerate() {
            let content = format!("Widget rated {grade}.");
            insert(
                &store,
                &content,
                &namespace,
                u32::try_from(arrival).expect("arrival fits u32"),
                *event_minute,
                grade_trust(grade),
            );
        }

        let mut consolidator =
            Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
        consolidator.register(Box::new(FactExtractionPass::new(
            Arc::new(extractor),
            Arc::new(HashEmbedder::new()),
            Arc::new(RuleSummarizer::with_default_rules()),
            pass_config,
        )));
        drain(&consolidator).await;

        (
            current_support_objects(&store, "rating"),
            total_facts_with_predicate(&store, "rating"),
            fact_stats_by_object(&store, "rating"),
        )
    })
}

/// The functional winner: the grade with the latest event time, ties settled by the smaller
/// canonical object order (= lexicographically smaller grade, since the keys share the
/// `"string:"` prefix). A pure function of the assertion values, never their arrival order.
fn functional_oracle(assertions: &[(String, u32)]) -> Vec<ObjectValue> {
    assertions
        .iter()
        .max_by(|(grade_a, event_a), (grade_b, event_b)| {
            // Greatest = latest event, then (for an event tie) the smaller grade — so rank
            // the smaller grade as greater by reversing the grade comparison.
            event_a.cmp(event_b).then(grade_b.cmp(grade_a))
        })
        .map(|(grade, _)| vec![ObjectValue::Text(grade.clone())])
        .unwrap_or_default()
}

/// A few orderings of the same multiset: as-generated, reversed, sorted by grade, and sorted
/// by event time. The last makes arrival agree with event order; the others scramble it
/// against the fixed event times — so agreement across all four is convergence under
/// reordering, not a lucky arrival order.
fn arrival_orderings(base: &[(String, u32)]) -> Vec<Vec<(String, u32)>> {
    let mut reversed = base.to_vec();
    reversed.reverse();
    let mut by_grade = base.to_vec();
    by_grade.sort_by(|a, b| a.0.cmp(&b.0));
    let mut by_event = base.to_vec();
    by_event.sort_by_key(|item| item.1);
    vec![base.to_vec(), reversed, by_grade, by_event]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// For any set of distinct functional assertions with arbitrary event times, every
    /// arrival order lands on the same single current value — the oracle winner — and no
    /// fact is lost (the fact count equals the number of distinct grades).
    #[test]
    fn a_functional_predicate_converges_under_any_arrival_order(
        case in functional_case(),
    ) {
        let base = case;
        let distinct = base.len() as u64;
        let oracle = functional_oracle(&base);

        let mut recalls = Vec::new();
        let mut stats_keys = Vec::new();
        for ordering in arrival_orderings(&base) {
            let (recall, fact_count, stats) = run_functional(&ordering);
            prop_assert_eq!(
                fact_count,
                distinct,
                "every distinct grade keeps a fact node (no silent loss)"
            );
            let keyed: BTreeMap<String, (u64, u64, u64, u64, u64, bool)> = stats
                .iter()
                .map(|(object, stats)| (object.clone(), stats_sans_access(stats)))
                .collect();
            recalls.push(recall);
            stats_keys.push(keyed);
        }
        for recall in &recalls {
            prop_assert_eq!(
                recall,
                &oracle,
                "recall is the oracle winner regardless of arrival order"
            );
        }
        for keyed in &stats_keys {
            prop_assert_eq!(
                keyed,
                &stats_keys[0],
                "every fact's Stats (minus the ambient-clock last_access) is identical across arrival orders"
            );
        }
    }
}

/// A case: 2..=6 distinct grades, each paired with an event minute in `0..=5` (so event
/// ties — which exercise the object-order tiebreak — occur). `subsequence` guarantees the
/// grades are distinct, so every triple is its own fact node.
fn functional_case() -> impl Strategy<Value = Vec<(String, u32)>> {
    proptest::sample::subsequence(GRADES.to_vec(), 2..=GRADES.len()).prop_flat_map(|grades| {
        let n = grades.len();
        let grades: Vec<String> = grades.into_iter().map(str::to_string).collect();
        (Just(grades), prop::collection::vec(0u32..=5, n))
            .prop_map(|(grades, minutes)| grades.into_iter().zip(minutes).collect())
    })
}

/// Re-running consolidation over a fully-consolidated store changes no `Stats`. Every field is
/// set once at fact creation and never mutated afterward — dedup reuse, supersession, and
/// contradiction touch only edges and the `status` scalar (materialize.rs) — and the cursor
/// does not reprocess a consolidated episode, so a second drain is a no-op. The full block
/// including `last_access` is compared here, since no fact is re-materialized on the second
/// pass: this is the last-write-wins "a lost update self-heals / replay is a no-op" guarantee.
#[test]
fn re_running_consolidation_leaves_fact_stats_unchanged() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime");
    rt.block_on(async {
        let store = store();
        let namespace = Namespace::Agent("widget".to_string());
        let (extractor, pass_config) = functional_pass();

        let assertions = [("alpha", 0u32), ("bravo", 1), ("charlie", 2)];
        for (arrival, (grade, event_minute)) in assertions.iter().enumerate() {
            let content = format!("Widget rated {grade}.");
            insert(
                &store,
                &content,
                &namespace,
                u32::try_from(arrival).expect("arrival fits u32"),
                *event_minute,
                grade_trust(grade),
            );
        }

        let mut consolidator =
            Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
        consolidator.register(Box::new(FactExtractionPass::new(
            Arc::new(extractor),
            Arc::new(HashEmbedder::new()),
            Arc::new(RuleSummarizer::with_default_rules()),
            pass_config,
        )));

        drain(&consolidator).await;
        let first = fact_stats_by_object(&store, "rating");
        assert_eq!(
            first.len(),
            assertions.len(),
            "every grade materialized exactly one fact"
        );

        drain(&consolidator).await;
        let second = fact_stats_by_object(&store, "rating");
        assert_eq!(
            first, second,
            "a second consolidation pass leaves every fact's Stats byte-identical"
        );
    });
}

// ---- contradiction: the contested value resolves the same way every time ----

/// Consolidate an up/down `status` contradiction with the given per-side trust in a chosen
/// arrival order, and return the recall set plus the total fact count.
fn run_contradiction(up_trust: f64, down_trust: f64, up_first: bool) -> (Vec<ObjectValue>, u64) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime");
    rt.block_on(async {
        let store = store();
        let namespace = Namespace::Agent("ops".to_string());
        let extractor = RuleExtractor::new(
            "rule-status",
            vec![Rule {
                marker: "status".to_string(),
                predicate: "status".to_string(),
                subject_type: "Service".to_string(),
                object: ObjectRule::Text,
                confidence: 0.9,
            }],
        );
        let mut detection = DetectionConfig::with_default_rules();
        detection.predicates.insert(
            "status".to_string(),
            PredicateRule {
                functional: false,
                contradicts: vec![(
                    ObjectValue::Text("up".to_string()),
                    ObjectValue::Text("down".to_string()),
                )],
            },
        );
        let pass_config = PassConfig {
            resolution: ResolutionConfig::default(),
            detection,
            summarization: SummarizationConfig::default(),
            induction: InductionConfig::default(),
            ..PassConfig::default()
        };

        let (first, first_trust, second, second_trust) = if up_first {
            (
                "Server status up.",
                up_trust,
                "Server status down.",
                down_trust,
            )
        } else {
            (
                "Server status down.",
                down_trust,
                "Server status up.",
                up_trust,
            )
        };
        insert(&store, first, &namespace, 0, 0, first_trust);
        insert(&store, second, &namespace, 5, 5, second_trust);

        let mut consolidator =
            Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
        consolidator.register(Box::new(FactExtractionPass::new(
            Arc::new(extractor),
            Arc::new(HashEmbedder::new()),
            Arc::new(RuleSummarizer::with_default_rules()),
            pass_config,
        )));
        drain(&consolidator).await;

        (
            current_support_objects(&store, "status"),
            total_facts_with_predicate(&store, "status"),
        )
    })
}

/// The surviving value of an up/down contradiction: the higher-trust side, ties settled by
/// the smaller canonical object order — `"string:down" < "string:up"`, so `down` is the
/// victim on a tie and `up` survives.
fn contradiction_oracle(up_trust: f64, down_trust: f64) -> Vec<ObjectValue> {
    let survivor = match up_trust.total_cmp(&down_trust) {
        Ordering::Greater => "up",
        Ordering::Less => "down",
        Ordering::Equal => "up",
    };
    vec![ObjectValue::Text(survivor.to_string())]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// For any pair of trusts, the contradiction resolves to the same surviving value in
    /// both arrival orders, that value is the oracle survivor, and both facts are retained
    /// (the contradiction retires nothing — count is always two). Trust is quantized to
    /// tenths so exact ties — which exercise the object-order tiebreak — actually occur.
    #[test]
    fn a_contradiction_converges_under_either_arrival_order(
        up_tenths in 0u8..=10,
        down_tenths in 0u8..=10,
    ) {
        let up_trust = f64::from(up_tenths) / 10.0;
        let down_trust = f64::from(down_tenths) / 10.0;
        let oracle = contradiction_oracle(up_trust, down_trust);

        let (recall_up_first, count_up_first) = run_contradiction(up_trust, down_trust, true);
        let (recall_down_first, count_down_first) =
            run_contradiction(up_trust, down_trust, false);

        prop_assert_eq!(count_up_first, 2, "both contradicting facts are retained");
        prop_assert_eq!(count_down_first, 2, "both contradicting facts are retained");
        prop_assert_eq!(
            &recall_up_first,
            &oracle,
            "recall is the higher-trust survivor"
        );
        prop_assert_eq!(
            &recall_up_first,
            &recall_down_first,
            "recall is identical regardless of arrival order"
        );
    }
}
