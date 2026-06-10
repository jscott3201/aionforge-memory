//! End-to-end tests for the audit read facade on `Memory` (06 §6, M4.T06 PR-5f): the
//! namespace scoping (the M4.T01 visible-set rule applied to audit rows), the refill
//! pagination above hidden ranges, and the `NotEnabled` verification mapping that holds
//! until audit signing is wired (PR-5g).

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::Identity;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{AuditVerification, Memory, MemoryConfig};
use aionforge_store::{Store, StoreConfig};
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

fn memory() -> Memory<FakeEmbedder> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-06-09T08:00:00-05:00[America/Chicago]"))
        .expect("migrate");
    Memory::new(
        Arc::new(store),
        FakeEmbedder::new(),
        MemoryConfig::default(),
        &ts("2026-06-09T08:00:00-05:00[America/Chicago]"),
    )
    .expect("memory")
}

/// An audit event about `subject` in `namespace`, occurring at minute `minute` (distinct
/// instants give the keyset pagination a stable spine).
fn audit_at(subject: &Id, namespace: Namespace, minute: u32) -> AuditEvent {
    let occurred = ts(&format!(
        "2026-06-09T09:{minute:02}:00-05:00[America/Chicago]"
    ));
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(format!("evt|{subject}|{namespace}|{minute}").as_bytes()),
            ingested_at: occurred.clone(),
            namespace,
            expired_at: None,
        },
        kind: AuditKind::Capture,
        subject_id: *subject,
        actor_id: Id::from_content_hash(b"actor"),
        payload: json!({ "minute": minute }),
        signature: String::new(),
        occurred_at: occurred,
    }
}

#[test]
fn audit_reads_scope_to_the_principals_visible_namespaces() {
    // The M4.T01 read rule applied to audit rows: an agent sees global plus its own
    // private namespace; system (where governance audits live) is never agent-visible.
    // The hidden rows must not shorten the page — the refill loop drives the L0 reader
    // until the limit is met or history ends, and the cursor stays continuable.
    let memory = memory();
    let subject = Id::from_content_hash(b"subject-fact");
    let alice = Principal::agent(Id::from_content_hash(b"alice"));
    let own = Namespace::Agent(alice.agent_id.to_string());

    // Interleave visible and hidden rows across the keyset order.
    for (minute, namespace) in [
        (1, own.clone()),
        (2, Namespace::System),
        (3, Namespace::Global),
        (4, Namespace::System),
        (5, own.clone()),
        (6, Namespace::Agent("bob".to_string())),
        (7, Namespace::Global),
    ] {
        memory
            .store()
            .commit_audit(&audit_at(&subject, namespace, minute))
            .expect("commit audit");
    }

    // One big page: exactly the 5 visible rows (2 own + 2 global + 1 own), system and
    // bob's namespace filtered out, oldest first.
    let page = memory
        .audit_history(&alice, &subject, None, 50)
        .expect("read history");
    let minutes: Vec<i64> = page
        .records
        .iter()
        .map(|r| r.event.payload["minute"].as_i64().expect("minute"))
        .collect();
    assert_eq!(minutes, vec![1, 3, 5, 7], "visible rows only, in order");
    assert!(page.next.is_none(), "history exhausted");
    assert!(
        page.records
            .iter()
            .all(|r| r.verification == AuditVerification::NotEnabled),
        "signing is not wired (PR-5g): every row reads NotEnabled, never a checked verdict"
    );

    // Page size 2 with hidden rows interleaved: pages stay FULL (the refill loop) and
    // the cursor walks the whole history without skipping or repeating a visible row.
    let first = memory
        .audit_history(&alice, &subject, None, 2)
        .expect("page 1");
    assert_eq!(
        first
            .records
            .iter()
            .map(|r| r.event.payload["minute"].as_i64().expect("minute"))
            .collect::<Vec<_>>(),
        vec![1, 3],
        "page 1 is full despite the hidden system row between"
    );
    let second = memory
        .audit_history(&alice, &subject, first.next.as_ref(), 2)
        .expect("page 2");
    assert_eq!(
        second
            .records
            .iter()
            .map(|r| r.event.payload["minute"].as_i64().expect("minute"))
            .collect::<Vec<_>>(),
        vec![5, 7],
        "page 2 continues exactly after page 1's last consumed row"
    );
    assert!(
        second.next.is_none(),
        "the limit-th row is also the last row of history: exact exhaustion reads \
         next None (the L0 paginate contract), not a wasted empty continuation"
    );
}

#[test]
fn a_trailing_hidden_row_keeps_the_continuation_cursor() {
    // When the page fills but a HIDDEN row still follows, the facade cannot know the
    // remainder is invisible without reading it — the cursor stays Some and the final
    // continuation drains the hidden tail into an empty, exhausted page.
    let memory = memory();
    let subject = Id::from_content_hash(b"tail-subject");
    let alice = Principal::agent(Id::from_content_hash(b"alice"));
    let own = Namespace::Agent(alice.agent_id.to_string());

    memory
        .store()
        .commit_audit(&audit_at(&subject, own, 1))
        .expect("own row");
    memory
        .store()
        .commit_audit(&audit_at(&subject, Namespace::System, 2))
        .expect("hidden tail row");

    let first = memory
        .audit_history(&alice, &subject, None, 1)
        .expect("page 1");
    assert_eq!(first.records.len(), 1);
    assert!(
        first.next.is_some(),
        "a row follows the full page (hidden, but unknowable without reading it)"
    );
    let second = memory
        .audit_history(&alice, &subject, first.next.as_ref(), 1)
        .expect("page 2");
    assert!(second.records.is_empty(), "the tail is hidden");
    assert!(second.next.is_none(), "exhausted");
}

#[test]
fn audit_by_kind_and_subject_kind_apply_the_same_scope() {
    // The kind-scoped axes go through the same visibility filter: a system-namespace
    // governance row of the requested kind stays invisible to an agent principal.
    let memory = memory();
    let subject = Id::from_content_hash(b"kind-subject");
    let alice = Principal::agent(Id::from_content_hash(b"alice"));
    let own = Namespace::Agent(alice.agent_id.to_string());

    memory
        .store()
        .commit_audit(&audit_at(&subject, own, 1))
        .expect("own row");
    memory
        .store()
        .commit_audit(&audit_at(&subject, Namespace::System, 2))
        .expect("system row");

    let by_kind = memory
        .audit_by_kind(&alice, AuditKind::Capture, None, 10)
        .expect("by kind");
    assert_eq!(by_kind.records.len(), 1, "system row filtered");

    let by_subject_kind = memory
        .audit_by_subject_kind(&alice, &subject, AuditKind::Capture, None, 10)
        .expect("by subject+kind");
    assert_eq!(by_subject_kind.records.len(), 1, "system row filtered");
    assert_eq!(
        by_subject_kind.records[0].event.payload["minute"]
            .as_i64()
            .expect("minute"),
        1
    );
}
