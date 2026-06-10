//! The cooling-window primitives (05 §1, M5.T05): the recently-ingested fact page the
//! off-cursor cooling sweep walks, and the stamp-once `cooled_until` write.
//!
//! Facts materialize in-cursor with `cooled_until = None` — the proximity decision
//! reads off-cursor-written core-block baselines, so stamping inside consolidation
//! would break byte-identical replay (00 §52). The sweep walks this page on the
//! host's cadence instead, by `(ingested_at, id)` keyset watermark (the D1 shape),
//! and stamps each core-proximate fact exactly once: a fact already carrying a stamp
//! is never re-stamped or extended, so a re-scan is a true no-op. The stamp expires
//! by comparison at rank time — no write ever clears it.

use aionforge_domain::edges::Audit;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::time::Timestamp;
use selene_core::{LabelDiff, PropertyDiff, PropertyMap, db_string};
use selene_graph::RowIndex;

use crate::convert::{
    as_embedder_model, as_embedding, as_id, as_namespace, as_timestamp, key, timestamp_value,
};
use crate::error::StoreError;
use crate::store::Store;
use crate::{NodeId, audit};

const ID: &str = "id";
const NAMESPACE: &str = "namespace";
const INGESTED_AT: &str = "ingested_at";
const EXPIRED_AT: &str = "expired_at";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const COOLED_UNTIL: &str = "cooled_until";

/// The keyset position of the last fact a cooling page visited: resume strictly
/// after `(ingested_at, id)`. Instant-ordered, so the sweep naturally follows the
/// materialization stream; like the D1 watermark it is exact but not complete under
/// a backdated `ingested_at`, and the heal is the same occasional fresh pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoolingCursor {
    /// `ingested_at` of the last visited fact.
    pub ingested_at: Timestamp,
    /// Its id — the total-order tiebreak within one instant.
    pub id: Id,
}

/// One fact the cooling sweep considers, carrying exactly what the proximity
/// decision reads. An unembedded fact rides through (it advances the watermark) but
/// can never be proximate; a fact already stamped is likewise visited and skipped.
#[derive(Debug, Clone)]
pub struct CoolingCandidate {
    /// The committed node, for the stamp write.
    pub node: NodeId,
    /// The fact id, for the cursor and the audit subject.
    pub id: Id,
    /// Ingestion instant, the cursor's major key.
    pub ingested_at: Timestamp,
    /// The fact's namespace — proximity is judged against this namespace's core
    /// blocks only.
    pub namespace: Namespace,
    /// The stored content embedding, when the fact was embedded at write time.
    pub embedding: Option<Embedding>,
    /// The model that produced it, for the cross-space guard.
    pub embedder_model: Option<EmbedderModel>,
    /// Whether a cooling stamp is already present (visited, never re-stamped).
    pub cooled: bool,
}

/// The outcome of one [`Store::cool_fact`] write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoolWrite {
    /// The stamp landed and the audit row was co-committed.
    Applied,
    /// Already stamped, or soft-forgotten out of recall — nothing written, no audit.
    Noop,
}

impl Store {
    /// The live facts ingested strictly after `after`, ascending `(ingested_at, id)`,
    /// at most `limit` (05 §1). Soft-forgotten facts are excluded — they are outside
    /// recall, so a stamp would be dead weight — but unembedded and already-stamped
    /// facts are included so the watermark advances past them.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a property decode fails.
    pub fn cooling_candidates(
        &self,
        after: Option<&CoolingCursor>,
        limit: usize,
    ) -> Result<Vec<CoolingCandidate>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let snapshot = self.graph().read();
        let label = db_string(Fact::LABEL)?;
        let Some(rows) = snapshot.nodes_with_label(&label) else {
            return Ok(Vec::new());
        };
        let id_key = db_string(ID)?;
        let namespace_key = db_string(NAMESPACE)?;
        let ingested_key = db_string(INGESTED_AT)?;
        let expired_key = db_string(EXPIRED_AT)?;
        let embedding_key = db_string(EMBEDDING)?;
        let model_key = db_string(EMBEDDER_MODEL)?;
        let cooled_key = db_string(COOLED_UNTIL)?;
        let watermark = after.map(|cursor| (cursor.ingested_at.timestamp(), cursor.id));

        let mut candidates: Vec<CoolingCandidate> = Vec::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            if props.get(&expired_key).is_some() {
                continue;
            }
            let (Some(id_value), Some(namespace_value), Some(ingested_value)) = (
                props.get(&id_key),
                props.get(&namespace_key),
                props.get(&ingested_key),
            ) else {
                continue;
            };
            let id = as_id(id_value)?;
            let ingested_at = as_timestamp(ingested_value)?;
            if watermark.is_some_and(|mark| (ingested_at.timestamp(), id) <= mark) {
                continue;
            }
            candidates.push(CoolingCandidate {
                node,
                id,
                ingested_at,
                namespace: as_namespace(namespace_value)?,
                embedding: props.get(&embedding_key).map(as_embedding).transpose()?,
                embedder_model: props.get(&model_key).map(as_embedder_model).transpose()?,
                cooled: props.get(&cooled_key).is_some(),
            });
        }
        candidates.sort_by(|a, b| {
            (a.ingested_at.timestamp(), a.id).cmp(&(b.ingested_at.timestamp(), b.id))
        });
        candidates.truncate(limit);
        Ok(candidates)
    }

    /// Stamp one fact's cooling window: set `cooled_until = until` and co-commit the
    /// caller's `Cooled` audit row in the same transaction (05 §1). Gated under the
    /// write lock on a real transition — an existing stamp is never overwritten or
    /// extended, and a soft-forgotten fact is left untouched — so a replay converges
    /// with no second audit row.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the node has no properties or a read/write fails.
    pub fn cool_fact(
        &self,
        node: NodeId,
        until: &Timestamp,
        audit_event: &AuditEvent,
    ) -> Result<CoolWrite, StoreError> {
        let cooled_key = db_string(COOLED_UNTIL)?;
        let expired_key = db_string(EXPIRED_AT)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let outcome = {
            let mut mutator = txn.mutator();
            let (already_cooled, expired) = {
                let props = mutator.read().node_properties(node).ok_or_else(|| {
                    StoreError::invariant("cooling write target has no properties".to_string())
                })?;
                (
                    props.get(&cooled_key).is_some(),
                    props.get(&expired_key).is_some(),
                )
            };
            if already_cooled || expired {
                CoolWrite::Noop
            } else {
                let diff = PropertyDiff::new([(key(COOLED_UNTIL)?, timestamp_value(until))], [])?;
                mutator.update_node(node, LabelDiff::new([], [])?, diff)?;
                let ensured = audit::ensure_event(&mut mutator, audit_event, self.audit_signer())?;
                if ensured.created {
                    mutator.create_edge(
                        audit_edge,
                        ensured.node,
                        node,
                        PropertyMap::from_pairs(Vec::new())?,
                    )?;
                }
                CoolWrite::Applied
            }
        };
        txn.commit()?;
        Ok(outcome)
    }
}
