//! The maintained candidate-state providers (data-model §9).
//!
//! Providers are not GQL/WAL objects — they are `Arc<dyn IndexProvider>` attached to
//! the graph at construction (and re-attached on recovery), so their specs are a
//! code-level constant the store builder wires in every boot, not a migration
//! statement. Each spec is a membership rule over labels and edge presence/absence
//! only; the engine cannot express a scalar predicate, so `current_support_facts`'
//! `status = 'active'` filter (§9) is applied at query time over the provider's
//! superset, not encoded here.

use std::sync::Arc;

use selene_core::{NodeId, db_string};
use selene_graph::{
    CandidateStateSpec, GraphError, IndexProvider, MaintainedCandidateStateProvider,
};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::search::CandidateSet;
use crate::store::Store;

pub(crate) const CURRENT_SUPPORT_FACTS: &str = "current_support_facts";
pub(crate) const PROVENANCE_CURRENT_SUPPORT_FACTS: &str = "provenance_current_support_facts";
pub(crate) const SCOPE_MEMBERSHIP: &str = "scope_membership";
pub(crate) const RECENCY_ACTIVE: &str = "recency_active";
pub(crate) const UNRESOLVED_CURRENT: &str = "unresolved_current";

pub(crate) const CANDIDATE_STATE_NAMES: &[&str] = &[
    CURRENT_SUPPORT_FACTS,
    PROVENANCE_CURRENT_SUPPORT_FACTS,
    SCOPE_MEMBERSHIP,
    RECENCY_ACTIVE,
    UNRESOLVED_CURRENT,
];

/// The five candidate-state specs (data-model §9), built fresh each call.
fn candidate_state_specs() -> Result<Vec<CandidateStateSpec>, StoreError> {
    let fact = db_string("Fact")?;
    let superseded_by = db_string("SUPERSEDED_BY")?;
    let contradicts = db_string("CONTRADICTS")?;

    Ok(vec![
        // current_support_facts: a Fact with no live SUPERSEDED_BY and no live
        // CONTRADICTS edge. Both edges remove the *source* (the superseded fact, and the
        // quarantined contradicting fact) per the domain edge docs, so both are
        // excluded outgoing. The `status = 'active'` half of §9 is a query-time filter
        // over this superset.
        CandidateStateSpec::new(db_string(CURRENT_SUPPORT_FACTS)?)
            .require_label(fact.clone())
            .exclude_outgoing(superseded_by.clone())
            .exclude_outgoing(contradicts.clone()),
        // provenance_current_support_facts: the above, plus an incoming SUPPORTS and an
        // outgoing HAS_PROVENANCE grounding.
        CandidateStateSpec::new(db_string(PROVENANCE_CURRENT_SUPPORT_FACTS)?)
            .require_label(fact.clone())
            .exclude_outgoing(superseded_by)
            .exclude_outgoing(contradicts.clone())
            .require_incoming(db_string("SUPPORTS")?)
            .require_outgoing(db_string("HAS_PROVENANCE")?),
        // scope_membership: anything with a live IN_SCOPE edge. This is the coarse
        // "in some scope" set; per-scope selection is query-time candidate-set algebra.
        CandidateStateSpec::new(db_string(SCOPE_MEMBERSHIP)?)
            .require_outgoing(db_string("IN_SCOPE")?),
        // recency_active: anything with a live RECENT_IN edge (coarse, like scope).
        CandidateStateSpec::new(db_string(RECENCY_ACTIVE)?)
            .require_outgoing(db_string("RECENT_IN")?),
        // unresolved_current: a Fact that nothing currently contradicts — no live
        // *incoming* CONTRADICTS. This is the deliberate dual of current_support_facts,
        // which drops the contradiction *source* (outgoing). Keeping the directions
        // opposite is what makes the §9 set algebra pay off: current_support_facts minus
        // unresolved_current is exactly the facts something contradicts but that are
        // otherwise still current — the contested incumbents the §9 "quarantine
        // reasoning" use names — while the intersection is the clean active set. Excluding
        // outgoing here instead would re-derive current_support_facts and lose that.
        CandidateStateSpec::new(db_string(UNRESOLVED_CURRENT)?)
            .require_label(fact)
            .exclude_incoming(contradicts),
    ])
}

/// Build the candidate-state provider the store attaches at construction.
pub(crate) fn candidate_state_provider() -> Result<Arc<dyn IndexProvider>, StoreError> {
    let provider = MaintainedCandidateStateProvider::new(candidate_state_specs()?)
        .map_err(GraphError::Provider)?;
    Ok(Arc::new(provider))
}

/// A maintained candidate-state set and its current size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateStateInfo {
    /// The provider's stable set name.
    pub name: String,
    /// How many nodes are currently in the set.
    pub candidate_count: usize,
    /// The graph generation this membership is proven current through. The engine
    /// only returns these infos after the provider has applied every mutation up to
    /// this generation, so a returned value is a live watermark, never stale.
    pub generation: u64,
}

impl Store {
    /// The current candidate-state sets and their sizes (data-model §9 introspection).
    ///
    /// The infos are generation-checked: the engine returns them only when the
    /// provider has applied every commit through the current graph generation, so a
    /// successful call is itself the proof that no set is stale.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the provider cannot prove it is current with the graph.
    pub fn candidate_state_infos(&self) -> Result<Vec<CandidateStateInfo>, StoreError> {
        let infos = self
            .graph()
            .vector_candidate_state_infos()
            .map_err(GraphError::Provider)?;
        Ok(infos
            .into_iter()
            .map(|info| CandidateStateInfo {
                name: info.name.as_str().to_owned(),
                candidate_count: info.candidate_count,
                generation: info.generation,
            })
            .collect())
    }

    /// The current membership of one maintained candidate-state set (data-model §9).
    ///
    /// Resolves the named provider set against the *current* graph snapshot and
    /// returns its members as engine node ids, sorted and de-duplicated by the engine.
    /// The lookup is generation-checked end to end: the engine binds the set to the
    /// same immutable snapshot whose generation it validates the provider against, so
    /// the returned membership can never lag the committed graph — a stale provider
    /// surfaces as an error rather than an out-of-date set. This is the typed read that
    /// the high-precision retrieval path (M2.T07/T08) composes; the `status = 'active'`
    /// half of `current_support_facts` (§9) remains a query-time scalar filter the
    /// caller layers on top, since a provider rule cannot express a scalar predicate.
    ///
    /// An empty vector means the set has no current members (the provider is always
    /// attached, so a known [`CandidateSet`] always resolves).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the provider cannot prove it is current with the graph.
    pub fn candidate_state_members(&self, set: CandidateSet) -> Result<Vec<NodeId>, StoreError> {
        let name = db_string(set.as_name())?;
        let resolved = self
            .graph()
            .vector_candidate_set(&name)
            .map_err(GraphError::Provider)?;
        Ok(resolved
            .map(|members| members.as_nodes().to_vec())
            .unwrap_or_default())
    }
}
