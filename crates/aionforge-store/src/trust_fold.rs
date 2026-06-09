//! The L0 read/write surface trust scoring folds over (06 §5, M4.T05).
//!
//! Trust is doubly-derived state: the canonical record is an append-only set of
//! `ReliabilityUpdate` [`AuditEvent`]s, and `Agent.trust_scores` (plus `Fact.stats.trust`) are
//! recomputable caches the L2 fold rewrites. This module is the store seam that makes that work:
//! it attributes an invalidated fact to its producing agents, reads an agent's reliability event
//! log, records a new event idempotently, and refreshes the two caches write-when-changed. It
//! adds **no** node or edge types and **no** index — `AuditKind::ReliabilityUpdate`, the
//! `AuditEvent.(kind, subject_id)` indexes, and the `Episode.agent_id` / `DERIVED_FROM` wiring all
//! already exist.
//!
//! Everything here is **off-cursor** — a consolidation pass is read-only ([`crate::pass`]), so
//! the trust fold runs from the engine facade, never from inside a pass. A split-out `impl Store`.

use aionforge_domain::edges::{Audit, DerivedFrom};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::agent::Agent;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use selene_core::{LabelDiff, PropertyDiff, PropertyMap, Value, db_string};
use selene_graph::RowIndex;

use crate::attestation::agent_node_in;
use crate::convert::{as_id, id_value, json_value, key};
use crate::error::StoreError;
use crate::store::Store;
use crate::{NodeId, agent, audit};

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

    /// The `ReliabilityUpdate` audit events recorded against an agent — the canonical log the L2
    /// fold replays into `(alpha, beta, score)` (06 §5, M4.T05).
    ///
    /// Probes the `AuditEvent.subject_id` index for `subject_id == agent_id` (the subject of a
    /// reliability event is the agent whose score moved) and keeps only `ReliabilityUpdate` kinds.
    ///
    /// # Errors
    /// Returns [`StoreError`] if an event row cannot be decoded.
    pub fn reliability_events(&self, agent_id: &Id) -> Result<Vec<AuditEvent>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(AuditEvent::LABEL)?;
        let prop = db_string(SUBJECT_ID)?;
        let value = id_value(agent_id)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let event = audit::from_properties(props)?;
            if event.kind == AuditKind::ReliabilityUpdate {
                events.push(event);
            }
        }
        Ok(events)
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
    /// `Fact.stats.trust` is recomputable derived state (the L2 scorer recomputes it as the `min`
    /// over the fact's distinct producing agents' reliabilities), so a surgical single-property
    /// update is safe; an unchanged value is a true no-op. This is what lets the retrieval `Trust`
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
