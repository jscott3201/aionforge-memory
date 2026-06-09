//! The L0 read/write surface trust scoring folds over (06 §5, M4.T05).
//!
//! Trust is doubly-derived state: the canonical record is an append-only multiset of
//! `ReliabilityUpdate` [`AuditEvent`]s, and `Agent.trust_scores` (plus `Fact.stats.trust`) are
//! recomputable caches the L2 fold rewrites. This module is the store seam that makes that work:
//! it attributes an invalidated fact to its producing agents, reads an agent's reliability event
//! log, records a new event idempotently, and refreshes the two caches write-when-changed. It
//! adds **no** node or edge types and **no** index — `AuditKind::ReliabilityUpdate`, the
//! `AuditEvent.kind` and `AuditEvent.subject_id` scalar indexes, and the `Episode.agent_id` /
//! `DERIVED_FROM` wiring all already exist. (`reliability_events` probes the `subject_id` index
//! and filters `kind` in memory — see its doc; this PR registers no composite.)
//!
//! Everything here is **off-cursor**: a consolidation pass is read-only
//! ([`crate::consolidation`]), so the trust fold runs from the engine facade, never from inside a
//! pass. These readers and cache-refreshers are a split-out `impl Store` block.

use aionforge_domain::edges::{Audit, DerivedFrom};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::agent::Agent;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::Fact;
use selene_core::{LabelDiff, PropertyDiff, PropertyMap, Value, db_string};
use selene_graph::RowIndex;

use crate::attestation::agent_node_in;
use crate::convert::{as_id, id_value, json_value, key};
use crate::error::StoreError;
use crate::store::Store;
use crate::{NodeId, agent, audit};

/// The identity-block `id` property (mirrors [`crate::episode`]).
const ID: &str = "id";
/// Episode property carrying the capturing agent (mirrors [`crate::episode`]).
const AGENT_ID: &str = "agent_id";
/// The single JSON-blob property holding `Agent.trust_scores` (mirrors [`crate::agent`]).
const TRUST_SCORES: &str = "trust_scores";
/// The `Fact.stats.trust` node-summary property (mirrors [`crate::fact`]).
const TRUST: &str = "trust";
/// `AuditEvent.subject_id`, the scalar-indexed by-subject axis (mirrors [`crate::audit`]).
const SUBJECT_ID: &str = "subject_id";

impl Store {
    /// The distinct agents that produced a fact, for reliability attribution (06 §5, M4.T05).
    ///
    /// Reads the fact's outgoing `DERIVED_FROM` edges (fact → source `Episode`) and collects each
    /// neighbor's `Episode.agent_id`, **distinct and id-sorted**. A fact deduped across several
    /// source episodes has several `DERIVED_FROM` edges; one agent that re-asserted it many times
    /// still counts once, which de-fangs self-corroboration. The attribution lives only in the
    /// committed graph (never the detector's read path, 06 §2), so this is an off-cursor read.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a neighbor's `agent_id` cannot be decoded.
    pub fn producing_agents(&self, fact: NodeId) -> Result<Vec<Id>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(DerivedFrom::LABEL)?;
        let Some(adjacency) = snapshot.outgoing_edges(fact) else {
            return Ok(Vec::new());
        };
        let agent_id_key = db_string(AGENT_ID)?;
        let mut agents: Vec<Id> = Vec::new();
        for edge in adjacency.iter_label(&label) {
            // DERIVED_FROM is polymorphic; a neighbor without an `agent_id` (not an Episode) is
            // simply not an attributable producer, so skip it rather than fail.
            let Some(value) = snapshot
                .node_properties(edge.neighbor)
                .and_then(|props| props.get(&agent_id_key).cloned())
            else {
                continue;
            };
            let agent_id = as_id(&value)?;
            if !agents.contains(&agent_id) {
                agents.push(agent_id);
            }
        }
        agents.sort_unstable();
        Ok(agents)
    }

    /// The facts a given agent produced — the reverse of [`Store::producing_agents`], for the
    /// reliability-trust recompute (06 §5, M4.T05).
    ///
    /// Walks the agent's episodes through the `Episode.agent_id` index, and for each the
    /// *incoming* `DERIVED_FROM` edges (`Fact -DERIVED_FROM-> Episode`) back to the facts derived
    /// from them, keeping only `Fact`-labeled neighbors — the edge is polymorphic, since a `Note`
    /// also derives from an episode (`Note -DERIVED_FROM-> Episode`). Returns the distinct fact
    /// nodes in discovery order. Like `producing_agents` this reads only the committed graph, so it
    /// is an off-cursor read; the L2 scorer uses it to find which facts to re-derive `stats.trust`
    /// for after an agent's reliability moves.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a row or label lookup fails.
    pub fn facts_produced_by(&self, agent_id: &Id) -> Result<Vec<NodeId>, StoreError> {
        let snapshot = self.graph().read();
        let episode_label = db_string(Episode::LABEL)?;
        let agent_prop = db_string(AGENT_ID)?;
        let agent_value = id_value(agent_id)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&episode_label, &agent_prop, &agent_value)
        else {
            return Ok(Vec::new());
        };
        let derived_label = db_string(DerivedFrom::LABEL)?;
        let fact_label = db_string(Fact::LABEL)?;
        let mut facts: Vec<NodeId> = Vec::new();
        for row in rows.iter() {
            let Some(episode) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            let Some(adjacency) = snapshot.incoming_edges(episode) else {
                continue;
            };
            for edge in adjacency.iter_label(&derived_label) {
                let fact = edge.neighbor;
                // DERIVED_FROM is polymorphic — a Note derives from an episode too — so keep only
                // the Fact-labeled producers and skip a Note (or anything else) on the edge.
                let is_fact = snapshot
                    .node_labels(fact)
                    .is_some_and(|labels| labels.contains(&fact_label));
                if is_fact && !facts.contains(&fact) {
                    facts.push(fact);
                }
            }
        }
        Ok(facts)
    }

    /// A fact's write-time trust baseline: the mean of its source episodes' immutable `stats.trust`
    /// (06 §5, M4.T05).
    ///
    /// Walks the fact's outgoing `DERIVED_FROM` edges to its source `Episode`s (the edge is
    /// polymorphic, so non-`Episode` endpoints are skipped) and means their write-time `trust`,
    /// summed in canonical episode-id order so the float is byte-identical on replay. Returns
    /// `None` when the fact has no episode source — there is no baseline to anchor a reliability
    /// recompute to. This reads the *source* episodes, **never the fact's own `stats.trust`** (the
    /// cache the recompute rewrites), so the reliability recompute that consumes it stays a pure
    /// function of primary state — no read-modify-write on the value it is recomputing.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a source episode's `id` or `trust` cannot be decoded.
    pub fn fact_source_trust_mean(&self, fact: NodeId) -> Result<Option<f64>, StoreError> {
        let snapshot = self.graph().read();
        let Some(adjacency) = snapshot.outgoing_edges(fact) else {
            return Ok(None);
        };
        let derived_label = db_string(DerivedFrom::LABEL)?;
        let episode_label = db_string(Episode::LABEL)?;
        let trust_key = db_string(TRUST)?;
        let id_key = db_string(ID)?;
        let mut sources: Vec<(Id, f64)> = Vec::new();
        for edge in adjacency.iter_label(&derived_label) {
            let episode = edge.neighbor;
            // DERIVED_FROM is polymorphic; only an Episode carries a write-time trust baseline.
            let is_episode = snapshot
                .node_labels(episode)
                .is_some_and(|labels| labels.contains(&episode_label));
            if !is_episode {
                continue;
            }
            let Some(props) = snapshot.node_properties(episode) else {
                continue;
            };
            let Some(Value::Float(trust)) = props.get(&trust_key).cloned() else {
                continue;
            };
            let Some(id_value) = props.get(&id_key).cloned() else {
                continue;
            };
            let id = as_id(&id_value)?;
            // A fact deduped across episodes has one DERIVED_FROM per source; count each once.
            if !sources.iter().any(|(seen, _)| *seen == id) {
                sources.push((id, trust));
            }
        }
        if sources.is_empty() {
            return Ok(None);
        }
        // Sum in canonical id order so the mean is byte-identical regardless of edge iteration.
        sources.sort_by_key(|(id, _)| *id);
        let sum: f64 = sources.iter().map(|(_, trust)| *trust).sum();
        Ok(Some(sum / sources.len() as f64))
    }

    /// The `ReliabilityUpdate` audit events recorded against an agent — the canonical log the L2
    /// fold replays into `(alpha, beta, score)` (06 §5, M4.T05).
    ///
    /// A thin wrapper over the shared `audit_events_eq` spine (the by-subject reader the M4.T06
    /// audit subgraph also uses): probes the `AuditEvent.subject_id` index for the agent
    /// whose score moved, keeping only `ReliabilityUpdate` kinds. The fold is order-independent, so
    /// the `(occurred_at, id)` sort the spine applies is harmless.
    ///
    /// # Errors
    /// Returns [`StoreError`] if an event row cannot be decoded.
    pub fn reliability_events(&self, agent_id: &Id) -> Result<Vec<AuditEvent>, StoreError> {
        self.audit_events_eq(
            SUBJECT_ID,
            &id_value(agent_id)?,
            Some(AuditKind::ReliabilityUpdate),
        )
    }

    /// Record one `ReliabilityUpdate` event against its agent subject, idempotently (06 §5).
    ///
    /// One commit writes the content-addressed [`AuditEvent`] and an `AUDIT` edge to the agent
    /// node, **deduped by id** under the write lock — a replay (the same content-addressed event
    /// id) returns the existing node and writes nothing, so re-processing the same invalidation,
    /// a crash-replay, or a double trigger each fold to one increment. The event *is* both the
    /// audit trail and the fold's idempotency marker.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translating or committing the event fails.
    pub fn record_reliability_update(&self, event: &AuditEvent) -> Result<NodeId, StoreError> {
        let (labels, props) = audit::to_node(event)?;
        let audit_edge = db_string(Audit::LABEL)?;
        let mut txn = self.graph().begin_write();
        let id = {
            let mut mutator = txn.mutator();
            match audit::find_existing(mutator.read(), &event.identity.id)? {
                Some(node) => node,
                None => {
                    let agent_node = agent_node_in(mutator.read(), &event.subject_id)?;
                    let node = mutator.create_node(labels, props)?;
                    // Wire AUDIT -> the agent subject so the M4.T06 by-subject walk reaches it.
                    // The agent should exist; if it somehow does not, the by-subject_id index read
                    // (reliability_events) still finds the event, so skip the edge rather than fail.
                    if let Some(agent_node) = agent_node {
                        mutator.create_edge(
                            audit_edge,
                            node,
                            agent_node,
                            PropertyMap::from_pairs(Vec::new())?,
                        )?;
                    }
                    node
                }
            }
        };
        txn.commit()?;
        Ok(id)
    }

    /// Refresh an agent's cached `trust_scores`, write-when-changed (06 §5, M4.T05).
    ///
    /// `Agent.trust_scores` is a recomputable cache of the [`Store::reliability_events`] fold, so
    /// this surgically replaces the single `trust_scores` JSON-blob property on the existing agent
    /// node when it differs, and is a true no-op when the fold produced no change. (A missing
    /// agent node — not expected post-enrollment — falls back to a whole-node create.)
    ///
    /// # Errors
    /// Returns [`StoreError`] if translating or committing fails.
    pub fn refresh_agent_trust(&self, agent: &Agent) -> Result<NodeId, StoreError> {
        let new_value = json_value(&agent.trust_scores)?;
        let mut txn = self.graph().begin_write();
        let id = {
            let mut mutator = txn.mutator();
            match agent_node_in(mutator.read(), &agent.identity.id)? {
                Some(node) => {
                    let trust_scores_key = db_string(TRUST_SCORES)?;
                    let unchanged = mutator
                        .read()
                        .node_properties(node)
                        .and_then(|props| props.get(&trust_scores_key).cloned())
                        == Some(new_value.clone());
                    if !unchanged {
                        mutator.update_node(
                            node,
                            LabelDiff::new([], [])?,
                            PropertyDiff::new([(key(TRUST_SCORES)?, new_value)], [])?,
                        )?;
                    }
                    node
                }
                None => {
                    let (labels, props) = agent::to_node(agent)?;
                    mutator.create_node(labels, props)?
                }
            }
        };
        txn.commit()?;
        Ok(id)
    }

    /// Refresh a fact's cached `stats.trust` node summary, write-when-changed (06 §5, M4.T05).
    ///
    /// `Fact.stats.trust` is recomputable derived state (the L2 scorer derives it from the fact's
    /// distinct producing agents' reliabilities), so a surgical single-property update is safe; an
    /// unchanged value is a true no-op. This is what lets the retrieval `Trust`
    /// signal sink a fact whose producer decayed without any query-time agent join.
    ///
    /// # Errors
    /// Returns [`StoreError`] if committing fails.
    pub fn refresh_fact_trust(&self, fact: NodeId, trust: f64) -> Result<(), StoreError> {
        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();
            let trust_key = db_string(TRUST)?;
            let current = mutator
                .read()
                .node_properties(fact)
                .and_then(|props| props.get(&trust_key).cloned());
            if current != Some(Value::Float(trust)) {
                mutator.update_node(
                    fact,
                    LabelDiff::new([], [])?,
                    PropertyDiff::new([(key(TRUST)?, Value::Float(trust))], [])?,
                )?;
            }
        }
        txn.commit()?;
        Ok(())
    }
}
