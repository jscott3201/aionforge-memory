//! Shared fixtures for the `read_memory` tool tests.
//!
//! A hermetic in-memory [`Memory`] over a fake embedder, an admin-capable authority, and
//! one seed helper per lifecycle kind (each writing straight into the store, bypassing the
//! Capturer, and returning the node's domain id). Split across two test binaries
//! (`read_memory.rs` — episode/contract tests; `read_memory_multikind.rs` — the all-kinds
//! tests), so each binary uses a subset and unused items here are expected per binary.

#![allow(dead_code, unused_imports)]

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::authz::{
    AuthorizationError, Authorizer, DefaultAuthorizer, Principal, VisibleSet,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::ContentHash;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, ProvenanceRecord};
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::nodes::work::WorkItem;
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::ReadMemoryToolParams;
use aionforge_store::{MaterializedNote, Store, StoreConfig};

// Re-exported so the test binaries only need `use read_memory_support::*;` to name the
// literals + entry points they assert on.
pub use aionforge_domain::ids::Id;
pub use aionforge_domain::namespace::Namespace;
pub use aionforge_domain::nodes::core::BlockKind;
pub use aionforge_domain::nodes::episodic::Role;
pub use aionforge_domain::nodes::semantic::FactStatus;
pub use aionforge_domain::nodes::work::WorkStatus;
pub use aionforge_mcp::{AuthEnabled, read_memory_tool};

#[derive(Clone)]
pub struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    pub fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

impl Default for FakeEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct FakeEmbedError;

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

/// An authority that grants the system-role reveal capability to exactly one agent,
/// modeling an admin. Everything else mirrors the default policy.
#[derive(Debug)]
pub struct AdminAuthorizer {
    admin: Id,
}

impl Authorizer for AdminAuthorizer {
    fn authorize_write(
        &self,
        principal: &Principal,
        target: &Namespace,
    ) -> Result<(), AuthorizationError> {
        DefaultAuthorizer.authorize_write(principal, target)
    }

    fn visible_namespaces(&self, principal: &Principal) -> VisibleSet {
        DefaultAuthorizer.visible_namespaces(principal)
    }

    fn may_surface_system(&self, principal: &Principal) -> bool {
        principal.agent_id == self.admin
    }
}

pub fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

pub fn memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open memory"),
    )
}

/// A memory whose authority grants `admin` the system-role reveal. Mirrors how
/// `open_in_memory` builds + migrates the store, then injects the stricter authority.
pub fn admin_memory(admin: Id) -> Arc<Memory<FakeEmbedder>> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&now()).expect("migrate store");
    Arc::new(
        Memory::with_authorizer(
            Arc::new(store),
            FakeEmbedder::new(),
            MemoryConfig::default(),
            Arc::new(AdminAuthorizer { admin }),
            &now(),
        )
        .expect("open memory with admin authority"),
    )
}

pub fn read_params(ids: &[Id], agent: Id) -> ReadMemoryToolParams {
    ReadMemoryToolParams {
        memory_ids: ids.iter().map(ToString::to_string).collect(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        verbose: None,
        full: None,
        include_system: None,
    }
}

/// Like [`read_params`] but with the reader asserting the given `teams` on this same call —
/// the per-call team assertion a by-id read of a team-namespace memory requires (parity with
/// `search`). `teams` are bare slugs; the resolver maps each to `Namespace::Team(<slug>)`.
pub fn read_params_with_teams(ids: &[Id], agent: Id, teams: &[&str]) -> ReadMemoryToolParams {
    ReadMemoryToolParams {
        teams: teams.iter().map(ToString::to_string).collect(),
        ..read_params(ids, agent)
    }
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: now(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn ident(id: Id, namespace: Namespace, expired: bool) -> Identity {
    Identity {
        id,
        ingested_at: now(),
        namespace,
        expired_at: expired.then(now),
    }
}

/// Seed one episode straight into the store, bypassing the Capturer. Returns its id.
pub fn seed(memory: &Memory<FakeEmbedder>, content: &str, namespace: Namespace, role: Role) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace,
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        role,
        captured_at: now(),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("finite")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    memory
        .store()
        .insert_episode(&episode)
        .expect("seed episode");
    id
}

/// Seed one episode WITH its signed creation provenance through the capture commit funnel, so it
/// carries the `Episode -HAS_PROVENANCE-> ProvenanceRecord` edge that `read_memory` projects under
/// verbose/full. Unlike [`seed`] (a bare `insert_episode`, no provenance), this is the only way to
/// exercise the provenance read surface. The provenance fields are caller-fixed so a test can
/// assert exact rendered values; returns the episode id.
pub fn seed_with_provenance(
    memory: &Memory<FakeEmbedder>,
    content: &str,
    namespace: Namespace,
    writer_agent_id: Id,
    model_family: &str,
    model_version: Option<&str>,
    trust_at_write: f64,
) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: ident(id, namespace.clone(), false),
        stats: stats(),
        content: content.to_string(),
        role: Role::Assistant,
        captured_at: now(),
        agent_id: writer_agent_id,
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("finite")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    let provenance = ProvenanceRecord {
        identity: ident(Id::generate(), namespace.clone(), false),
        subject_id: id,
        writer_agent_id,
        signature: String::new(),
        source_episode_ids: Vec::new(),
        model_family: model_family.to_string(),
        model_version: model_version.map(str::to_string),
        trust_at_write,
    };
    let audit = AuditEvent {
        identity: ident(Id::generate(), namespace, false),
        kind: AuditKind::Capture,
        subject_id: id,
        actor_id: writer_agent_id,
        payload: serde_json::json!({ "verdict": "new" }),
        signature: String::new(),
        occurred_at: now(),
    };
    memory
        .store()
        .commit_capture(&episode, &provenance, &audit)
        .expect("seed episode with provenance");
    id
}

// ----- Multi-kind seed helpers -----
//
// Each seeds one node of a lifecycle kind straight into the store and returns its domain id.
// Embeddings are `None` to stay clear of the configured 4-dim vector check.

pub fn seed_fact(
    memory: &Memory<FakeEmbedder>,
    statement: &str,
    predicate: &str,
    namespace: Namespace,
    status: FactStatus,
    expired: bool,
) -> Id {
    let id = Id::generate();
    let fact = Fact {
        identity: ident(id, namespace, expired),
        stats: stats(),
        subject_id: Id::generate(),
        predicate: predicate.to_string(),
        object: ObjectValue::Text("object".to_string()),
        confidence: 1.0,
        status,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    };
    memory.store().insert_fact(&fact).expect("seed fact");
    id
}

pub fn seed_entity(
    memory: &Memory<FakeEmbedder>,
    canonical_name: &str,
    description: &str,
    entity_type: &str,
    namespace: Namespace,
) -> Id {
    let id = Id::generate();
    let entity = Entity {
        identity: ident(id, namespace, false),
        stats: stats(),
        canonical_name: canonical_name.to_string(),
        entity_type: entity_type.to_string(),
        aliases: Vec::new(),
        description: Some(description.to_string()),
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    memory.store().insert_entity(&entity).expect("seed entity");
    id
}

pub fn seed_note(memory: &Memory<FakeEmbedder>, content: &str, namespace: Namespace) -> Id {
    let id = Id::generate();
    let note = Note {
        identity: ident(id, namespace.clone(), false),
        stats: stats(),
        content: content.to_string(),
        context: None,
        keywords: Vec::new(),
        embedding: None,
        embedder_model: None,
        derived_from_episode: None,
    };
    let audit = AuditEvent {
        identity: ident(Id::generate(), namespace, false),
        kind: AuditKind::Distill,
        subject_id: id,
        actor_id: Id::generate(),
        payload: serde_json::json!({ "outcome": "written" }),
        signature: String::new(),
        occurred_at: now(),
    };
    memory
        .store()
        .seed_notes_for_test(
            &[MaterializedNote {
                note,
                source_facts: Vec::new(),
            }],
            &now(),
        )
        .expect("seed note");
    memory
        .store()
        .commit_audit(&audit)
        .expect("seed note audit");
    id
}

fn build_skill(
    id: Id,
    name: &str,
    description: &str,
    namespace: Namespace,
    deprecated: bool,
) -> Skill {
    Skill {
        identity: ident(id, namespace, false),
        stats: stats(),
        name: name.to_string(),
        version: 1,
        description: description.to_string(),
        problem_embedding: None,
        embedder_model: None,
        language: "text".to_string(),
        body: "the procedure body".to_string(),
        params: serde_json::Value::Null,
        preconditions: None,
        postconditions: None,
        capabilities: Vec::new(),
        success_count: 0,
        failure_count: 0,
        mean_latency_ms: None,
        source_hash: ContentHash::of(b"skill-body"),
        last_success_at: None,
        last_failure_at: None,
        deprecated_at: deprecated.then(now),
        induced: false,
    }
}

pub fn seed_skill(
    memory: &Memory<FakeEmbedder>,
    name: &str,
    description: &str,
    namespace: Namespace,
    deprecated: bool,
) -> Id {
    let id = Id::generate();
    let skill = build_skill(id, name, description, namespace, deprecated);
    memory
        .store()
        .save_skill(&skill, None, &[])
        .expect("seed skill");
    id
}

pub fn seed_bad_pattern(
    memory: &Memory<FakeEmbedder>,
    description: &str,
    namespace: Namespace,
) -> Id {
    // A BadPattern attaches to a live Skill via HAS_FAILURE, so seed a host skill first.
    let host = build_skill(
        Id::generate(),
        "host-skill-for-bad-pattern",
        "host skill",
        namespace.clone(),
        false,
    );
    let skill_node = memory
        .store()
        .save_skill(&host, None, &[])
        .expect("seed host skill");
    let id = Id::generate();
    let pattern = BadPattern {
        identity: ident(id, namespace, false),
        stats: stats(),
        description: description.to_string(),
        embedding: None,
        embedder_model: None,
        observed_at: now(),
    };
    memory
        .store()
        .save_bad_pattern(&pattern, skill_node)
        .expect("seed bad pattern");
    id
}

/// Seed one work item straight into the store. Returns its domain id. Identity-only — no
/// Stats, no embedding — so it never trips the dimension check the memory kinds carry.
#[allow(clippy::too_many_arguments)]
pub fn seed_work_item(
    memory: &Memory<FakeEmbedder>,
    level: &str,
    title: &str,
    body: Option<&str>,
    status: WorkStatus,
    parent: Option<Id>,
    ordinal: u64,
    namespace: Namespace,
) -> Id {
    let id = Id::generate();
    let item = WorkItem {
        identity: ident(id, namespace, false),
        title: title.to_string(),
        body: body.map(str::to_string),
        level: level.to_string(),
        work_status: status,
        parent_id: parent,
        ordinal,
    };
    memory
        .store()
        .save_work_item(&item)
        .expect("seed work item");
    id
}

/// Seed (mint) one tag straight into the store. Returns its content-addressed domain id.
pub fn seed_tag(
    memory: &Memory<FakeEmbedder>,
    slug: &str,
    display: Option<&str>,
    namespace: Namespace,
) -> Id {
    let (id, _) = memory
        .store()
        .ensure_tag(&namespace, slug, display, &now())
        .expect("seed tag");
    id
}

pub fn seed_core(
    memory: &Memory<FakeEmbedder>,
    content: &str,
    namespace: Namespace,
    block_kind: BlockKind,
    expired: bool,
) -> Id {
    let id = Id::generate();
    let block = CoreBlock {
        identity: ident(id, namespace.clone(), expired),
        stats: stats(),
        content: content.to_string(),
        block_kind,
        sensitivity: None,
        drift_baseline: None,
        embedding: None,
        embedder_model: None,
    };
    let audit = AuditEvent {
        identity: ident(Id::generate(), namespace, false),
        kind: AuditKind::Distill,
        subject_id: id,
        actor_id: Id::generate(),
        payload: serde_json::json!({ "outcome": "written" }),
        signature: String::new(),
        occurred_at: now(),
    };
    memory
        .store()
        .create_core_block(&block, &audit)
        .expect("seed core block");
    id
}
