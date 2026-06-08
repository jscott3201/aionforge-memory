//! Resolving an agent's public key from the store (06 §3).
//!
//! [`StoreKeyResolver`] bridges the domain [`PublicKeyResolver`] seam to the store's
//! `agent_by_id` lookup: it reads the registered `Agent` and returns its base64 public
//! key, mapping a store failure into the seam's [`ResolveError`]. An unregistered agent
//! resolves to `None`, which the signing gate treats as a fail-closed rejection.

use std::sync::Arc;

use aionforge_domain::ids::Id;
use aionforge_domain::verify::{PublicKeyResolver, ResolveError};
use aionforge_store::Store;

/// Resolves a writer agent's public key from the store by agent id.
pub struct StoreKeyResolver {
    store: Arc<Store>,
}

impl StoreKeyResolver {
    /// Wrap a store for public-key resolution.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl PublicKeyResolver for StoreKeyResolver {
    fn public_key(&self, agent_id: &Id) -> Result<Option<String>, ResolveError> {
        self.store
            .agent_by_id(agent_id)
            .map(|agent| agent.map(|agent| agent.public_key))
            .map_err(|error| ResolveError(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::Identity;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
    use aionforge_domain::time::Timestamp;
    use aionforge_store::StoreConfig;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime literal")
    }

    fn store() -> Arc<Store> {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: 8,
        })
        .expect("open store");
        store
            .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
            .expect("migrate store");
        Arc::new(store)
    }

    fn agent(public_key: &str) -> Agent {
        Agent {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-08T09:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Agent("ops".to_string()),
                expired_at: None,
            },
            public_key: public_key.to_string(),
            model_family: "test".to_string(),
            model_version: None,
            trust_scores: TrustScores::default(),
            status: AgentStatus::Active,
        }
    }

    #[test]
    fn resolves_a_registered_agents_key() {
        let store = store();
        let agent = agent("cHVibGljLWtleQ==");
        let id = agent.identity.id;
        store.create_agent(&agent).expect("create agent");

        let resolver = StoreKeyResolver::new(Arc::clone(&store));
        assert_eq!(
            resolver.public_key(&id).expect("resolve"),
            Some("cHVibGljLWtleQ==".to_string())
        );
    }

    #[test]
    fn unknown_agent_resolves_to_none() {
        let resolver = StoreKeyResolver::new(store());
        assert_eq!(resolver.public_key(&Id::generate()).expect("resolve"), None);
    }
}
