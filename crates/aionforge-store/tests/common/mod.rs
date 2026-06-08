//! Shared fixtures for the maintained current-state provider acceptance tests
//! (data-model §9, M2.T02). Split across `providers.rs` (membership via the typed
//! M2.T01 ops) and `provider_grounding.rs` (the grounded/scope/recency sets that need
//! edges no typed writer exists for yet). Each test binary uses a subset, so dead-code
//! is allowed here rather than per-item.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::{About, Contradicts, SupersededBy};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, CandidateSet, NodeId, Store, StoreConfig, Value};

pub fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

/// A bound `ZONED DATETIME` value for GQL fixtures (the engine value, not the domain
/// [`Timestamp`]). `Timestamp` is a `jiff::Zoned`, so this just boxes one.
pub fn zdt() -> Value {
    Value::ZonedDateTime(Box::new(ts("2026-06-06T12:00:00-05:00[America/Chicago]")))
}

pub fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

/// A fresh, empty temp directory unique to `label`, removed first so re-runs start
/// clean. Mirrors the no-temp-crate convention in `persistence.rs`.
pub fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aionforge-providers-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

pub fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

pub fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
        namespace: Namespace::Agent("alice".to_string()),
        expired_at: None,
    }
}

pub fn entity(name: &str) -> Entity {
    Entity {
        identity: identity(Id::generate()),
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    }
}

pub fn fact(subject: Id, predicate: &str, object: ObjectValue, statement: &str) -> Fact {
    Fact {
        identity: identity(Id::generate()),
        stats: stats(),
        subject_id: subject,
        predicate: predicate.to_string(),
        object,
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

/// An open (current, live) validity window starting at `from`.
pub fn open_window(from: &str) -> About {
    About {
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    }
}

pub fn superseded_by(reason: &str, from: &str) -> SupersededBy {
    SupersededBy {
        reason: reason.to_string(),
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    }
}

pub fn contradicts(by: &str, from: &str) -> Contradicts {
    Contradicts {
        detected_by: by.to_string(),
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    }
}

/// The current membership of `set` as a node-id set, via the typed accessor.
pub fn members(store: &Store, set: CandidateSet) -> BTreeSet<NodeId> {
    store
        .candidate_state_members(set)
        .expect("candidate-state members")
        .into_iter()
        .collect()
}

/// The current `current_support_facts` membership mapped to domain id strings. Domain
/// ids are stable across recovery (node ids are an engine-internal currency), so this
/// is the right key for asserting set identity survives a restart.
pub fn current_fact_ids(store: &Store) -> BTreeSet<String> {
    store
        .candidate_state_members(CandidateSet::CurrentSupportFacts)
        .expect("members")
        .into_iter()
        .map(|node| {
            store
                .fact_by_node_id(node)
                .expect("read fact")
                .expect("member is a live Fact")
                .identity
                .id
                .to_string()
        })
        .collect()
}

/// Assert a fact about a freshly inserted subject entity, returning its node id.
pub fn assert_about(store: &Store, subject: &Entity, f: &Fact, window: &About) -> NodeId {
    let subject_node = store.insert_entity(subject).expect("insert subject entity");
    store
        .assert_fact(f, subject_node, window)
        .expect("assert fact")
}

// --- GQL fixtures for the edges/grounding the typed M2.T01 ops do not yet write ------
//
// IN_SCOPE / RECENT_IN / SUPPORTS / HAS_PROVENANCE and their endpoint nodes (Scope,
// RecencyWindow, ProvenanceRecord) get their own typed writers in later milestones
// (M2.T04/T05/T08). Until then these tests wire them straight through the engine's
// parameter-bound write path, which is a real commit through the same funnel — every
// value is a bound parameter; only the trusted static labels sit in the source.

/// Insert a minimal valid `Scope` (every `NOT NULL` field bound).
pub fn insert_scope(store: &Store, id: &Id) {
    let query = BoundQuery::new(
        "INSERT (s:Scope {id: $id, ingested_at: $ts, namespace: $ns, name: $name, scope_kind: $kind})",
    )
    .bind_uuid("id", id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:alice")
    .unwrap()
    .bind_str("name", "test-scope")
    .unwrap()
    .bind_str("kind", "task")
    .unwrap();
    store.execute(&query).expect("insert scope");
}

/// Insert a minimal valid `RecencyWindow` (every `NOT NULL` field bound).
pub fn insert_recency_window(store: &Store, id: &Id) {
    let query = BoundQuery::new(
        "INSERT (w:RecencyWindow {id: $id, ingested_at: $ts, namespace: $ns, label: $label})",
    )
    .bind_uuid("id", id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:alice")
    .unwrap()
    .bind_str("label", "last-hour")
    .unwrap();
    store.execute(&query).expect("insert recency window");
}

/// Insert a minimal valid `ProvenanceRecord` (every `NOT NULL` field bound).
pub fn insert_provenance(store: &Store, id: &Id, subject: &Id) {
    let query = BoundQuery::new(
        "INSERT (p:ProvenanceRecord {id: $id, ingested_at: $ts, namespace: $ns, \
         subject_id: $subj, writer_agent_id: $writer, signature: $sig, \
         model_family: $mf, trust_at_write: $tw})",
    )
    .bind_uuid("id", id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:alice")
    .unwrap()
    .bind_uuid("subj", subject)
    .unwrap()
    .bind_uuid("writer", Id::from_content_hash(b"agent:alice"))
    .unwrap()
    .bind_str("sig", "signature-bytes")
    .unwrap()
    .bind_str("mf", "test-model")
    .unwrap()
    .bind("tw", Value::Float(0.5))
    .unwrap();
    store.execute(&query).expect("insert provenance record");
}

/// Insert a property-free edge of fixed `source` between two ids bound as parameters.
pub fn insert_edge(store: &Store, source: &str, from: &Id, to: &Id) {
    let query = BoundQuery::new(source)
        .bind_uuid("from", from)
        .unwrap()
        .bind_uuid("to", to)
        .unwrap();
    store.execute(&query).expect("insert edge");
}
