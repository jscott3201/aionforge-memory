//! Integration tests for the conservative skill-induction pass (05 §1, M3.T06).
//!
//! Drives the real scheduler so induction is exercised through the atomic flip: episodes are
//! inserted, the pass runs, and induced skills are read back from the committed graph. Covers the
//! reuse-evidence gate, every conservative precondition (role, private namespace, structure
//! floor, off-by-default), idempotent replay, and the rule-version-bump version path.

use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, InductionConfig, RuleInducer, SkillInductionPass,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::time::Timestamp;
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

/// A multi-line procedure that clears the default structure floor (>= 16 chars, >= 5 tokens).
const PROCEDURE: &str =
    "run the database migration\nthen restart the api service\nfinally verify the health endpoint";

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate");
    Arc::new(store)
}

/// The default agent-private namespace the tests induce into.
fn alice() -> Namespace {
    Namespace::Agent("alice".to_string())
}

/// Induction tuning that is *on* with the given recurrence threshold (everything else default).
fn enabled(threshold: usize) -> InductionConfig {
    InductionConfig {
        enabled: true,
        repetition_threshold: threshold,
        ..InductionConfig::default()
    }
}

/// Insert a `raw` episode with explicit content, role, and namespace. The `content_hash` is the
/// hash of the content, so re-inserting the same content makes a recurrence the pass can count.
fn insert(store: &Store, content: &str, role: Role, namespace: &Namespace, minute: u32) {
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

/// Insert `count` byte-identical procedure episodes (a recurring reuse signal).
fn insert_repeats(store: &Store, content: &str, role: Role, namespace: &Namespace, count: u32) {
    for minute in 0..count {
        insert(store, content, role, namespace, minute);
    }
}

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

fn induction_consolidator(
    store: &Arc<Store>,
    inducer: RuleInducer,
    config: InductionConfig,
) -> Consolidator {
    let mut consolidator = Consolidator::new(Arc::clone(store), ConsolidationConfig::default());
    consolidator.register(Box::new(SkillInductionPass::new(Arc::new(inducer), config)));
    consolidator
}

/// Every induced skill in the graph, decoded from the store (enumerated, not name-guessed).
fn induced_skills(store: &Store) -> Vec<Skill> {
    let query = BoundQuery::new("MATCH (s:Skill) RETURN s.id AS id");
    let QueryResult::Rows(rows) = store.execute(&query).expect("skill query") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in 0..rows.row_count() {
        if let Some(Value::String(id)) = rows.value(row, 0) {
            let id = Id::parse(id.as_str()).expect("valid id");
            if let Some(skill) = store.skill_by_id(&id).expect("decode skill")
                && skill.induced
            {
                out.push(skill);
            }
        }
    }
    out
}

/// Count `InduceSkill` audit events wired to a specific skill id.
fn induce_audits_for(store: &Store, skill_id: &Id) -> usize {
    let query = BoundQuery::new(
        "MATCH (a:AuditEvent)-[:AUDIT]->(s:Skill {id: $sid}) WHERE a.kind = $k RETURN a.id AS id",
    )
    .bind_str("sid", skill_id.as_str())
    .expect("bind sid")
    .bind_str("k", "induce_skill")
    .expect("bind kind");
    match store.execute(&query).expect("audit query") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

/// True if the induced skill links to a source episode by `DERIVED_FROM`.
fn has_episode_lineage(store: &Store, skill_id: &Id) -> bool {
    let query = BoundQuery::new(
        "MATCH (s:Skill {id: $sid})-[:DERIVED_FROM]->(e:Episode) RETURN e.id AS id LIMIT 1",
    )
    .bind_str("sid", skill_id.as_str())
    .expect("bind sid");
    matches!(store.execute(&query).expect("lineage query"), QueryResult::Rows(rows) if rows.row_count() > 0)
}

#[tokio::test]
async fn repeated_procedure_induces_one_private_flagged_skill() {
    let store = store();
    insert_repeats(&store, PROCEDURE, Role::Assistant, &alice(), 3);
    let consolidator =
        induction_consolidator(&store, RuleInducer::with_default_rules(), enabled(3));

    drain(&consolidator).await;

    let skills = induced_skills(&store);
    assert_eq!(
        skills.len(),
        1,
        "three identical procedures induce exactly one skill"
    );
    let skill = &skills[0];
    assert!(skill.induced, "the skill is flagged induced");
    assert_eq!(
        skill.identity.namespace,
        alice(),
        "confined to the agent-private namespace"
    );
    assert_eq!(skill.body, PROCEDURE, "the body is the procedure verbatim");
    assert!(
        skill.name.starts_with("induced/"),
        "name is visibly induced: {}",
        skill.name
    );
    assert_eq!(skill.version, 1, "the first induced version");
    assert_eq!(skill.success_count, 0, "reliability is earned from zero");
    assert!(
        skill.deprecated_at.is_none(),
        "the induced version is active"
    );
    assert!(
        skill.problem_embedding.is_none(),
        "rule induction is lexical-only (no embedder)"
    );

    assert_eq!(
        induce_audits_for(&store, &skill.identity.id),
        1,
        "one InduceSkill audit"
    );
    assert!(
        has_episode_lineage(&store, &skill.identity.id),
        "skill links to its source episode"
    );
}

#[tokio::test]
async fn replay_does_not_duplicate_the_induced_skill() {
    let store = store();
    insert_repeats(&store, PROCEDURE, Role::Assistant, &alice(), 3);
    let consolidator =
        induction_consolidator(&store, RuleInducer::with_default_rules(), enabled(3));
    drain(&consolidator).await;
    assert_eq!(
        induced_skills(&store).len(),
        1,
        "first drain induces one skill"
    );

    // A fourth identical episode reconstructs the same content-addressed id → skip-on-replay.
    insert(&store, PROCEDURE, Role::Assistant, &alice(), 9);
    drain(&consolidator).await;
    let skills = induced_skills(&store);
    assert_eq!(
        skills.len(),
        1,
        "a later identical episode induces no duplicate"
    );
    assert_eq!(
        induce_audits_for(&store, &skills[0].identity.id),
        1,
        "the audit is not re-written on replay"
    );
}

#[tokio::test]
async fn threshold_above_window_still_induces() {
    // A misconfiguration where the threshold exceeds the recurrence window must not silently
    // disable induction: the probe window is clamped up to the threshold so the count can reach it.
    let store = store();
    insert_repeats(&store, PROCEDURE, Role::Assistant, &alice(), 3);
    let config = InductionConfig {
        enabled: true,
        repetition_threshold: 3,
        recurrence_window: 1,
        ..InductionConfig::default()
    };
    let consolidator = induction_consolidator(&store, RuleInducer::with_default_rules(), config);
    drain(&consolidator).await;
    assert_eq!(
        induced_skills(&store).len(),
        1,
        "threshold > window is clamped, not silently disabled"
    );
}

#[tokio::test]
async fn below_threshold_does_not_induce() {
    let store = store();
    insert_repeats(&store, PROCEDURE, Role::Assistant, &alice(), 2);
    let consolidator =
        induction_consolidator(&store, RuleInducer::with_default_rules(), enabled(3));
    drain(&consolidator).await;
    assert!(
        induced_skills(&store).is_empty(),
        "two recurrences is below the threshold of three"
    );
}

#[tokio::test]
async fn non_procedural_role_does_not_induce() {
    let store = store();
    insert_repeats(&store, PROCEDURE, Role::User, &alice(), 4);
    let consolidator =
        induction_consolidator(&store, RuleInducer::with_default_rules(), enabled(3));
    drain(&consolidator).await;
    assert!(
        induced_skills(&store).is_empty(),
        "a repeated user utterance is not a procedure"
    );
}

#[tokio::test]
async fn non_private_namespace_does_not_induce() {
    let store = store();
    insert_repeats(&store, PROCEDURE, Role::Assistant, &Namespace::Global, 4);
    let consolidator =
        induction_consolidator(&store, RuleInducer::with_default_rules(), enabled(3));
    drain(&consolidator).await;
    assert!(
        induced_skills(&store).is_empty(),
        "induction is refused outside a private namespace"
    );
}

#[tokio::test]
async fn thin_content_does_not_induce() {
    let store = store();
    // Long enough on characters but only one distinct token (below min_distinct_tokens).
    insert_repeats(
        &store,
        "retry retry retry retry retry retry",
        Role::Assistant,
        &alice(),
        4,
    );
    let consolidator =
        induction_consolidator(&store, RuleInducer::with_default_rules(), enabled(3));
    drain(&consolidator).await;
    assert!(
        induced_skills(&store).is_empty(),
        "content below the structure floor is not induced"
    );
}

#[tokio::test]
async fn disabled_by_default_induces_nothing_and_is_absent_from_rule_versions() {
    let store = store();
    insert_repeats(&store, PROCEDURE, Role::Assistant, &alice(), 4);
    // Default config: induction is OFF.
    let consolidator = induction_consolidator(
        &store,
        RuleInducer::with_default_rules(),
        InductionConfig::default(),
    );

    let rule_versions = consolidator.rule_versions();
    assert!(
        rule_versions.get("induce_skills").is_none(),
        "a disabled pass is excluded from the cursor's rule_versions: {rule_versions}"
    );

    drain(&consolidator).await;
    assert!(
        induced_skills(&store).is_empty(),
        "off by default induces nothing"
    );
}

#[tokio::test]
async fn rule_version_bump_cuts_a_new_version_and_deprecates_the_prior() {
    let store = store();

    // First inducer (v1) over three repeats → one active induced version.
    insert_repeats(&store, PROCEDURE, Role::Assistant, &alice(), 3);
    let v1 = induction_consolidator(&store, RuleInducer::new("induce-v1"), enabled(3));
    drain(&v1).await;
    let after_v1 = induced_skills(&store);
    assert_eq!(after_v1.len(), 1, "v1 induces one skill");
    let name = after_v1[0].name.clone();

    // A bumped inducer (v2) over fresh repeats of the SAME content → a new version under the
    // same name, with the prior version deprecated (deprecate-never-delete).
    insert_repeats(&store, PROCEDURE, Role::Assistant, &alice(), 10);
    let v2 = induction_consolidator(&store, RuleInducer::new("induce-v2"), enabled(3));
    drain(&v2).await;

    let mut versions = store.skill_versions(&name).expect("versions");
    versions.sort_by_key(|s| s.version);
    assert_eq!(
        versions.len(),
        2,
        "the rule-version bump cut a second version"
    );
    assert!(
        versions[0].deprecated_at.is_some(),
        "the prior version is deprecated"
    );
    assert!(
        versions[1].deprecated_at.is_none(),
        "the new version is active"
    );
    assert!(
        versions.iter().all(|s| s.induced),
        "both versions are flagged induced"
    );

    let (_, active) = store
        .active_skill(&name)
        .expect("active")
        .expect("an active version");
    assert_eq!(active.version, 2, "the active version is the bumped one");
}
