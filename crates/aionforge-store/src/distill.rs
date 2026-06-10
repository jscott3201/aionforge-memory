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
//! disjoint from the rule summaries — the two tiers coexist and a replay is a no-op.
//!
//! ## Provenance
//!
//! The `Note` schema carries no model-identity field (02 §4.6), so the consolidating model's
//! identity, endpoint, and seed travel in a paired [`AuditEvent`] payload under the `distill`
//! audit kind. For a written note the audit is wired `AuditEvent -AUDIT-> Note`, so "which model
//! produced this note" is a single hop — exactly what the cross-family guard traces (07 §T3,
//! M6.T01), and unambiguous even when a note rolls up facts about several entities. A call that
//! produced no note (the detail-retention guard rejected a lossy summary, or the distiller
//! declined) is still audited — "for every call" (M3.T08) — and anchored `AUDIT -> Entity` on
//! the cluster subject instead.
//!
//! The audit `payload` is the same open `serde_json::Value` every audit kind uses; this surface
//! writes it verbatim and never inspects it. Constructing it from only non-secret fields (the
//! declared model family/version, the endpoint, the pinned seed — never the API key) is the
//! distiller's contract, upheld where the payload is built (M3.T08 PR-B).

use std::collections::HashMap;

use aionforge_domain::edges::Audit;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::time::Timestamp;
use selene_core::{NodeId, PropertyMap};

use crate::dedup::entity_node_by_id;
use crate::error::StoreError;
use crate::materialize::ensure_edge;
use crate::note::{MaterializedNote, materialize_notes};
use crate::store::Store;

/// A distilled note paired with the audit recording the call that produced it (M3.T08).
///
/// Pairing is structural so the provenance audit can be wired straight to the note it describes
/// (`AuditEvent -AUDIT-> Note`): one note, one producing call, one queryable provenance edge.
#[derive(Debug, Clone)]
pub struct DistilledNoteWrite {
    /// The distilled note to write, with its `DERIVED_FROM` source facts.
    pub note: MaterializedNote,
    /// The `distill` audit recording the call's model identity, endpoint, seed, and outcome.
    pub audit: AuditEvent,
}

impl Store {
    /// Materialize a batch of distilled notes and their provenance audits in one fresh write
    /// transaction, entirely off the consolidation cursor (M3.T08).
    ///
    /// `written` carries the notes the distiller produced, each paired with the audit of the call
    /// that produced it. Each note is content-addressed and deduped by id, so a re-run writes no
    /// second copy and a crash mid-batch is safe to retry. Every source fact is already committed
    /// (the distiller reads the current graph, not an in-flight episode), so note lineage resolves
    /// through the committed index — the in-transaction fact map and the canonical remap are empty
    /// here. The shared note materializer wires each `Note -DERIVED_FROM-> Fact` edge; this
    /// surface then wires the paired `AuditEvent -AUDIT-> Note` so the consolidating model is one
    /// hop from the note for the cross-family guard (07 §T3, M6.T01).
    ///
    /// `declined` carries the audits of calls that produced no note — a lossy summary the
    /// detail-retention guard rejected, or a cluster the distiller declined — so that every call
    /// is recorded (M3.T08). Lacking a note, each is anchored `AUDIT -> Entity` on its subject;
    /// if that entity is somehow absent from the committed graph the audit node is still written
    /// (the provenance is the payload, not the edge) and only the edge is skipped, logged, so one
    /// stale subject cannot wedge the batch.
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
        written: &[DistilledNoteWrite],
        declined: &[AuditEvent],
        now: &Timestamp,
    ) -> Result<(), StoreError> {
        if written.is_empty() && declined.is_empty() {
            return Ok(());
        }

        // All source facts are committed, so resolution falls through to the index: an empty
        // in-transaction fact map and an empty canonical remap (mirrors the cursor path's note
        // step, minus the episode-local facts it has and this off-cursor batch does not).
        let empty_fact_nodes: HashMap<String, NodeId> = HashMap::new();
        let empty_canonical: HashMap<aionforge_domain::ids::Id, aionforge_domain::ids::Id> =
            HashMap::new();
        let notes: Vec<MaterializedNote> = written.iter().map(|w| w.note.clone()).collect();

        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();

            // Notes + `DERIVED_FROM` lineage (deduped by content-addressed id; an unresolvable
            // source drops just that edge, logged, like the cursor path). The returned node of
            // each note, in order, is what we wire its provenance audit to.
            let note_nodes = materialize_notes(
                &mut mutator,
                &notes,
                &empty_fact_nodes,
                &empty_canonical,
                now,
            )?;

            // Provenance for each written note: audit node (deduped by id), then `AUDIT -> Note`.
            for (write, note_node) in written.iter().zip(note_nodes) {
                let audit_node = crate::audit::ensure_event(&mut mutator, &write.audit)?.node;
                ensure_edge(
                    &mut mutator,
                    Audit::LABEL,
                    audit_node,
                    note_node,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }

            // Calls that produced no note: audit recorded anyway, anchored on the subject entity.
            for event in declined {
                let audit_node = crate::audit::ensure_event(&mut mutator, event)?.node;
                match entity_node_by_id(mutator.read(), &event.subject_id)? {
                    Some(subject_node) => ensure_edge(
                        &mut mutator,
                        Audit::LABEL,
                        audit_node,
                        subject_node,
                        PropertyMap::from_pairs(Vec::new())?,
                    )?,
                    None => tracing::warn!(
                        audit = %event.identity.id,
                        subject = %event.subject_id,
                        "distillation: declined-call audit subject entity not found; skipping AUDIT edge"
                    ),
                }
            }
        }
        txn.commit()?;
        Ok(())
    }
}
