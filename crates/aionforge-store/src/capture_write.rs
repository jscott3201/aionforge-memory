//! The capture write funnel and its forensic readers (04 §1).
//!
//! A split-out `impl Store` (Rust lets one impl span modules in a crate) holding the
//! single atomic commit that publishes a captured turn — episode, provenance, audit —
//! plus the standalone audit write for a rejected attempt that produces no memory node,
//! and the by-node-id readers used by tests and inspection. Sibling modules reach the
//! private graph through the `pub(crate)` [`Store::graph`] accessor, never the field.

use aionforge_domain::edges::{Audit, HasProvenance};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::{AuditEvent, ProvenanceRecord};
use selene_core::{NodeId, PropertyMap, db_string};

use crate::error::StoreError;
use crate::store::Store;
use crate::{audit, convert, episode, provenance};

impl Store {
    /// Commit a capture bundle through the single mutation funnel (04 §1).
    ///
    /// Writes the episode, its provenance record, and the capture audit event as one
    /// atomic commit, wiring `Episode -HAS_PROVENANCE-> ProvenanceRecord` and
    /// `AuditEvent -AUDIT-> Episode`. The caller has already set each record's
    /// `subject_id`/`actor_id` to the episode's domain id; the edges connect the
    /// freshly assigned node ids. Durable before visible, like every write here.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, any node/edge mutation, or the commit
    /// fails; nothing is published if any step fails.
    pub fn commit_capture(
        &self,
        episode: &Episode,
        provenance: &ProvenanceRecord,
        audit: &AuditEvent,
    ) -> Result<CaptureWriteIds, StoreError> {
        let (episode_labels, episode_props) = episode::to_node(episode)?;
        let (provenance_labels, provenance_props) = provenance::to_node(provenance)?;
        // Capture audits are fresh-id appends (no dedup funnel), but they sign through the
        // same commit-time stamp as every funneled author, so an enabled signer covers all
        // capture-channel events too.
        let audit = audit::signed_copy(audit, self.audit_signer());
        let (audit_labels, audit_props) = audit::to_node(audit.as_ref())?;
        let has_provenance = db_string(HasProvenance::LABEL)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let ids = {
            let mut mutator = txn.mutator();
            let episode_id = mutator.create_node(episode_labels, episode_props)?;
            let provenance_id = mutator.create_node(provenance_labels, provenance_props)?;
            mutator.create_edge(
                has_provenance,
                episode_id,
                provenance_id,
                PropertyMap::from_pairs(Vec::new())?,
            )?;
            // Direct create, not the `audit::ensure_event` funnel: capture audit ids are
            // freshly generated (UUIDv7), never content-addressed, so there is no predictable
            // id for a shadow copy to claim and no replay that should dedup — capture is an
            // append, and a duplicate id here is a real invariant violation that must surface.
            let audit_id = mutator.create_node(audit_labels, audit_props)?;
            mutator.create_edge(
                audit_edge,
                audit_id,
                episode_id,
                PropertyMap::from_pairs(Vec::new())?,
            )?;
            CaptureWriteIds {
                episode: episode_id,
                provenance: provenance_id,
                audit: audit_id,
            }
        };
        txn.commit()?;
        Ok(ids)
    }

    /// Write a single standalone audit event in its own transaction — for an event whose subject is
    /// not a committed memory node, such as a `namespace_denied` write rejection whose subject is
    /// the acting agent (06 §1, M4.T01).
    ///
    /// No `AUDIT` edge is wired: a capture audit points at its `Episode`, but a rejected write
    /// produces no memory node, and the agent subject has no node in the capture flow. The event is
    /// instead discoverable by the **scalar `kind` and `subject_id` indexes** (`subject_id` is the
    /// agent, so an M4.T06 by-subject lookup over `subject_id` returns an agent's denied attempts).
    /// The `(subject_id, occurred_at)` and `(kind, occurred_at)` composites are now built (the
    /// `indexes` module), and `actor_id` is scalar-indexed, so the by-subject, by-kind, and by-actor
    /// axes are all index-backed. The `(actor_id, occurred_at)` composite stays deferred — the
    /// `actor_id` scalar index plus an `occurred_at` sort covers that axis until a workload needs it.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translating the event or the commit fails.
    pub fn commit_audit(&self, audit: &AuditEvent) -> Result<NodeId, StoreError> {
        Ok(self.commit_audit_created(audit)?.0)
    }

    /// [`Store::commit_audit`], also answering whether this call **created** the row —
    /// `false` means a content-identical event (same content-addressed `AuditEvent.id`)
    /// was already committed and this call deduplicated to it. The drift sweep counts
    /// `warnings_emitted` over newly created rows only, so a re-scan over already-warned
    /// ground reads back as zero (the D1 sweep's replays-excluded convention).
    ///
    /// # Errors
    /// Returns [`StoreError`] if translating the event or the commit fails.
    pub fn commit_audit_created(&self, audit: &AuditEvent) -> Result<(NodeId, bool), StoreError> {
        let mut txn = self.graph().begin_write();
        let ensured = {
            let mut mutator = txn.mutator();
            // Content-addressed dedup against the in-txn working graph, under the write lock: a
            // replayed event (same `AuditEvent.id`) reuses the existing node, so a deterministic
            // retry — e.g. a refused attestation re-sent verbatim — never trips the `id` UNIQUE
            // constraint and surfaces a spurious store error. `ensure_event` also reconciles the
            // stored signature with the incoming copy's (the funnel for every audit author).
            audit::ensure_event(&mut mutator, audit, self.audit_signer())?
        };
        txn.commit()?;
        Ok((ensured.node, ensured.created))
    }

    /// Read a provenance record back by its node id (for tests and inspection).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded.
    pub fn provenance_by_node_id(
        &self,
        id: NodeId,
    ) -> Result<Option<ProvenanceRecord>, StoreError> {
        match self.graph().read().node_properties(id) {
            Some(props) => Ok(Some(provenance::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read the signed creation provenance attached to a captured episode by its **external**
    /// id, walking `Episode -HAS_PROVENANCE-> ProvenanceRecord`. This is the agent-facing
    /// "who wrote this" proof (`writer_agent_id` + model + `trust_at_write` + write instant),
    /// distinct from the System-namespace capture `AuditEvent` (governance forensics), which
    /// stays host-only.
    ///
    /// Only captured episodes carry a provenance edge, so a non-episode id — or an episode
    /// written without provenance — returns `None`. One read snapshot, matching the by-id read
    /// contract of [`Store::resolved_memory_by_id`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored provenance cannot be decoded.
    pub fn provenance_for(&self, episode_id: &Id) -> Result<Option<ProvenanceRecord>, StoreError> {
        let snapshot = self.graph().read();
        let Some(episode_node) = convert::node_by_id(&snapshot, Episode::LABEL, episode_id)? else {
            return Ok(None);
        };
        let has_provenance = db_string(HasProvenance::LABEL)?;
        let Some(adjacency) = snapshot.outgoing_edges(episode_node) else {
            return Ok(None);
        };
        let Some(edge) = adjacency.iter_label(&has_provenance).next() else {
            return Ok(None);
        };
        match snapshot.node_properties(edge.neighbor) {
            Some(props) => Ok(Some(provenance::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read an audit event back by its node id (for tests and inspection).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded.
    pub fn audit_event_by_node_id(&self, id: NodeId) -> Result<Option<AuditEvent>, StoreError> {
        match self.graph().read().node_properties(id) {
            Some(props) => Ok(Some(audit::from_properties(props)?)),
            None => Ok(None),
        }
    }
}

/// The node ids assigned by a [`Store::commit_capture`] write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureWriteIds {
    /// The committed episode.
    pub episode: NodeId,
    /// The provenance record proving the write.
    pub provenance: NodeId,
    /// The capture audit event.
    pub audit: NodeId,
}
