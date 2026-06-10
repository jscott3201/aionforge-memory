//! The semantic memory tier: canonical facts and entities (02 §4.2, §4.3).

use serde::{Deserialize, Serialize};

use crate::blocks::{Identity, Stats};
use crate::embedding::{EmbedderModel, Embedding};
use crate::ids::Id;
use crate::time::Timestamp;
use crate::value::ObjectValue;

/// The assertion lifecycle status of a fact (02 §4.2).
///
/// The fast scalar filter over fact validity; the maintained current-state
/// providers (02 §9) compute "what is true now" from the `ABOUT` and
/// supersession/contradiction edges. The storage layer applies the DB
/// `DEFAULT 'active'`; [`Default`] mirrors it for in-Rust construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactStatus {
    /// Asserted and currently believed.
    #[default]
    Active,
    /// Held aside pending review (e.g. low trust or contradiction).
    Quarantined,
    /// Replaced by a newer assertion via a supersession edge.
    Superseded,
}

/// A single source span backing an extracted fact (02 §6.2).
///
/// Locates the `[start, end)` byte range within the referenced episode's content
/// that the extractor drew the assertion from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceSpan {
    /// The episode the span points into.
    pub episode_id: Id,
    /// Inclusive start byte offset within the episode content.
    pub start: usize,
    /// Exclusive end byte offset within the episode content.
    pub end: usize,
}

/// Extractor identity and source provenance for a fact (`Fact.extraction`, 02 §6.2).
///
/// A well-defined shape (versus the open bags elsewhere), so it is a typed
/// sub-struct: extractor model family/version, the byte spans it drew from, and
/// the extraction-rule version that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Extraction {
    /// Extractor model family.
    pub extractor_model_family: Option<String>,
    /// Extractor model version.
    pub extractor_model_version: Option<String>,
    /// The episode source spans the assertion was drawn from.
    pub source_spans: Vec<SourceSpan>,
    /// Version of the extraction rule set that produced the fact.
    pub extraction_rule_version: Option<String>,
}

/// A semantic triple: a canonical, bi-temporal assertion (02 §4.2).
///
/// The triple is `(subject_id, predicate, object)` where the object collapses the
/// spec's `object_kind` / `object_entity_id` / `object_value` columns into the
/// single typed [`ObjectValue`]. The four bi-temporal validity timestamps live on
/// the fact's `ABOUT` edge (and on supersession/contradiction edges), not on the
/// `Fact` node. Currentness is modeled by **edge presence**: a fact is current iff
/// it has no live `SUPERSEDED_BY` and no live `CONTRADICTS` edge (the
/// `current_support_facts` provider rule, 02 §9). `status` is a **redundant scalar
/// mirror** of that edge-presence state — a fast filter narrowed in retrieval, not
/// the source of truth (02 §4.2; GAP-4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    /// Shared identity block.
    pub identity: Identity,
    /// Shared stats block.
    pub stats: Stats,
    /// The canonical `Entity.id` the fact is about (mirrored from the `ABOUT` edge).
    pub subject_id: Id,
    /// The relation.
    pub predicate: String,
    /// The typed object (entity reference, literal, or structured JSON).
    pub object: ObjectValue,
    /// Extraction/assertion confidence in `[0, 1]`.
    pub confidence: f64,
    /// The assertion lifecycle status.
    pub status: FactStatus,
    /// Canonical natural-language rendering (the BM25/embedding surface).
    pub statement: String,
    /// Embedding of `statement`, if computed.
    pub embedding: Option<Embedding>,
    /// Identity of the model that produced the embedding.
    pub embedder_model: Option<EmbedderModel>,
    /// Extractor identity and source provenance.
    pub extraction: Option<Extraction>,
    /// The cooling stamp (05 §1, M5.T05): set once by the off-cursor cooling sweep
    /// when the fact lands proximate to a high-trust core block. Rank-time trust is
    /// reduced until this instant and recovers without a write — the modulation is a
    /// pure read-time computation over this stamp. `None` = never cooled.
    pub cooled_until: Option<Timestamp>,
}

impl Fact {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Fact";
}

/// A canonical referent: the one node many surface forms resolve to (02 §4.3).
///
/// Canonicalization (collapsing many surface forms into one entity) is a
/// consolidation product; this type only holds the resolved record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    /// Shared identity block.
    pub identity: Identity,
    /// Shared stats block.
    pub stats: Stats,
    /// The preferred name.
    pub canonical_name: String,
    /// The entity type (e.g. `Person` / `Project` / `Repo` / `Tool` / `Concept` / `Place`).
    pub entity_type: String,
    /// Alternate surface forms; the BM25 alias surface.
    pub aliases: Vec<String>,
    /// Optional free-text description.
    pub description: Option<String>,
    /// Embedding, if computed.
    pub embedding: Option<Embedding>,
    /// Identity of the model that produced the embedding.
    pub embedder_model: Option<EmbedderModel>,
    /// Open attribute bag: attribute name -> value, for facets without a scalar
    /// column; intentionally an open shape (02 §6.3).
    pub attributes: Option<serde_json::Value>,
}

impl Entity {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Entity";
}
