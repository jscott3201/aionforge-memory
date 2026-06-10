//! The drift sweep's behavior read (05 §1, M5.T05): the recent embedded episodes of
//! one namespace, in canonical order, capped.
//!
//! "Recent agent behavior" is the raw episode trace — written at capture, embedded
//! with the write-time model, never edited — deliberately *not* the consolidation's
//! derived facts, which are the poisonable downstream product. The read returns
//! stored vectors only; the drift detector never embeds anything itself.
//!
//! A label scan, like the forget sweep's: the drift sweep runs at host cadence (not
//! per recall), the window and cap bound the working set, and an episode the scan
//! cannot vouch for (no embedding, soft-forgotten, outside the window) simply drops
//! out. Selection keeps the `cap` **most recent** matches; the rows return in
//! ascending `(ingested_at, id)` — the canonical order that makes a replayed
//! behavior centroid byte-identical (float summation is order-sensitive even when
//! the mean is not).

use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::time::Timestamp;
use selene_core::db_string;
use selene_graph::RowIndex;

use crate::convert::{as_embedder_model, as_embedding, as_id, as_namespace, as_timestamp};
use crate::error::StoreError;
use crate::store::Store;

const ID: &str = "id";
const NAMESPACE: &str = "namespace";
const INGESTED_AT: &str = "ingested_at";
const EXPIRED_AT: &str = "expired_at";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";

/// One stored behavior vector: an episode's embedding with the model identity that
/// produced it. The model rides along so the drift layer can refuse a cross-space
/// comparison (02 §13.5) — an episode embedded under a different model than the
/// baseline is the *caller's* skip decision, not silently mixed in here.
#[derive(Debug, Clone, PartialEq)]
pub struct BehaviorVector {
    /// The stored content embedding.
    pub embedding: Embedding,
    /// The model that produced it, when recorded.
    pub embedder_model: Option<EmbedderModel>,
}

impl Store {
    /// The embedded, live episodes of `namespace` with `ingested_at` in
    /// `[since, until)`, keeping the `cap` most recent, returned in ascending
    /// `(ingested_at, id)` order (05 §1).
    ///
    /// Excluded, deliberately: soft-forgotten episodes (`expired_at` set — and the
    /// asymmetry is fail-closed: hiding on-baseline behavior can only *raise* the
    /// apparent drift, never mask it) and episodes without a stored embedding (the
    /// detector never embeds; what was never embedded is not measurable behavior).
    ///
    /// # Errors
    /// Returns [`StoreError`] if a property decode fails.
    pub fn recent_embedded_episodes(
        &self,
        namespace: &Namespace,
        since: &Timestamp,
        until: &Timestamp,
        cap: usize,
    ) -> Result<Vec<BehaviorVector>, StoreError> {
        if cap == 0 {
            return Ok(Vec::new());
        }
        let snapshot = self.graph().read();
        let label = db_string(Episode::LABEL)?;
        let Some(rows) = snapshot.nodes_with_label(&label) else {
            return Ok(Vec::new());
        };
        let id_key = db_string(ID)?;
        let namespace_key = db_string(NAMESPACE)?;
        let ingested_key = db_string(INGESTED_AT)?;
        let expired_key = db_string(EXPIRED_AT)?;
        let embedding_key = db_string(EMBEDDING)?;
        let model_key = db_string(EMBEDDER_MODEL)?;

        let since_ts = since.timestamp();
        let until_ts = until.timestamp();
        let mut matches: Vec<(Timestamp, Id, BehaviorVector)> = Vec::new();
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
            let Some(embedding_value) = props.get(&embedding_key) else {
                continue;
            };
            let Some(namespace_value) = props.get(&namespace_key) else {
                continue;
            };
            if as_namespace(namespace_value)? != *namespace {
                continue;
            }
            let Some(ingested_value) = props.get(&ingested_key) else {
                continue;
            };
            let ingested_at = as_timestamp(ingested_value)?;
            let instant = ingested_at.timestamp();
            if instant < since_ts || instant >= until_ts {
                continue;
            }
            let Some(id_value) = props.get(&id_key) else {
                continue;
            };
            matches.push((
                ingested_at,
                as_id(id_value)?,
                BehaviorVector {
                    embedding: as_embedding(embedding_value)?,
                    embedder_model: props.get(&model_key).map(as_embedder_model).transpose()?,
                },
            ));
        }

        // Keep the most recent `cap`, then hand back ascending canonical order. The
        // instant-based key ignores time-zone representation, with the id as the
        // total-order tie-break (the consolidation watermark's convention).
        matches.sort_by_key(|row| std::cmp::Reverse((row.0.timestamp(), row.1)));
        matches.truncate(cap);
        matches.reverse();
        Ok(matches.into_iter().map(|(_, _, vector)| vector).collect())
    }
}
