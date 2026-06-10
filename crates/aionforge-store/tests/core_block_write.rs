//! Store-level tests for the core-block write surface (05 §4, M5.T04): the un-attested
//! genesis create, the in-place attested whole-value edit, the carry-forward drift
//! baseline, the honest removal of a stale embedding, attester dedup, and the typed
//! not-live refusal probed under the write lock.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::AttestedBy;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{
    CoreAttestation, CoreBlockReplacement, CoreEditWrite, NodeId, Store, StoreConfig,
};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
}

fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

fn owner_ns() -> Namespace {
    Namespace::Agent("identity-owner".to_string())
}

fn block(content: &str, kind: BlockKind) -> CoreBlock {
    CoreBlock {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace: owner_ns(),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.95,
            trust: 0.9,
            last_access: now(),
            access_count_recent: 1,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        block_kind: kind,
        sensitivity: None,
        drift_baseline: None,
        embedding: None,
        embedder_model: None,
    }
}

fn core_edit_audit(subject: &Id, seed: &[u8]) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(seed),
            ingested_at: now(),
            namespace: owner_ns(),
            expired_at: None,
        },
        kind: AuditKind::CoreEdit,
        subject_id: *subject,
        actor_id: Id::from_content_hash(b"core-editor"),
        payload: serde_json::json!({"reason": "test"}),
        signature: String::new(),
        occurred_at: now(),
    }
}

fn enroll_attester(store: &Store, seed: &[u8]) -> NodeId {
    let agent = Agent {
        identity: Identity {
            id: Id::from_content_hash(seed),
            ingested_at: now(),
            namespace: Namespace::Agent("attester".to_string()),
            expired_at: None,
        },
        public_key: "cHVibGljLWtleQ==".to_string(),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll attester")
}

fn vote() -> AttestedBy {
    AttestedBy {
        attested_at: now(),
        signature: "sig".to_string(),
        category: None,
    }
}

fn replacement(content: &str) -> CoreBlockReplacement {
    CoreBlockReplacement {
        content: content.to_string(),
        drift_baseline: None,
        embedding: None,
    }
}

fn block_node(store: &Store, id: &Id) -> NodeId {
    store
        .memory_by_id(id, &["CoreBlock"])
        .expect("resolve")
        .expect("live block")
        .node
}

#[test]
fn create_writes_the_block_and_its_genesis_audit_atomically() {
    let store = store();
    let b = block("I keep user data confidential.", BlockKind::Redline);
    store
        .create_core_block(&b, &core_edit_audit(&b.identity.id, b"genesis"))
        .expect("create");

    let read = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read, b, "the genesis block round-trips");

    let rows = store
        .audit_by_kind(AuditKind::CoreEdit, None, 10)
        .expect("audit")
        .events;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].subject_id, b.identity.id);
}

#[test]
fn an_edit_swaps_content_in_place_on_the_same_stable_node() {
    let store = store();
    let mut b = block("I respond tersely.", BlockKind::Persona);
    b.drift_baseline = Some(serde_json::json!({"summary": "terse"}));
    store
        .create_core_block(&b, &core_edit_audit(&b.identity.id, b"genesis"))
        .expect("create");
    let node = block_node(&store, &b.identity.id);
    let attester = enroll_attester(&store, b"second-attester");

    let outcome = store
        .edit_core_block(
            node,
            &replacement("I respond thoroughly, with sources."),
            &[CoreAttestation {
                attester,
                edge: vote(),
            }],
            &core_edit_audit(&b.identity.id, b"edit-1"),
        )
        .expect("edit");
    let CoreEditWrite::Applied {
        attesters_recorded, ..
    } = outcome
    else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(attesters_recorded, 1);

    // Same id, same single node: in-place whole-value replacement, never a version
    // chain — and the untouched drift baseline carried forward.
    let read = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read.content, "I respond thoroughly, with sources.");
    assert_eq!(
        read.drift_baseline,
        Some(serde_json::json!({"summary": "terse"})),
        "an ordinary content edit never re-baselines drift"
    );
    assert_eq!(
        store.live_core_blocks().expect("scan").len(),
        1,
        "one block is one node for its whole life"
    );
    assert_eq!(
        store.distinct_attesters(node).expect("attesters").len(),
        1,
        "the attester's vote is on the block"
    );
    assert_eq!(
        store
            .audit_by_kind(AuditKind::CoreEdit, None, 10)
            .expect("audit")
            .events
            .len(),
        2,
        "genesis and edit each have their row"
    );
}

#[test]
fn the_drift_baseline_updates_only_when_the_caller_rebaselines() {
    let store = store();
    let b = block("I cite sources.", BlockKind::Commitment);
    store
        .create_core_block(&b, &core_edit_audit(&b.identity.id, b"genesis"))
        .expect("create");
    let node = block_node(&store, &b.identity.id);
    let attester = enroll_attester(&store, b"second-attester");

    let mut rebaseline = replacement("I cite primary sources.");
    rebaseline.drift_baseline = Some(serde_json::json!({"summary": "primary sources"}));
    store
        .edit_core_block(
            node,
            &rebaseline,
            &[CoreAttestation {
                attester,
                edge: vote(),
            }],
            &core_edit_audit(&b.identity.id, b"edit-rebaseline"),
        )
        .expect("edit");

    let read = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(
        read.drift_baseline,
        Some(serde_json::json!({"summary": "primary sources"}))
    );
}

#[test]
fn a_stale_embedding_is_removed_rather_than_served() {
    let store = store();
    let mut b = block("I avoid speculation.", BlockKind::Commitment);
    b.embedding = Some(Embedding::new(vec![0.1, 0.2, 0.3, 0.4]).expect("embedding"));
    b.embedder_model = Some(EmbedderModel {
        family: "fake".to_string(),
        version: "1".to_string(),
        dimension: 4,
    });
    store
        .create_core_block(&b, &core_edit_audit(&b.identity.id, b"genesis"))
        .expect("create");
    let node = block_node(&store, &b.identity.id);
    let attester = enroll_attester(&store, b"second-attester");

    // No fresh embedding supplied: the old vector indexes the old content, so it is
    // removed, not left to match text the block no longer says.
    store
        .edit_core_block(
            node,
            &replacement("I label speculation clearly when asked for it."),
            &[CoreAttestation {
                attester,
                edge: vote(),
            }],
            &core_edit_audit(&b.identity.id, b"edit-no-embedding"),
        )
        .expect("edit");
    let read = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read.embedding, None);
    assert_eq!(read.embedder_model, None);

    // A fresh pair supplied: the swap carries the new-content vector.
    let mut with_vector = replacement("I label speculation clearly.");
    with_vector.embedding = Some((
        Embedding::new(vec![0.4, 0.3, 0.2, 0.1]).expect("embedding"),
        EmbedderModel {
            family: "fake".to_string(),
            version: "2".to_string(),
            dimension: 4,
        },
    ));
    store
        .edit_core_block(
            node,
            &with_vector,
            &[CoreAttestation {
                attester,
                edge: vote(),
            }],
            &core_edit_audit(&b.identity.id, b"edit-with-embedding"),
        )
        .expect("edit");
    let read = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert!(read.embedding.is_some());
    assert_eq!(read.embedder_model.expect("model").version, "2");
}

#[test]
fn duplicate_attestations_collapse_to_one_recorded_vote() {
    let store = store();
    let b = block("I am candid about uncertainty.", BlockKind::Persona);
    store
        .create_core_block(&b, &core_edit_audit(&b.identity.id, b"genesis"))
        .expect("create");
    let node = block_node(&store, &b.identity.id);
    let attester = enroll_attester(&store, b"second-attester");

    let outcome = store
        .edit_core_block(
            node,
            &replacement("I am candid about uncertainty, with calibration."),
            &[
                CoreAttestation {
                    attester,
                    edge: vote(),
                },
                CoreAttestation {
                    attester,
                    edge: vote(),
                },
            ],
            &core_edit_audit(&b.identity.id, b"edit-dup"),
        )
        .expect("edit");
    let CoreEditWrite::Applied {
        attesters_recorded, ..
    } = outcome
    else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(
        attesters_recorded, 1,
        "one agent is one vote no matter how many times it appears"
    );
    assert_eq!(store.distinct_attesters(node).expect("attesters").len(), 1);
}

#[test]
fn editing_a_retired_or_purged_block_is_the_typed_not_live() {
    let store = store();
    let attester = enroll_attester(&store, b"second-attester");

    // Retired: expired_at set at genesis.
    let mut retired = block("an already-retired stance", BlockKind::Persona);
    retired.identity.expired_at = Some(now());
    store
        .create_core_block(
            &retired,
            &core_edit_audit(&retired.identity.id, b"genesis-r"),
        )
        .expect("create");
    let retired_node = store
        .memory_by_id(&retired.identity.id, &["CoreBlock"])
        .expect("resolve")
        .expect("present")
        .node;
    let outcome = store
        .edit_core_block(
            retired_node,
            &replacement("must not apply"),
            &[CoreAttestation {
                attester,
                edge: vote(),
            }],
            &core_edit_audit(&retired.identity.id, b"edit-retired"),
        )
        .expect("call");
    assert_eq!(outcome, CoreEditWrite::NotLive);
    assert_eq!(
        store
            .core_block_by_id(&retired.identity.id)
            .expect("read")
            .expect("present")
            .content,
        "an already-retired stance",
        "a refused edit touches nothing"
    );

    // Purged: the erasure path destroyed the node between resolve and edit.
    let doomed = block("a purged identity", BlockKind::Persona);
    store
        .create_core_block(&doomed, &core_edit_audit(&doomed.identity.id, b"genesis-d"))
        .expect("create");
    let doomed_node = block_node(&store, &doomed.identity.id);
    let purge_audit = AuditEvent {
        kind: AuditKind::Purge,
        ..core_edit_audit(&doomed.identity.id, b"purge-d")
    };
    store
        .hard_purge(&[doomed_node], &purge_audit)
        .expect("purge");
    let outcome = store
        .edit_core_block(
            doomed_node,
            &replacement("must not apply"),
            &[CoreAttestation {
                attester,
                edge: vote(),
            }],
            &core_edit_audit(&doomed.identity.id, b"edit-purged"),
        )
        .expect("call");
    assert_eq!(outcome, CoreEditWrite::NotLive);
    assert_eq!(
        store
            .audit_by_kind(AuditKind::CoreEdit, None, 10)
            .expect("audit")
            .events
            .len(),
        2,
        "only the two genesis audits exist; refused edits audit nothing"
    );
}
