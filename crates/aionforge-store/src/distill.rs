//! The L0 write surface for the optional, off-by-default LLM distiller (M3.T08).
//!
//! Distillation is the LLM-backed counterpart to the deterministic M2.T06 summary pass, but
//! it runs **off the critical consolidation path** (04 §*Canonical vs. distilled*): the cursor
//! flip writes only the canonical, byte-deterministic rule summaries, while distilled notes are
//! materialized here, in their own transaction, by a separate driver. That separation is the
//! whole point — enabling distillation can never perturb the consolidation path's byte-identical
//! replay, and a slow or unavailable distiller degrades to the canonical tier without ever
//! touching the cursor.
//!
//! This surface therefore does exactly one thing the consolidation surface deliberately does
//! not: write [`Note`](aionforge_domain::nodes::associative::Note) nodes and their lineage
//! **without** an episode flip or a cursor advance. A distilled note's id is content-addressed
//! over its source set under the distiller's own rule version, so it lands in an id-space
//! disjoint from the rule summaries — the two tiers coexist and a replay is a no-op. The
//! `Note` schema carries no model-identity field (02 §4.6), so the consolidating model's
//! identity, endpoint, and seed are recorded in the paired [`AuditEvent`] payload (the
//! `distill` audit kind), which the cross-family guard reads for lineage (07 §T3, M6.T01).

use std::collections::HashMap;

use aionforge_domain::edges::Audit;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::time::Timestamp;
use selene_core::PropertyMap;

use crate::dedup::entity_node_by_id;
use crate::error::StoreError;
use crate::materialize::ensure_edge;
use crate::note::{MaterializedNote, materialize_notes};
use crate::store::Store;

impl Store {
    /// Materialize a batch of distilled notes and their provenance audits in one fresh write
    /// transaction, entirely off the consolidation cursor (M3.T08).
    ///
    /// Each note is content-addressed and deduped by id, so a re-run writes no second copy and a
    /// crash mid-batch is safe to retry. Every source fact is already committed (the distiller
    /// reads the current graph, not an in-flight episode), so note lineage resolves through the
    /// committed index — the in-transaction fact map and the canonical remap are empty here. The
    /// note's `Note -DERIVED_FROM-> Fact` edges are wired by the shared note materializer.
    ///
    /// Each `AuditEvent` records one distillation call's full provenance (model identity,
    /// endpoint, seed, and outcome) in its payload; it is written deduped by its content-addressed
    /// id and wired `AuditEvent -AUDIT-> Entity` to the subject it distilled. The audit's
    /// `subject_id` names that entity; if it is somehow absent from the committed graph the audit
    /// node is still written — the provenance lives in the payload, not the edge — and only the
    /// edge is skipped (logged), so one stale subject cannot wedge the batch.
    ///
    /// This never reads or writes any episode, cursor, or `consolidation_state`: distillation is
    /// invisible to the scheduler, and the canonical consolidation path is untouched whether this
    /// succeeds, fails, or is never called at all.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a translation, a mutation, or the commit fails. A failed commit
    /// rolls back the whole batch — the graph is left exactly as it was, with no partial notes.
    pub fn materialize_distilled_notes(
        &self,
        notes: &[MaterializedNote],
        audits: &[AuditEvent],
        now: &Timestamp,
    ) -> Result<(), StoreError> {
        if notes.is_empty() && audits.is_empty() {
            return Ok(());
        }

        // All source facts are committed, so resolution falls through to the index: an empty
        // in-transaction fact map and an empty canonical remap (mirrors the cursor path's note
        // step, minus the episode-local facts it has and this off-cursor batch does not).
        let empty_fact_nodes: HashMap<String, selene_core::NodeId> = HashMap::new();
        let empty_canonical: HashMap<aionforge_domain::ids::Id, aionforge_domain::ids::Id> =
            HashMap::new();

        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();

            // Notes + `DERIVED_FROM` lineage (deduped by content-addressed id; an unresolvable
            // source drops just that edge, logged, like the cursor path).
            materialize_notes(
                &mut mutator,
                notes,
                &empty_fact_nodes,
                &empty_canonical,
                now,
            )?;

            // Provenance audits: node deduped by content-addressed id, then `AUDIT -> Entity`.
            for event in audits {
                let audit_node =
                    match crate::audit::find_existing(mutator.read(), &event.identity.id)? {
                        Some(node) => node,
                        None => {
                            let (labels, props) = crate::audit::to_node(event)?;
                            mutator.create_node(labels, props)?
                        }
                    };
                match entity_node_by_id(mutator.read(), &event.subject_id)? {
                    Some(subject_node) => ensure_edge(
                        &mut mutator,
                        Audit::LABEL,
                        audit_node,
                        subject_node,
                        PropertyMap::from_pairs(Vec::new())?,
                    )?,
                    None => tracing::warn!(
                        audit = event.identity.id.as_str(),
                        subject = event.subject_id.as_str(),
                        "distillation: audit subject entity not found; skipping AUDIT edge"
                    ),
                }
            }
        }
        txn.commit()?;
        Ok(())
    }
}
