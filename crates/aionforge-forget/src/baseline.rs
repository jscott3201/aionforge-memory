//! The attested drift baseline (05 §1, M5.T05): the schema riding in
//! `CoreBlock.drift_baseline` and its integrity checks.
//!
//! The baseline — not the block content — is the asset drift detection guards, so its
//! only write path is the attested `edit_core_block` flow (content unchanged, baseline
//! replaced): seeding or moving it is an identity decision a quorum co-signs, never a
//! detector decision. An un-attested setter would be the drift-laundering primitive —
//! poison behavior slowly, rebaseline quietly, and the detector measures distance from
//! the poisoned anchor.
//!
//! The schema is versioned, self-describing JSON. Two snapshots ride together because
//! they answer different operator questions: `block_embedding` anchors "how far is
//! behavior from this commitment?" and `behavior_centroid` anchors "how far has
//! behavior moved since we last affirmed it?". Both are derived, non-authoritative
//! state (02 §13.7) — fully rebuildable from committed episodes plus a fresh attested
//! edit.

use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::ContentHash;
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::time::Timestamp;
use serde::{Deserialize, Serialize};

/// The drift baseline stored in `CoreBlock.drift_baseline` (05 §1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftBaseline {
    /// Schema version; this module reads only [`DriftBaseline::VERSION`].
    pub v: u32,
    /// The embedding space every vector in this baseline lives in. Mandatory
    /// (02 §13.5): a baseline is meaningless without knowing its space, and the
    /// detector refuses any cross-space comparison.
    pub embedder_model: EmbedderModel,
    /// Hash of the block content at baseline time — the integrity anchor. An attested
    /// content edit that does not also rebaseline leaves this stale, which the
    /// detector reports as needs-rebaseline rather than scoring against an anchor
    /// that no longer describes the block.
    pub content_hash: ContentHash,
    /// Snapshot of the block's content embedding at baseline time: the identity
    /// anchor the score measures distance from.
    pub block_embedding: Embedding,
    /// The namespace behavior centroid observed at baseline time. `None` is the
    /// genesis-before-behavior state — a baseline attested before the namespace had
    /// any embedded episodes; nothing can drift from behavior never observed, so the
    /// block scores `0.0` until a rebaseline captures a real centroid.
    pub behavior_centroid: Option<Embedding>,
    /// When the baseline was attested.
    pub baselined_at: Timestamp,
    /// The behavior window (seconds) the centroid was computed over.
    pub window_secs: u64,
    /// How many episodes fed the centroid (`0` for genesis-before-behavior).
    pub sample_size: usize,
}

impl DriftBaseline {
    /// The schema version this module writes and the only one it reads.
    pub const VERSION: u32 = 1;

    /// Serialize for the `drift_baseline` JSON column (the `edit_core_block` draft).
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("a baseline of plain values serializes")
    }

    /// Parse a stored `drift_baseline` value, refusing an unknown schema version.
    ///
    /// # Errors
    /// Returns the human-readable reason (carried into the sweep report) when the
    /// JSON does not parse as this schema or the version is not
    /// [`DriftBaseline::VERSION`].
    pub fn from_value(value: &serde_json::Value) -> Result<Self, String> {
        let baseline: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("drift baseline does not parse: {error}"))?;
        if baseline.v != Self::VERSION {
            return Err(format!(
                "drift baseline schema version {} is not the supported {}",
                baseline.v,
                Self::VERSION
            ));
        }
        Ok(baseline)
    }

    /// Whether the block's current content is the content this baseline anchored.
    /// `false` means an attested edit moved the block since: the `block_embedding`
    /// snapshot no longer describes it, and the honest answer is needs-rebaseline,
    /// not a score.
    #[must_use]
    pub fn matches_content(&self, block: &CoreBlock) -> bool {
        self.content_hash == ContentHash::of(block.content.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::core::BlockKind;

    fn ts() -> Timestamp {
        "2026-06-10T09:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime")
    }

    fn model() -> EmbedderModel {
        EmbedderModel {
            family: "fake".to_string(),
            version: "1".to_string(),
            dimension: 4,
        }
    }

    fn baseline(content: &str) -> DriftBaseline {
        DriftBaseline {
            v: DriftBaseline::VERSION,
            embedder_model: model(),
            content_hash: ContentHash::of(content.as_bytes()),
            block_embedding: Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("embedding"),
            behavior_centroid: Some(Embedding::new(vec![0.0, 1.0, 0.0, 0.0]).expect("embedding")),
            baselined_at: ts(),
            window_secs: 604_800,
            sample_size: 12,
        }
    }

    fn block(content: &str) -> CoreBlock {
        CoreBlock {
            identity: Identity {
                id: Id::from_content_hash(b"block"),
                ingested_at: ts(),
                namespace: Namespace::Agent("owner".to_string()),
                expired_at: None,
            },
            stats: Stats {
                importance: 1.0,
                trust: 0.9,
                last_access: ts(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: content.to_string(),
            block_kind: BlockKind::Commitment,
            sensitivity: None,
            drift_baseline: None,
            embedding: None,
            embedder_model: None,
        }
    }

    #[test]
    fn round_trips_through_the_json_column_shape() {
        let original = baseline("never deploy on friday");
        let read = DriftBaseline::from_value(&original.to_value()).expect("parses back");
        assert_eq!(read, original);
    }

    #[test]
    fn genesis_centroid_none_round_trips() {
        let genesis = DriftBaseline {
            behavior_centroid: None,
            sample_size: 0,
            ..baseline("seeded before any behavior")
        };
        let read = DriftBaseline::from_value(&genesis.to_value()).expect("parses back");
        assert_eq!(read.behavior_centroid, None);
        assert_eq!(read.sample_size, 0);
    }

    #[test]
    fn refuses_garbage_and_foreign_versions() {
        assert!(DriftBaseline::from_value(&serde_json::json!({"summary": "old shape"})).is_err());
        assert!(DriftBaseline::from_value(&serde_json::json!(null)).is_err());
        let future = serde_json::json!({
            "v": 2,
            "embedder_model": {"family": "fake", "version": "1", "dimension": 4},
            "content_hash": "0123456789abcdef",
            "block_embedding": [1.0, 0.0],
            "behavior_centroid": null,
            "baselined_at": "2026-06-10T09:00:00-05:00[America/Chicago]",
            "window_secs": 1,
            "sample_size": 0
        });
        let error = DriftBaseline::from_value(&future).expect_err("future version refused");
        assert!(error.contains("version 2"), "{error}");
    }

    #[test]
    fn content_integrity_anchors_to_the_block_body() {
        let anchored = baseline("never deploy on friday");
        assert!(anchored.matches_content(&block("never deploy on friday")));
        assert!(
            !anchored.matches_content(&block("never deploy on friday or saturday")),
            "an edited block no longer matches the baseline anchor"
        );
    }
}
