//! Translation between a domain [`Note`] and a selene-db node (02 §4.6).
//!
//! The associative tier's link-evolution unit, and the home of conservative
//! consolidation summaries (M2.T06): a summary is a `Note` derived from the facts it
//! rolls up, never a `Fact` (a summary is not a canonical subject-anchored triple and
//! must stay out of the current-state machinery). The note carries `DERIVED_FROM`
//! lineage to its sources as edges; `derived_from_episode` is a single-id convenience.

use std::collections::HashMap;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::DerivedFrom;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::time::Timestamp;
use selene_core::{DbString, LabelSet, NodeId, PropertyMap, Value, db_string};
use selene_graph::{Mutator, RowIndex, SeleneGraph};

use crate::convert::{
    as_bool, as_embedder_model, as_embedding, as_f64, as_id, as_namespace, as_str, as_timestamp,
    as_u64, embedder_model_value, embedding_value, id_value, key, namespace_value, string_value,
    timestamp_value,
};
use crate::error::StoreError;
use crate::materialize::{FactKey, derived_from_props, ensure_edge, resolve_instruction_fact};
#[cfg(feature = "test-support")]
use crate::store::Store;

// Identity block (§3).
const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
// Stats block (§3).
const IMPORTANCE: &str = "importance";
const TRUST: &str = "trust";
const LAST_ACCESS: &str = "last_access";
const ACCESS_COUNT_RECENT: &str = "access_count_recent";
const REFERENCED_COUNT: &str = "referenced_count";
const SURPRISE: &str = "surprise";
const IS_PINNED: &str = "is_pinned";
// Note per-kind fields (§4.6).
const CONTENT: &str = "content";
const CONTEXT: &str = "context";
const KEYWORDS: &str = "keywords";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const DERIVED_FROM_EPISODE: &str = "derived_from_episode";

/// A conservative summary [`Note`] to materialize, with the facts it rolls up (M2.T06).
///
/// A summary is a `Note`, never a `Fact`: a roll-up is not a canonical subject-anchored
/// triple, and keeping it out of the `Fact` tier keeps it out of the supersession /
/// contradiction / current-state machinery. The note's id is content-derived from its
/// source set, so re-running the episode dedups to a no-op; `source_facts` are resolved to
/// nodes at materialize time (a source created this same txn from the in-txn fact map, a
/// committed one from the index) and wired as `Note -DERIVED_FROM-> Fact` lineage.
#[derive(Debug, Clone)]
pub struct MaterializedNote {
    /// The note node to write. Its id is content-addressed by the source set.
    pub note: Note,
    /// The facts this note summarizes; each becomes a `DERIVED_FROM` edge.
    pub source_facts: Vec<FactKey>,
}

/// The selene-db node label for a note (mirrors [`Note::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Note::LABEL)?))
}

/// Translate a [`Note`] into the `(labels, properties)` pair for `create_node`.
pub(crate) fn to_node(note: &Note) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(17);

    // Identity block.
    pairs.push((key(ID)?, id_value(&note.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&note.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&note.identity.namespace)?));
    if let Some(expired_at) = &note.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Stats block.
    pairs.push((key(IMPORTANCE)?, Value::Float(note.stats.importance)));
    pairs.push((key(TRUST)?, Value::Float(note.stats.trust)));
    pairs.push((key(LAST_ACCESS)?, timestamp_value(&note.stats.last_access)));
    pairs.push((
        key(ACCESS_COUNT_RECENT)?,
        Value::Uint(note.stats.access_count_recent),
    ));
    pairs.push((
        key(REFERENCED_COUNT)?,
        Value::Uint(note.stats.referenced_count),
    ));
    pairs.push((key(SURPRISE)?, Value::Float(note.stats.surprise)));
    pairs.push((key(IS_PINNED)?, Value::Bool(note.stats.is_pinned)));

    // Per-kind fields.
    pairs.push((key(CONTENT)?, string_value(&note.content)?));
    if let Some(context) = &note.context {
        pairs.push((key(CONTEXT)?, string_value(context)?));
    }
    if !note.keywords.is_empty() {
        let items = note
            .keywords
            .iter()
            .map(|k| string_value(k))
            .collect::<Result<Vec<_>, _>>()?;
        pairs.push((key(KEYWORDS)?, Value::List(items)));
    }
    if let Some(embedding) = &note.embedding {
        pairs.push((key(EMBEDDING)?, embedding_value(embedding)?));
    }
    if let Some(model) = &note.embedder_model {
        pairs.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
    }
    if let Some(episode) = &note.derived_from_episode {
        pairs.push((key(DERIVED_FROM_EPISODE)?, id_value(episode)?));
    }

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`Note`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Note, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };

    let identity = Identity {
        id: as_id(require(ID)?)?,
        ingested_at: as_timestamp(require(INGESTED_AT)?)?,
        namespace: as_namespace(require(NAMESPACE)?)?,
        expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
    };
    let stats = Stats {
        importance: as_f64(require(IMPORTANCE)?)?,
        trust: as_f64(require(TRUST)?)?,
        last_access: as_timestamp(require(LAST_ACCESS)?)?,
        access_count_recent: as_u64(require(ACCESS_COUNT_RECENT)?)?,
        referenced_count: as_u64(require(REFERENCED_COUNT)?)?,
        surprise: as_f64(require(SURPRISE)?)?,
        is_pinned: as_bool(require(IS_PINNED)?)?,
    };
    let keywords = match get(KEYWORDS)? {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| Ok(as_str(v)?.to_string()))
            .collect::<Result<Vec<_>, StoreError>>()?,
        _ => Vec::new(),
    };

    Ok(Note {
        identity,
        stats,
        content: as_str(require(CONTENT)?)?.to_string(),
        context: get(CONTEXT)?.map(as_str).transpose()?.map(str::to_string),
        keywords,
        embedding: get(EMBEDDING)?.map(as_embedding).transpose()?,
        embedder_model: get(EMBEDDER_MODEL)?.map(as_embedder_model).transpose()?,
        derived_from_episode: get(DERIVED_FROM_EPISODE)?.map(as_id).transpose()?,
    })
}

/// Materialize the summary notes (M2.T06) into the open flip transaction: write each
/// content-addressed note (deduped by id, so a replay writes no second copy) and wire its
/// `Note -DERIVED_FROM-> Fact` lineage to the facts it rolls up. A source fact resolves
/// like a supersession endpoint — one created in this same txn from `fact_nodes`, a
/// committed one from the index. An unresolvable source is a pass bug: degrade gracefully,
/// dropping just that lineage edge (logged) so one bad key cannot wedge the whole commit.
///
/// Returns the node of each note in input order, so a caller that must wire something else to
/// the note it just wrote (e.g. an `AuditEvent -AUDIT-> Note` provenance edge) need not re-probe.
/// The cursor path ignores the return.
pub(crate) fn materialize_notes(
    mutator: &mut Mutator<'_, '_>,
    notes: &[MaterializedNote],
    fact_nodes: &HashMap<String, NodeId>,
    canonical_id: &HashMap<Id, Id>,
    now: &Timestamp,
) -> Result<Vec<NodeId>, StoreError> {
    let mut note_nodes = Vec::with_capacity(notes.len());
    for materialized in notes {
        let note_node = match find_existing_note(mutator.read(), &materialized.note.identity.id)? {
            Some(node) => node, // already written by a prior run; reuse it (idempotent replay)
            None => {
                let (labels, props) = to_node(&materialized.note)?;
                mutator.create_node(labels, props)?
            }
        };
        for source in &materialized.source_facts {
            let Some(fact_node) =
                resolve_instruction_fact(mutator.read(), fact_nodes, canonical_id, source)?
            else {
                tracing::warn!(
                    note = %materialized.note.identity.id,
                    "consolidation: summary source fact unresolved; skipping DERIVED_FROM edge"
                );
                continue;
            };
            ensure_edge(
                mutator,
                DerivedFrom::LABEL,
                note_node,
                fact_node,
                derived_from_props(now)?,
            )?;
        }
        note_nodes.push(note_node);
    }
    Ok(note_nodes)
}

#[cfg(feature = "test-support")]
impl Store {
    /// Test-support: write a batch of `Note` nodes (and their `DERIVED_FROM` lineage to any
    /// already-committed source facts) in one fresh write transaction, through the same
    /// [`materialize_notes`] path the consolidation cursor uses.
    ///
    /// **Doc-hidden and for tests only.** It exists so integration tests in sibling crates can
    /// seed standalone `Note` nodes (with explicit embeddings, off any episode/cursor) without
    /// driving a full consolidation flip. It is content-addressed and deduped by id, so a re-run
    /// writes no second copy. It never touches an episode, cursor, or `consolidation_state`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a translation, a mutation, or the commit fails.
    #[doc(hidden)]
    pub fn seed_notes_for_test(
        &self,
        notes: &[MaterializedNote],
        now: &Timestamp,
    ) -> Result<Vec<NodeId>, StoreError> {
        let empty_fact_nodes: HashMap<String, NodeId> = HashMap::new();
        let empty_canonical: HashMap<Id, Id> = HashMap::new();
        let mut txn = self.graph().begin_write();
        let nodes = {
            let mut mutator = txn.mutator();
            materialize_notes(
                &mut mutator,
                notes,
                &empty_fact_nodes,
                &empty_canonical,
                now,
            )?
        };
        txn.commit()?;
        Ok(nodes)
    }
}

/// Find a summary note already written with this content-addressed id, returning its node.
/// `Note.id` is indexed (M2.T06), so this is a probe — the dedup that makes a replay of the
/// same episode write no second copy of a note it already produced.
fn find_existing_note(snapshot: &SeleneGraph, id: &Id) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(Note::LABEL)?;
    let prop = db_string("id")?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}
