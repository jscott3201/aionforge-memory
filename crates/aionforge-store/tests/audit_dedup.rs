//! Integration tests for the audit-write funnel (`audit::ensure_event`, M4.T06 PR-5e):
//! content-addressed dedup that RECONCILES the stored signature instead of treating
//! "row exists" as "write nothing".
//!
//! The attack this closes: `AuditEvent.id` is content-addressed over everything except
//! the signature, and the id is UNIQUE — so before the funnel, whichever copy landed
//! first owned the row forever. A pre-placed blank-signature copy of a predictable id
//! silently shadowed the later, legitimately signed emit, which read back as benign
//! "legacy unsigned".

mod common;

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{BoundQuery, NodeId, QueryResult, Store, StoreConfig};
use common::{stats, store, temp_dir, ts};
use serde_json::json;

fn now() -> Timestamp {
    ts("2026-06-09T09:00:00-05:00[America/Chicago]")
}

/// A content-addressed audit event (the deduped families: governance, consolidation,
/// attestation) with the caller's choice of signature.
fn content_addressed_audit(key: &str, signature: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(key.as_bytes()),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::NamespaceDenied,
        subject_id: Id::from_content_hash(b"subject"),
        actor_id: Id::from_content_hash(b"actor"),
        payload: json!({ "reason": "test" }),
        signature: signature.to_string(),
        occurred_at: now(),
    }
}

/// How many `AuditEvent` rows carry this domain id (the UNIQUE-backed dedup axis).
fn rows_with_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.id = $id RETURN a.id AS id")
        .bind_uuid("id", id)
        .expect("bind");
    match store.execute(&query).expect("query") {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The stored signature of the audit node, read back through the domain decoder.
fn stored_signature(store: &Store, node: NodeId) -> String {
    store
        .audit_event_by_node_id(node)
        .expect("read back")
        .expect("audit event exists")
        .signature
}

#[test]
fn a_blank_row_upgrades_to_signed_in_place() {
    // The shadow attack, replayed as a heal: a blank-signature copy lands first (today's
    // unsigned emits, or an attacker pre-placing the predictable content id), then the
    // legitimately signed copy of the SAME event arrives. The funnel must keep one row,
    // keep the node id, and upgrade the stored signature in place — not silently no-op.
    let store = store();
    let blank = content_addressed_audit("shadowed-event", "");
    let signed = content_addressed_audit("shadowed-event", "c2lnbmF0dXJl");

    let first = store.commit_audit(&blank).expect("commit blank");
    let second = store.commit_audit(&signed).expect("commit signed");

    assert_eq!(first, second, "the dedup reuses the existing node");
    assert_eq!(
        rows_with_id(&store, &blank.identity.id),
        1,
        "one row per content-addressed id — the UNIQUE axis holds"
    );
    assert_eq!(
        stored_signature(&store, first),
        "c2lnbmF0dXJl",
        "the blank stored signature upgraded to the signed copy's"
    );
}

#[test]
fn a_replay_with_the_same_signature_is_a_noop() {
    // Ed25519 is deterministic (RFC 8032), so a crash-replay re-signs identical bytes:
    // the funnel sees stored == incoming and must change nothing — same node, same row
    // count, same signature. (This holds whatever the reconcile policy does elsewhere:
    // for equal copies, Keep and Upgrade are byte-identical outcomes.)
    let store = store();
    let signed = content_addressed_audit("replayed-event", "c2lnbmF0dXJl");

    let first = store.commit_audit(&signed).expect("first commit");
    let second = store.commit_audit(&signed).expect("verbatim replay");

    assert_eq!(first, second, "the replay reuses the node");
    assert_eq!(rows_with_id(&store, &signed.identity.id), 1);
    assert_eq!(stored_signature(&store, first), "c2lnbmF0dXJl");
}

#[test]
fn a_signed_row_never_downgrades_to_blank() {
    // The inverse of the shadow heal: once a row carries a signature, a blank re-emit of
    // the same content (signing later disabled, or an attacker re-sending blank copies)
    // must not strip it. Proof is monotone — the latch only ever closes.
    let store = store();
    let signed = content_addressed_audit("latched-event", "c2lnbmF0dXJl");
    let blank = content_addressed_audit("latched-event", "");

    let first = store.commit_audit(&signed).expect("commit signed");
    let second = store.commit_audit(&blank).expect("blank re-emit");

    assert_eq!(first, second, "the dedup reuses the node");
    assert_eq!(rows_with_id(&store, &signed.identity.id), 1);
    assert_eq!(
        stored_signature(&store, first),
        "c2lnbmF0dXJl",
        "the stored signature survives a blank re-emit — no silent downgrade"
    );
}

#[test]
fn a_conflicting_signature_keeps_the_stored_one() {
    // Two signed copies that disagree (policy case 4): reachable honestly by a verbatim
    // crash-heal re-signed AFTER a key rotation. The verifier binds the row to the key
    // whose validity window contains the STORED `ingested_at`, which a dedup hit never
    // re-stamps — so the stored signature is the one that verifies, and overwriting it
    // with newer-key bytes would flip the row Valid -> Invalid.
    let store = store();
    let original = content_addressed_audit("contested-event", "b3JpZ2luYWw=");
    let conflicting = content_addressed_audit("contested-event", "Y29uZmxpY3Q=");

    let first = store.commit_audit(&original).expect("commit original");
    let second = store
        .commit_audit(&conflicting)
        .expect("conflicting re-emit");

    assert_eq!(first, second, "the dedup reuses the node");
    assert_eq!(rows_with_id(&store, &original.identity.id), 1);
    assert_eq!(
        stored_signature(&store, first),
        "b3JpZ2luYWw=",
        "the first signature wins — it is the one the stored ingested_at's key window matches"
    );
}

#[test]
fn the_retry_budget_count_is_invariant_to_signature_status() {
    // `count_consolidation_failures` is the durable retry budget the scheduler reads to
    // decide retry-vs-fatal (a poison-pill episode must escalate even across restarts).
    // The adversarial-review must-fix for PR-5e: that count must be PROVABLY invariant to
    // signature/verification status — a blank, signed, or tampered-signature failure row
    // all count, and a signature upgrade on one row must not change the count. Dropping a
    // "suspicious" row here would hand a poison-pill episode a fresh retry budget.
    let store = store();
    let (node_id, episode) = insert_episode(&store, "doomed");

    // Three failure attempts in three signature states: blank (pre-signing), well-formed
    // base64, and garbage bytes that no verifier would accept.
    let blank = failure_audit(&episode.identity.id, 1, "");
    let signed = failure_audit(&episode.identity.id, 2, "c2lnbmF0dXJl");
    let garbage = failure_audit(&episode.identity.id, 3, "!!not-base64!!");
    for audit in [&blank, &signed, &garbage] {
        store
            .record_consolidation_failure(node_id, audit, false)
            .expect("record failure");
    }
    assert_eq!(
        store
            .count_consolidation_failures(&episode.identity.id)
            .expect("count"),
        3,
        "every failure row counts, whatever its signature state"
    );

    // A signed re-emit of the blank attempt (same content id) heals the signature in
    // place — and the count must not move: the upgrade is an update, never a new row.
    let healed = failure_audit(&episode.identity.id, 1, "aGVhbGVk");
    store
        .record_consolidation_failure(node_id, &healed, false)
        .expect("re-record signed");
    assert_eq!(
        store
            .count_consolidation_failures(&episode.identity.id)
            .expect("count"),
        3,
        "the in-place signature upgrade does not change the retry budget"
    );
}

/// A deterministic stand-in for the substrate audit signer (the real one is Ed25519 in
/// aionforge-trust): object-safe, content-derived output, no crypto.
#[derive(Debug)]
struct FakeSigner;
impl aionforge_domain::verify::AuditEventSigner for FakeSigner {
    fn sign(&self, event: &AuditEvent) -> String {
        format!("fake-sig|{}", event.identity.id)
    }
}

#[test]
fn an_installed_signer_stamps_blank_audit_writes_at_commit() {
    // The 5g chokepoint contract: the signer lives on the Store and the write funnel
    // stamps every blank-signature event at commit time — no author site opts in or out.
    let store = store();
    store
        .install_audit_signer(std::sync::Arc::new(FakeSigner))
        .expect("first install");
    let event = content_addressed_audit("signed-at-commit", "");

    let node = store.commit_audit(&event).expect("commit");
    assert_eq!(
        stored_signature(&store, node),
        format!("fake-sig|{}", event.identity.id),
        "the blank event was stamped inside the commit, not by the author"
    );

    // The author's verbatim replay re-signs deterministically (the trait contract) and
    // dedups into the same row — sign-before-probe plus the latch keep it a no-op.
    let replay = store.commit_audit(&event).expect("replay");
    assert_eq!(node, replay, "the replay reuses the row");
    assert_eq!(rows_with_id(&store, &event.identity.id), 1);

    // Install-once is structural: a second signer is refused loudly, so two signers can
    // never share one store's life.
    assert!(
        store
            .install_audit_signer(std::sync::Arc::new(FakeSigner))
            .is_err(),
        "a second install must be refused"
    );
}

#[test]
fn an_installed_signer_heals_a_pre_placed_blank_shadow() {
    // The end-to-end shadow heal: a blank copy lands while signing is off (or is
    // attacker-pre-placed), signing is then enabled, and the author's deterministic
    // re-emit upgrades the stored row through the latch — same node, one row.
    let store = store();
    let event = content_addressed_audit("healed-shadow", "");
    let first = store.commit_audit(&event).expect("blank shadow lands");

    store
        .install_audit_signer(std::sync::Arc::new(FakeSigner))
        .expect("first install");
    let second = store.commit_audit(&event).expect("signed re-emit");

    assert_eq!(first, second, "the heal reuses the row");
    assert_eq!(rows_with_id(&store, &event.identity.id), 1);
    assert_eq!(
        stored_signature(&store, first),
        format!("fake-sig|{}", event.identity.id),
        "the shadow row now carries the commit-time signature"
    );
}

#[test]
fn an_already_signed_event_passes_through_an_installed_signer() {
    // An author-signed event (KeyRotation is signed by a SPECIFIC key, possibly the
    // outgoing one during rotation) must reach the store byte-identical: the commit-time
    // stamp covers only blank signatures, it never re-signs.
    let store = store();
    store
        .install_audit_signer(std::sync::Arc::new(FakeSigner))
        .expect("first install");
    let event = content_addressed_audit("author-signed", "b3V0Z29pbmc=");

    let node = store.commit_audit(&event).expect("commit");
    assert_eq!(
        stored_signature(&store, node),
        "b3V0Z29pbmc=",
        "the author's signature survives — the stamp never replaces"
    );
}

#[test]
fn recovery_refuses_a_pre_latch_immutable_signature_schema() {
    // A store whose persisted DDL predates the latch declares `AuditEvent.signature`
    // IMMUTABLE. Recovery replays the PERSISTED statements (not the compiled-in
    // catalog), `migrate()` is version-guarded, and the engine has no `ALTER TYPE` —
    // so on such a binding the blank -> signed heal would surface as an
    // `ImmutablePropertyUpdate` aborting whole write transactions at arbitrary later
    // commits. The open-time latch check must refuse the binding loudly instead.
    let dir = temp_dir("pre-latch-audit-schema");
    {
        let store = Store::open_persistent(&dir, StoreConfig::default()).expect("open fresh store");
        // The pre-latch AuditEvent declaration, verbatim from the pre-M4.T06 catalog.
        store
            .execute(&BoundQuery::new(
                r#"CREATE NODE TYPE IF NOT EXISTS :AuditEvent (
                    id :: UUID NOT NULL UNIQUE IMMUTABLE,
                    ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
                    namespace :: STRING NOT NULL,
                    expired_at :: ZONED DATETIME,
                    kind :: STRING NOT NULL,
                    subject_id :: UUID NOT NULL,
                    actor_id :: UUID NOT NULL,
                    payload :: JSON NOT NULL,
                    signature :: STRING NOT NULL IMMUTABLE,
                    occurred_at :: ZONED DATETIME NOT NULL IMMUTABLE
                ) STRICT"#,
            ))
            .expect("declare the pre-latch type");
    }
    let err = Store::recover(&dir, StoreConfig::default(), &now())
        .expect_err("a pre-latch binding must be refused at open, not at a later commit");
    assert!(
        err.to_string().contains("AuditEvent.signature"),
        "the refusal names the drifted property: {err}"
    );
}

/// A `consolidation_failed` audit content-keyed on (episode, attempt), with the caller's
/// choice of signature. The id MIRRORS the scheduler's `failure_audit_id`
/// (aionforge-consolidate scheduler.rs: `consolidation_failed|{episode_id}|{attempt}`) —
/// the scheduler's fn is private, so this is a copy that can drift; the invariant under
/// test (count keys on `subject_id` + `kind`, never the id or signature) holds for any
/// id, so drift would not silently weaken the test.
fn failure_audit(subject: &Id, attempt: u32, signature: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(
                format!("consolidation_failed|{subject}|{attempt}").as_bytes(),
            ),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::ConsolidationFailed,
        subject_id: *subject,
        actor_id: Id::from_content_hash(b"scheduler"),
        payload: json!({ "pass": "noop", "reason": "boom", "attempt": attempt }),
        signature: signature.to_string(),
        occurred_at: now(),
    }
}

fn insert_episode(store: &Store, content: &str) -> (NodeId, Episode) {
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: now(),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    let node_id = store.insert_episode(&episode).expect("insert episode");
    (node_id, episode)
}
