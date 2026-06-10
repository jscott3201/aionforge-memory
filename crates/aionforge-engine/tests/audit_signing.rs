//! End-to-end tests for live audit signing (06 §6, M4.T06 PR-5g2): the one engine branch
//! that reads `sign_audit_events` — custody + keyring provisioning, the commit-time
//! signer install, the genesis echo (and its dedup heal on restart), and the verified
//! read facade reporting `Checked(Valid)` for substrate-signed rows.
//!
//! Fixture rows are stamped from the same wall clock that anchors genesis (taken AFTER
//! construction, so `ingested_at` lands inside the must-sign window): the verifier binds
//! each row to the key whose validity window contains `ingested_at`, and a row dated
//! before genesis would sit before the cutover and never read `Valid`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::Identity;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{AuditVerification, Memory, MemoryConfig, SecurityGate};
use aionforge_store::{Store, StoreConfig};
use aionforge_trust::AuditStatus;
use serde_json::json;

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
struct NeverError;
impl std::fmt::Display for NeverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}
impl std::error::Error for NeverError {}
impl Embedder for FakeEmbedder {
    type Error = NeverError;
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

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

/// The real wall clock: the genesis anchor is stamped from the caller's `now`, and the
/// verifier binds rows by `ingested_at` against that anchor — so fixtures must use the
/// same clock (a hardcoded future date becomes a time bomb the day reality passes it).
fn wall_now() -> Timestamp {
    Timestamp::now()
}

/// A fresh, empty temp directory unique to `label` (no temp crate, mirrors the store
/// crate's persistence tests).
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aionforge-audit-signing-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn signing_config(data_dir: &std::path::Path) -> MemoryConfig {
    MemoryConfig {
        security: SecurityGate {
            sign_audit_events: true,
            audit_data_dir: Some(data_dir.to_path_buf()),
            ..SecurityGate::default()
        },
        ..MemoryConfig::default()
    }
}

fn migrated_store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-06-09T08:00:00-05:00[America/Chicago]"))
        .expect("migrate");
    Arc::new(store)
}

/// A capture-channel audit stamped NOW — after the genesis anchor taken at construction,
/// so it sits inside the must-sign window (key_for's admission bound is inclusive).
fn future_audit(key: &str, namespace: Namespace) -> AuditEvent {
    let at = wall_now();
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(key.as_bytes()),
            ingested_at: at.clone(),
            namespace,
            expired_at: None,
        },
        kind: AuditKind::Capture,
        subject_id: Id::from_content_hash(b"subject"),
        actor_id: Id::from_content_hash(b"actor"),
        payload: json!({ "k": key }),
        signature: String::new(),
        occurred_at: at,
    }
}

#[test]
fn enabling_signing_provisions_genesis_and_signs_author_events() {
    let dir = temp_dir("genesis");
    let store = migrated_store();
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        signing_config(&dir),
        &wall_now(),
    )
    .expect("memory with signing");

    // The keyring anchored on disk, genesis echoed into the store, self-signed.
    assert!(
        aionforge_trust::keyring_path(&dir).exists(),
        "the keyring file is the out-of-band anchor"
    );
    let genesis = store
        .audit_by_kind(AuditKind::KeyRotation, None, 10)
        .expect("read genesis");
    assert_eq!(genesis.events.len(), 1, "one genesis row");
    assert!(
        !genesis.events[0].signature.is_empty(),
        "genesis is self-signed"
    );

    // A blank author event is stamped at commit and reads back Checked(Valid) on the
    // verified facade for a principal who can see it.
    let alice = Principal::agent(Id::from_content_hash(b"alice"));
    let own = Namespace::Agent(alice.agent_id.to_string());
    let event = future_audit("signed-flow", own);
    let node = store.commit_audit(&event).expect("commit");
    let stored = store
        .audit_event_by_node_id(node)
        .expect("read")
        .expect("exists");
    assert!(!stored.signature.is_empty(), "stamped at commit");

    let page = memory
        .audit_history(&alice, &event.subject_id, None, 10)
        .expect("facade read");
    assert_eq!(page.records.len(), 1);
    assert_eq!(
        page.records[0].verification,
        AuditVerification::Checked(AuditStatus::Valid),
        "the substrate-signed row verifies against the keyring anchor"
    );
}

#[test]
fn a_restart_on_the_same_anchor_heals_idempotently() {
    // A REAL restart: the WAL-backed store is recovered fresh from disk, the held seed
    // matches the anchored keyring, and the genesis re-emit dedups against the recovered
    // row — one genesis forever, no duplicate, no error. (Two Memory instances over one
    // SHARED Store is a different, refused pattern: install-once fails the second loudly.)
    let dir = temp_dir("restart");
    let now = ts("2026-06-09T08:00:00-05:00[America/Chicago]");
    let store_config = StoreConfig {
        embedding_dimension: 4,
    };
    {
        let store = Arc::new(
            Store::open_persistent_migrated(&dir, store_config, &now).expect("open persistent"),
        );
        Memory::new(
            Arc::clone(&store),
            FakeEmbedder::new(),
            signing_config(&dir),
            &wall_now(),
        )
        .expect("first start");
        assert_eq!(
            store
                .audit_by_kind(AuditKind::KeyRotation, None, 10)
                .expect("read")
                .events
                .len(),
            1
        );
    }
    let recovered = Arc::new(Store::recover(&dir, store_config).expect("recover"));
    Memory::new(
        Arc::clone(&recovered),
        FakeEmbedder::new(),
        signing_config(&dir),
        &wall_now(),
    )
    .expect("restart over the same anchor");
    let genesis = recovered
        .audit_by_kind(AuditKind::KeyRotation, None, 10)
        .expect("read genesis");
    assert_eq!(
        genesis.events.len(),
        1,
        "genesis healed by dedup, not duplicated"
    );

    // The shared-store double-provision IS refused: a second Memory over the same
    // recovered store hits the install-once guard loudly.
    assert!(
        Memory::new(
            recovered,
            FakeEmbedder::new(),
            signing_config(&dir),
            &wall_now(),
        )
        .is_err(),
        "two engines may not double-provision one store"
    );
}

#[test]
fn signing_off_changes_nothing() {
    let dir = temp_dir("off");
    let store = migrated_store();
    let _memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        MemoryConfig::default(),
        &wall_now(),
    )
    .expect("memory without signing");

    assert!(
        !aionforge_trust::keyring_path(&dir).exists(),
        "no anchor is created when signing is off"
    );
    let event = future_audit("unsigned-flow", Namespace::System);
    let node = store.commit_audit(&event).expect("commit");
    let stored = store
        .audit_event_by_node_id(node)
        .expect("read")
        .expect("exists");
    assert!(stored.signature.is_empty(), "blank stays blank");
    assert!(
        store
            .audit_by_kind(AuditKind::KeyRotation, None, 10)
            .expect("read")
            .events
            .is_empty(),
        "no genesis row"
    );
}

#[test]
fn a_seed_that_does_not_match_the_anchor_is_refused_at_startup() {
    let dir = temp_dir("mismatch");
    let store = migrated_store();
    Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        signing_config(&dir),
        &wall_now(),
    )
    .expect("first start anchors the keyring");

    // Same anchor, different identity: an env-custody seed that is not the anchored key.
    let mut config = signing_config(&dir);
    config.security.audit_seed = Some(secrecy::SecretString::from(
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
    ));
    let second_store = migrated_store();
    let err = match Memory::new(second_store, FakeEmbedder::new(), config, &wall_now()) {
        Ok(_) => panic!("a mismatched seed must never sign against this anchor"),
        Err(err) => err,
    };
    assert!(
        format!("{err:?}").contains("AuditSigning"),
        "the failure is the provisioning gate: {err:?}"
    );
}
