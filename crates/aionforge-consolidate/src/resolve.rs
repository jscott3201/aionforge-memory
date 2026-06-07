//! Entity resolution: map a surface form to a canonical entity (write-and-consolidation
//! §2, M2.T04).
//!
//! Resolution runs read-only against a store snapshot and is confined to the episode's
//! namespace (no global entity pool — a subliminal-trait safety boundary). For each
//! surface form the pipeline tries, in order: intra-episode coreference (the same or a
//! token-subset surface already seen this episode), an exact name/alias gate over the
//! BM25 entity index, then embedding clustering over the entity vector index (a neighbor
//! within `merge_threshold` of the surface embedding). Failing all three it forms a NEW
//! entity — the conservative default, since a wrong merge is far harder to undo than a
//! wrong split. New entities get a content-derived id (`Id::from_content_hash` over
//! namespace + type + normalized name), so the same surface always yields the same id,
//! which keeps the whole pass deterministic.

use aionforge_domain::contracts::EntitySurface;
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_store::{SearchKind, Store, StoreError};

use crate::config::ResolutionConfig;

/// How a surface form was resolved (recorded in the `canonicalize` audit decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolutionMethod {
    /// Folded into an entity already resolved earlier in this same episode.
    Coref,
    /// Matched an existing entity's canonical name or alias exactly.
    AliasExact,
    /// Matched an existing entity within the embedding merge threshold.
    EmbeddingCluster,
    /// No match — a new entity was formed.
    New,
}

impl ResolutionMethod {
    /// The stable string recorded in the audit payload.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ResolutionMethod::Coref => "coref",
            ResolutionMethod::AliasExact => "alias_exact",
            ResolutionMethod::EmbeddingCluster => "embedding_cluster",
            ResolutionMethod::New => "new",
        }
    }
}

/// The outcome of resolving one surface form.
#[derive(Debug, Clone)]
pub(crate) struct Resolution {
    /// The canonical entity id the surface resolved (or was minted) to.
    pub id: Id,
    /// The canonical name (the existing entity's, or this surface for a new entity);
    /// recorded in the `canonicalize` audit decision.
    pub canonical_name: String,
    /// Whether this resolution formed a new entity.
    pub is_new: bool,
    /// How the decision was reached.
    pub method: ResolutionMethod,
    /// Resolution confidence in `[0, 1]` (audit only; not the fact's confidence).
    pub confidence: f64,
    /// Ids of the candidate entities considered, for the audit trail.
    pub candidates: Vec<String>,
}

/// One entity resolved so far this episode, for intra-episode coreference and for
/// accumulating the aliases a new entity is created with.
struct CorefEntry {
    id: Id,
    canonical_name: String,
    entity_type: String,
    aliases: Vec<String>,
    is_new: bool,
}

/// The per-episode coreference accumulator (reset for each episode).
#[derive(Default)]
pub(crate) struct CorefTable {
    entries: Vec<CorefEntry>,
}

impl CorefTable {
    /// The new entities discovered this episode, as `(id, canonical_name, type, aliases)`.
    pub(crate) fn new_entities(&self) -> Vec<(Id, String, String, Vec<String>)> {
        self.entries
            .iter()
            .filter(|entry| entry.is_new)
            .map(|entry| {
                (
                    entry.id.clone(),
                    entry.canonical_name.clone(),
                    entry.entity_type.clone(),
                    entry.aliases.clone(),
                )
            })
            .collect()
    }

    /// Find an entry that corefers with `name`/`entity_type` and fold the surface in as an
    /// alias. Matches on a case-insensitive name/alias hit or a token-subset relation.
    fn match_and_fold(&mut self, name: &str, entity_type: &str) -> Option<Id> {
        let surface_tokens = tokens(name);
        let normalized = normalize(name);
        for entry in &mut self.entries {
            if entry.entity_type != entity_type {
                continue;
            }
            let exact = normalize(&entry.canonical_name) == normalized
                || entry.aliases.iter().any(|a| normalize(a) == normalized);
            let subsumes = {
                let canonical_tokens = tokens(&entry.canonical_name);
                is_subset(&surface_tokens, &canonical_tokens)
                    || is_subset(&canonical_tokens, &surface_tokens)
            };
            if exact || subsumes {
                fold_alias(entry, name);
                return Some(entry.id.clone());
            }
        }
        None
    }

    /// Record an existing-entity resolution so later surfaces in this episode coref to it.
    fn insert_existing(
        &mut self,
        id: Id,
        canonical_name: String,
        entity_type: String,
        surface: &str,
    ) {
        if self.entries.iter().any(|entry| entry.id == id) {
            return;
        }
        let mut entry = CorefEntry {
            id,
            canonical_name,
            entity_type,
            aliases: Vec::new(),
            is_new: false,
        };
        fold_alias(&mut entry, surface);
        self.entries.push(entry);
    }

    /// Record a newly-formed entity (its first surface is the canonical name).
    fn insert_new(&mut self, id: Id, canonical_name: String, entity_type: String) {
        self.entries.push(CorefEntry {
            id,
            canonical_name,
            entity_type,
            aliases: Vec::new(),
            is_new: true,
        });
    }
}

/// Resolve one surface form to a canonical entity within `namespace` (read-only).
///
/// # Errors
/// Returns [`StoreError`] if a candidate search or read fails.
pub(crate) fn resolve_surface(
    store: &Store,
    config: &ResolutionConfig,
    namespace: &Namespace,
    surface: &EntitySurface,
    embedding: &Embedding,
    coref: &mut CorefTable,
) -> Result<Resolution, StoreError> {
    let name = surface.surface.trim();

    // 1. Intra-episode coreference.
    if let Some(id) = coref.match_and_fold(name, &surface.entity_type) {
        return Ok(Resolution {
            id,
            canonical_name: name.to_string(),
            is_new: false,
            method: ResolutionMethod::Coref,
            confidence: 1.0,
            candidates: Vec::new(),
        });
    }

    // 2. Exact name/alias gate over the BM25 entity index.
    if let Some(hit) = exact_entity(store, config, namespace, name, &surface.entity_type)? {
        coref.insert_existing(
            hit.id.clone(),
            hit.canonical_name.clone(),
            surface.entity_type.clone(),
            name,
        );
        return Ok(Resolution {
            id: hit.id,
            canonical_name: hit.canonical_name,
            is_new: false,
            method: ResolutionMethod::AliasExact,
            confidence: 1.0,
            candidates: hit.candidates,
        });
    }

    // 3. Embedding clustering over the entity vector index.
    if let Some(hit) = nearest_entity(store, config, namespace, &surface.entity_type, embedding)? {
        coref.insert_existing(
            hit.id.clone(),
            hit.canonical_name.clone(),
            surface.entity_type.clone(),
            name,
        );
        return Ok(Resolution {
            id: hit.id,
            canonical_name: hit.canonical_name,
            is_new: false,
            method: ResolutionMethod::EmbeddingCluster,
            confidence: (1.0 - hit.distance).clamp(0.0, 1.0),
            candidates: hit.candidates,
        });
    }

    // 4. New entity (conservative default).
    let id = new_entity_id(namespace, &surface.entity_type, name);
    coref.insert_new(id.clone(), name.to_string(), surface.entity_type.clone());
    Ok(Resolution {
        id,
        canonical_name: name.to_string(),
        is_new: true,
        method: ResolutionMethod::New,
        confidence: 0.5,
        candidates: Vec::new(),
    })
}

/// A matched existing entity plus the candidate ids that were considered.
struct EntityHit {
    id: Id,
    canonical_name: String,
    candidates: Vec<String>,
    distance: f64,
}

/// Find an existing entity whose canonical name or an alias matches `name` exactly
/// (case-insensitive), within `namespace` and of the same type.
fn exact_entity(
    store: &Store,
    config: &ResolutionConfig,
    namespace: &Namespace,
    name: &str,
    entity_type: &str,
) -> Result<Option<EntityHit>, StoreError> {
    let mut candidates = Vec::new();
    for hit in store.text_search(SearchKind::Entity, name, config.candidate_k)? {
        let Some(entity) = store.entity_by_node_id(hit.node)? else {
            continue;
        };
        if entity.identity.namespace != *namespace || entity.entity_type != entity_type {
            continue;
        }
        candidates.push(entity.identity.id.as_str().to_string());
        let normalized = normalize(name);
        let matches = normalize(&entity.canonical_name) == normalized
            || entity.aliases.iter().any(|a| normalize(a) == normalized);
        if matches {
            return Ok(Some(EntityHit {
                id: entity.identity.id,
                canonical_name: entity.canonical_name,
                candidates,
                distance: 0.0,
            }));
        }
    }
    Ok(None)
}

/// Find the nearest existing entity within `merge_threshold` of `embedding`, in
/// `namespace` and of the same type. Hits are best-first, so the first in-namespace,
/// same-type neighbor under the threshold wins.
fn nearest_entity(
    store: &Store,
    config: &ResolutionConfig,
    namespace: &Namespace,
    entity_type: &str,
    embedding: &Embedding,
) -> Result<Option<EntityHit>, StoreError> {
    let mut candidates = Vec::new();
    for hit in store.vector_search_exact(SearchKind::Entity, embedding, config.candidate_k)? {
        let Some(entity) = store.entity_by_node_id(hit.node)? else {
            continue;
        };
        if entity.identity.namespace != *namespace || entity.entity_type != entity_type {
            continue;
        }
        candidates.push(entity.identity.id.as_str().to_string());
        if hit.score <= config.merge_threshold {
            return Ok(Some(EntityHit {
                id: entity.identity.id,
                canonical_name: entity.canonical_name,
                candidates,
                distance: hit.score,
            }));
        }
    }
    Ok(None)
}

/// The deterministic id for a new entity: a content hash over namespace, type, and the
/// `normalize`d name, so the same surface in the same namespace always mints the same id —
/// and, crucially, the same normalization the exact-match gate uses, so two surfaces that
/// differ only in case or spacing resolve to one entity rather than splitting into duplicates.
fn new_entity_id(namespace: &Namespace, entity_type: &str, name: &str) -> Id {
    let key = format!("{}|{}|{}", namespace, entity_type, normalize(name));
    Id::from_content_hash(key.as_bytes())
}

/// Add `surface` as an alias of `entry` unless it is the canonical name or already present
/// (compared in `normalize`d form, so case/spacing variants are not stored as separate aliases).
fn fold_alias(entry: &mut CorefEntry, surface: &str) {
    let normalized = normalize(surface);
    if normalize(&entry.canonical_name) == normalized
        || entry.aliases.iter().any(|a| normalize(a) == normalized)
    {
        return;
    }
    entry.aliases.push(surface.to_string());
}

/// The canonical identity/comparison form of a surface name: Unicode-lowercased, internal
/// whitespace runs collapsed to single spaces, ends trimmed. Both the new-entity id derivation
/// and every exact name/alias equality test route through this, so the two never disagree —
/// the source of the only normalization the module recognizes. `Resolution.canonical_name`
/// deliberately keeps the original-case surface for display; only identity and equality
/// normalize.
fn normalize(name: &str) -> String {
    name.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lowercased whitespace tokens of a surface form.
fn tokens(surface: &str) -> Vec<String> {
    surface.split_whitespace().map(str::to_lowercase).collect()
}

/// Whether every token of `needle` appears in `haystack` (and `needle` is non-empty).
fn is_subset(needle: &[String], haystack: &[String]) -> bool {
    !needle.is_empty() && needle.iter().all(|token| haystack.contains(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> Namespace {
        Namespace::Agent("tester".to_string())
    }

    #[test]
    fn normalize_collapses_case_and_internal_whitespace() {
        assert_eq!(normalize("New York"), "new york");
        assert_eq!(normalize("New   York"), "new york");
        assert_eq!(normalize("new\tyork"), "new york");
        assert_eq!(normalize("  Alice  "), "alice");
        assert_eq!(normalize("ALICE"), "alice");
    }

    #[test]
    fn new_entity_id_is_stable_across_case_and_spacing_variants() {
        // The whole point of the fix: case/spacing variants of the same surface mint one id,
        // so the exact-match gate and the id derivation never disagree and split a duplicate.
        let base = new_entity_id(&ns(), "Person", "New York");
        assert_eq!(base, new_entity_id(&ns(), "Person", "new york"));
        assert_eq!(base, new_entity_id(&ns(), "Person", "New   York"));
        assert_eq!(base, new_entity_id(&ns(), "Person", "  NEW YORK  "));
    }

    #[test]
    fn new_entity_id_separates_distinct_names_types_and_namespaces() {
        let a = new_entity_id(&ns(), "Person", "Alice");
        assert_ne!(a, new_entity_id(&ns(), "Person", "Bob"));
        assert_ne!(a, new_entity_id(&ns(), "Org", "Alice"));
        assert_ne!(
            a,
            new_entity_id(&Namespace::Agent("other".to_string()), "Person", "Alice")
        );
    }
}
