//! The L0 surface for the optional, off-by-default note-link evolution layer (M3.T09).
//!
//! Link evolution is the LLM-driven half of the associative `Note` tier: it draws and revises
//! `RELATES_TO` edges between notes. Like distillation it is **non-canonical and runs off the
//! consolidation cursor** — this surface writes edges in its own transaction, never inside an
//! episode flip, so enabling it cannot perturb the byte-deterministic consolidation path.
//!
//! `RELATES_TO` is **versioned bi-temporally** (02 §5): `valid_from` / `valid_to` plus
//! `ingested_at` / `expired_at`, exactly like a `Fact`'s validity. A link is *current* while its
//! `valid_to` is unset; revising one closes the prior version (sets `valid_to`/`expired_at`) and
//! opens a new one, the same close-and-replace shape as fact supersession ([`crate::materialize`]).
//!
//! Unlike a node, an edge is **not content-addressed** — selene allocates its `EdgeId` — so
//! idempotency here is *value-keyed*, not id-keyed: the driver reads the current links
//! ([`Store::relates_to_links`]), decides per ordered `(source, target)` pair whether to create,
//! leave alone, or revise, and this surface re-checks at write time so a crash-and-replay or a
//! double call writes nothing new. The surface **enforces one current relationship per ordered
//! pair**: a same-label create is a no-op, and a different-label create is refused while the pair
//! still has a live edge — so a relabel must be staged as a close of the current version followed
//! by a fresh create (closes run before creates in the same transaction, so a staged relabel sees
//! the pair already free).

use aionforge_domain::edges::RelatesTo;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::time::Timestamp;
use selene_core::{EdgeId, NodeId, PropertyDiff, PropertyMap, db_string};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{
    as_id, as_str, id_value, key, namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;
use crate::materialize::ensure_edge;
use crate::store::Store;
use crate::{audit, note};

// RELATES_TO per-edge fields (02 §5; catalog `RELATES_TO`).
const RELATIONSHIP_LABEL: &str = "relationship_label";
const VALID_FROM: &str = "valid_from";
const VALID_TO: &str = "valid_to";
const INGESTED_AT: &str = "ingested_at";
const EXPIRED_AT: &str = "expired_at";

/// One `RELATES_TO` edge out of a source note, as the link-evolution driver sees it: where it
/// points, how it is labeled, its `EdgeId` (so a revision can close it), and whether it is the
/// current version (`valid_to` unset and not expired).
#[derive(Debug, Clone)]
pub struct RelatesToLink {
    /// The target note this link points to.
    pub target_id: Id,
    /// The relationship label this version carries.
    pub relationship_label: String,
    /// The edge's allocated id, the handle a revision uses to close this version.
    pub edge_id: EdgeId,
    /// Whether this is the current version (not closed, not expired).
    pub live: bool,
}

/// A new `RELATES_TO` edge to open: a directed, labeled link valid from `valid_from`.
#[derive(Debug, Clone)]
pub struct LinkEdgeWrite {
    /// The source note the link points from.
    pub source_id: Id,
    /// The target note the link points to.
    pub target_id: Id,
    /// The relationship label (the caller validates it against the closed vocabulary).
    pub relationship_label: String,
    /// Event-time start of the link's validity window.
    pub valid_from: Timestamp,
}

impl Store {
    /// The live (unexpired) notes in one namespace, sorted by id and bounded by `limit` — the
    /// deterministic candidate pool the link-evolution driver pairs up (M3.T09).
    ///
    /// Bounding and sorting here make a run reproducible: the same graph yields the same pool in
    /// the same order, and a high-volume namespace cannot ask one run to consider everything.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a stored note cannot be decoded.
    pub fn notes_in_namespace(
        &self,
        namespace: &Namespace,
        limit: usize,
    ) -> Result<Vec<Note>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(Note::LABEL)?;
        let prop = db_string("namespace")?;
        let value = namespace_value(namespace)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
            return Ok(Vec::new());
        };
        let mut notes = Vec::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let note = note::from_properties(props)?;
            if note.identity.expired_at.is_none() {
                notes.push(note);
            }
        }
        notes.sort_by_key(|a| a.identity.id);
        notes.truncate(limit);
        Ok(notes)
    }

    /// Every `RELATES_TO` edge out of a source note (current and closed), so the driver can decide
    /// per target whether a proposed link is new, already present, or a relabeling — and count how
    /// many times a pair has already been revised, for the cascade guard (M3.T09). Both current and
    /// closed versions are returned; the driver reads [`RelatesToLink::live`] to tell them apart,
    /// staging only a *current* edge's `edge_id` into a revision's closes and counting closed
    /// versions per pair against the revision cap.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the source note id or an edge cannot be read.
    pub fn relates_to_links(&self, source_id: &Id) -> Result<Vec<RelatesToLink>, StoreError> {
        let snapshot = self.graph().read();
        let Some(source_node) = node_by_id(&snapshot, Note::LABEL, source_id)? else {
            return Ok(Vec::new());
        };
        let label = db_string(RelatesTo::LABEL)?;
        let Some(adjacency) = snapshot.outgoing_edges(source_node) else {
            return Ok(Vec::new());
        };
        let mut links = Vec::new();
        for edge in adjacency.iter_label(&label) {
            let Some(props) = snapshot.edge_properties(edge.edge_id) else {
                continue;
            };
            let relationship_label = match props.get(&db_string(RELATIONSHIP_LABEL)?) {
                Some(value) => as_str(value)?.to_string(),
                None => continue,
            };
            let live = props.get(&db_string(VALID_TO)?).is_none()
                && props.get(&db_string(EXPIRED_AT)?).is_none();
            let Some(target_props) = snapshot.node_properties(edge.neighbor) else {
                continue;
            };
            let target_id = match target_props.get(&db_string("id")?) {
                Some(value) => as_id(value)?,
                None => continue,
            };
            links.push(RelatesToLink {
                target_id,
                relationship_label,
                edge_id: edge.edge_id,
                live,
            });
        }
        Ok(links)
    }

    /// Materialize a link-evolution run in one fresh write transaction, off the consolidation
    /// cursor (M3.T09): close the `closes` edges (set `valid_to`/`expired_at`), open the `creates`
    /// edges, and write the `audits`.
    ///
    /// Both edge steps are idempotent so a crash-and-replay or a double call is a no-op: closing an
    /// already-closed (or absent) edge is skipped, and a create whose `(source, target)` pair
    /// already carries a current edge with the same label is skipped. The surface keeps **one
    /// current link per pair** — a create with a *different* label is refused while the prior
    /// version is still live (close it first to relabel), so a stale proposal cannot fork a pair
    /// into two current links. Each audit is deduped by its content-addressed id and wired
    /// `AuditEvent -AUDIT-> Note` to its subject (the source note); a subject absent from the graph
    /// still records the audit (the provenance is the payload) and only the edge is skipped.
    ///
    /// This never reads or writes any episode, cursor, or `consolidation_state`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a translation, a mutation, or the commit fails; a failed commit
    /// rolls back the whole run.
    pub fn materialize_link_edges(
        &self,
        creates: &[LinkEdgeWrite],
        closes: &[EdgeId],
        audits: &[AuditEvent],
        now: &Timestamp,
    ) -> Result<(), StoreError> {
        if creates.is_empty() && closes.is_empty() && audits.is_empty() {
            return Ok(());
        }
        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();

            // Close prior versions first (a revision closes the old before opening the new), each
            // idempotent: an absent edge is skipped (it can only be a stale or bad handle, since we
            // never delete edges), and one already carrying a `valid_to` is left as-is.
            let valid_to_key = db_string(VALID_TO)?;
            for &edge_id in closes {
                let open = match mutator.read().edge_properties(edge_id) {
                    Some(props) => props.get(&valid_to_key).is_none(),
                    None => {
                        tracing::warn!(
                            edge = ?edge_id,
                            "link evolution: edge to close not found; skipping (never existed or \
                             already removed)"
                        );
                        continue;
                    }
                };
                if !open {
                    continue; // already carries a `valid_to` — idempotent
                }
                mutator.update_edge(
                    edge_id,
                    PropertyDiff::new(
                        [
                            (key(VALID_TO)?, timestamp_value(now)),
                            (key(EXPIRED_AT)?, timestamp_value(now)),
                        ],
                        [],
                    )?,
                )?;
            }

            // Open new versions, enforcing one current link per ordered pair: skip a same-label
            // duplicate (idempotent), and refuse a different-label create while the pair still has a
            // live edge (the driver stages a relabel as a close + a create, in that order).
            for create in creates {
                let Some(source_node) = node_by_id(mutator.read(), Note::LABEL, &create.source_id)?
                else {
                    tracing::warn!(
                        source = %create.source_id,
                        "link evolution: source note not found; skipping link create"
                    );
                    continue;
                };
                let Some(target_node) = node_by_id(mutator.read(), Note::LABEL, &create.target_id)?
                else {
                    tracing::warn!(
                        target = %create.target_id,
                        "link evolution: target note not found; skipping link create"
                    );
                    continue;
                };
                match live_link_label(mutator.read(), source_node, target_node)? {
                    None => {}
                    Some(existing) if existing == create.relationship_label => {
                        continue; // a current link with this label exists — idempotent no-op
                    }
                    Some(existing) => {
                        tracing::warn!(
                            source = %create.source_id,
                            target = %create.target_id,
                            current = existing.as_str(),
                            proposed = create.relationship_label.as_str(),
                            "link evolution: a different current link exists on this pair; close it \
                             before relabeling — skipping create to keep one current link per pair"
                        );
                        continue;
                    }
                }
                mutator.create_edge(
                    db_string(RelatesTo::LABEL)?,
                    source_node,
                    target_node,
                    relates_to_props(&create.relationship_label, &create.valid_from, now)?,
                )?;
            }

            // Provenance audits: dedup by content-addressed id, then `AUDIT -> source Note`.
            for event in audits {
                let audit_node = audit::ensure_event(&mut mutator, event)?.node;
                match node_by_id(mutator.read(), Note::LABEL, &event.subject_id)? {
                    Some(subject_node) => ensure_edge(
                        &mut mutator,
                        aionforge_domain::edges::Audit::LABEL,
                        audit_node,
                        subject_node,
                        PropertyMap::from_pairs(Vec::new())?,
                    )?,
                    None => tracing::warn!(
                        audit = %event.identity.id,
                        subject = %event.subject_id,
                        "link evolution: audit subject note not found; skipping AUDIT edge"
                    ),
                }
            }
        }
        txn.commit()?;
        Ok(())
    }
}

/// The property map for a fresh `RELATES_TO` edge: the label and its open bi-temporal window
/// (`valid_to` / `expired_at` left unset to mark it current).
fn relates_to_props(
    relationship_label: &str,
    valid_from: &Timestamp,
    now: &Timestamp,
) -> Result<PropertyMap, StoreError> {
    Ok(PropertyMap::from_pairs(vec![
        (key(RELATIONSHIP_LABEL)?, string_value(relationship_label)?),
        (key(VALID_FROM)?, timestamp_value(valid_from)),
        (key(INGESTED_AT)?, timestamp_value(now)),
    ])?)
}

/// The label of the current `RELATES_TO` edge running `source -> target`, if one is live — the
/// value-keyed probe behind link idempotency and the one-current-relationship-per-pair invariant.
///
/// At most one version of a pair is current, so this returns that version's label, or `None` when
/// the pair has no live edge. A same-label create is then a no-op; a different-label create is a
/// relabel the caller must stage as a close of the current edge followed by a fresh create.
fn live_link_label(
    snapshot: &SeleneGraph,
    source: NodeId,
    target: NodeId,
) -> Result<Option<String>, StoreError> {
    let label = db_string(RelatesTo::LABEL)?;
    let Some(adjacency) = snapshot.outgoing_edges(source) else {
        return Ok(None);
    };
    let valid_to_key = db_string(VALID_TO)?;
    let expired_at_key = db_string(EXPIRED_AT)?;
    let relationship_label_key = db_string(RELATIONSHIP_LABEL)?;
    for edge in adjacency.iter_label(&label) {
        if edge.neighbor != target {
            continue;
        }
        let Some(props) = snapshot.edge_properties(edge.edge_id) else {
            continue;
        };
        let live = props.get(&valid_to_key).is_none() && props.get(&expired_at_key).is_none();
        if !live {
            continue;
        }
        if let Some(value) = props.get(&relationship_label_key) {
            return Ok(Some(as_str(value)?.to_string()));
        }
    }
    Ok(None)
}

/// The committed node carrying this id under the given label (ids are unique per kind).
fn node_by_id(snapshot: &SeleneGraph, label: &str, id: &Id) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(label)?;
    let prop = db_string("id")?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}
