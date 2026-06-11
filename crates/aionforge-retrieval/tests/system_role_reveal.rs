//! Acceptance for the admin-gated system-role reveal (07 §4, M6.T02): system-role
//! memories surface only when the caller requests it AND the injected authority grants
//! the capability — never on a free request flag alone. Both exclusion gates (the role
//! gate and the system-namespace gate) lift in lockstep.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::authz::{
    AuthorizationError, Authorizer, DefaultAuthorizer, Principal, VisibleSet,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::{HybridRetriever, RecallQuery, RetrieverConfig};
use aionforge_store::{Store, StoreConfig};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
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
        let out: Vec<Embedding> = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
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

fn seed(store: &Store, content: &str, namespace: Namespace, role: Role) {
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace,
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role,
        captured_at: ts("2026-06-06T09:29:59-05:00[America/Chicago]"),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("finite")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("seed episode");
}

/// A stricter authority that grants the system-role surface capability to one agent,
/// modeling an admin reveal. Everything else mirrors the default policy.
#[derive(Debug)]
struct AdminAuthorizer {
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

/// Recall under the given authority and request flag; returns the bundle contents.
async fn recall_with(
    store: Arc<Store>,
    authorizer: Arc<dyn Authorizer>,
    include_system: bool,
) -> Vec<String> {
    let r = HybridRetriever::with_authorizer(
        store,
        FakeEmbedder::new(),
        RetrieverConfig::default(),
        authorizer,
    );
    let mut query = RecallQuery::new("turn", alice(), 10);
    query.options.include_system = include_system;
    let bundle = r.recall(query).await.expect("recall");
    bundle
        .structured
        .iter()
        .map(|e| e.content().to_string())
        .collect()
}

#[tokio::test]
async fn the_admin_capability_surfaces_a_system_role_episode() {
    let store = store();
    seed(&store, "a normal user turn", alice_ns(), Role::User);
    // A system-role episode in alice's own (visible) namespace: the role gate is what
    // excludes it, so the reveal must lift the role gate.
    seed(&store, "a system directive turn", alice_ns(), Role::System);

    let admin = Arc::new(AdminAuthorizer { admin: alice_id() });
    let contents = recall_with(store, admin, true).await;
    assert!(
        contents.contains(&"a system directive turn".to_string()),
        "the capability + the request surfaces system-role content: {contents:?}"
    );
}

#[tokio::test]
async fn the_admin_capability_surfaces_system_namespace_content() {
    let store = store();
    // System-namespace content: the NAMESPACE gate excludes it, so the reveal must also
    // lift the namespace gate (both gates lift in lockstep).
    seed(
        &store,
        "substrate-internal control content",
        Namespace::System,
        Role::User,
    );

    let admin = Arc::new(AdminAuthorizer { admin: alice_id() });
    let contents = recall_with(store, admin, true).await;
    assert!(
        contents.contains(&"substrate-internal control content".to_string()),
        "the reveal lifts the namespace gate too: {contents:?}"
    );
}

#[tokio::test]
async fn the_request_flag_is_inert_without_the_capability() {
    let store = store();
    seed(&store, "a system directive turn", alice_ns(), Role::System);

    // The flag is set but the DEFAULT authority never grants the capability, so the
    // request is inert — a free bool can never be a security gate.
    let default_auth: Arc<dyn Authorizer> = Arc::new(DefaultAuthorizer);
    let contents = recall_with(Arc::clone(&store), default_auth, true).await;
    assert!(
        !contents.contains(&"a system directive turn".to_string()),
        "include_system alone cannot reveal anything: {contents:?}"
    );

    // And a non-admin principal is denied even by the admin-aware authority.
    let other = Id::generate();
    let admin = Arc::new(AdminAuthorizer { admin: other });
    let contents = recall_with(store, admin, true).await;
    assert!(
        !contents.contains(&"a system directive turn".to_string()),
        "a principal without the capability gets nothing: {contents:?}"
    );
}
