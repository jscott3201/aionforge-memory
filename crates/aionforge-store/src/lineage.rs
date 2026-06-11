//! Writer-identity and note-lineage reads for the cross-family consolidation guard
//! (07 §3, M6.T01).
//!
//! The guard compares the consolidating model's family against the families of the
//! writers whose content it would condense. The families live on records that already
//! exist — `ProvenanceRecord.model_family` (the signed write-proof), `Episode.origin`
//! (the unsigned capture copy), `Agent.model_family` (the agent's current declaration),
//! and the `Distill` audit payload (the model that authored a distilled note) — but no
//! read joined them until now. This module is that join: off-cursor, read-only, no new
//! node, edge, or index.
//!
//! Strings come back **raw** (trimmed only at collection time, never rewritten):
//! family normalization is comparison-time logic that lives with the guard in
//! `aionforge-security`, which depends on this crate — the store cannot call it, and
//! stored provenance must never be mutated to fit a comparator anyway.
//!
//! Fail-closed posture: a source whose family cannot be resolved — a fact with no
//! source episode, an episode with no provenance/origin/agent family, a resolved-but-
//! empty string, a `Distill` event with a null family — flips
//! [`WriterFamilySet::unverifiable`] instead of being silently dropped. The guard
//! treats an unverifiable writer as a trigger, never as "differs" (07 §3: an
//! unverifiable family breaks both guard and lineage).

use std::collections::BTreeSet;

use aionforge_domain::edges::{Audit, DerivedFrom, HasProvenance};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::agent::Agent;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::{Episode, Origin};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::Fact;
use selene_core::db_string;
use selene_graph::SeleneGraph;

use crate::NodeId;
use crate::audit;
use crate::audit_read::MAX_AUDIT_PAGE;
use crate::convert::{as_id, as_str, json_from_value, node_by_id};
use crate::error::StoreError;
use crate::store::Store;

/// The episode/agent property carrying the writer model family (mirrors
/// [`crate::provenance`] and [`crate::agent`]).
const MODEL_FAMILY: &str = "model_family";
/// The episode property carrying the origin block (mirrors [`crate::episode`]).
const ORIGIN: &str = "origin";
/// The episode property carrying the capturing agent (mirrors [`crate::episode`]).
const AGENT_ID: &str = "agent_id";
/// The identity-block `id` property (mirrors every node module).
const ID: &str = "id";

/// The distinct writer model families behind a set of sources, plus whether any
/// source could not vouch for one.
///
/// `families` holds the **raw** recorded strings (trimmed, deduplicated, sorted for
/// determinism) — normalization belongs to the guard at comparison time.
/// `unverifiable` is sticky: one unresolvable source sets it regardless of how many
/// others resolved, because the guard must treat "we cannot say who wrote part of
/// this" as a trigger, not an average.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WriterFamilySet {
    /// Distinct raw writer families, trimmed, sorted ascending.
    pub families: Vec<String>,
    /// Whether any source's writer family could not be resolved (or was empty).
    pub unverifiable: bool,
}

/// The consolidating model recorded against a distilled note, decoded from its
/// `Distill` audit payload (the Note schema itself carries no model identity, 02 §4.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidatingModel {
    /// The recorded model family; `None` when the event recorded a null family.
    pub family: Option<String>,
    /// The recorded model version, if any.
    pub version: Option<String>,
}

/// A note's full lineage bundle: sources, producing model, and writer families —
/// the "lineage and model identity queryable via `DERIVED_FROM` + provenance"
/// acceptance surface (plan M6.T01), assembled from existing primitives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteLineage {
    /// The note's domain id.
    pub note: Id,
    /// Source facts the note condenses (`Note -DERIVED_FROM-> Fact`), id-sorted.
    pub source_facts: Vec<Id>,
    /// Source episodes behind those facts (and any direct `Note -DERIVED_FROM->
    /// Episode` wiring), id-sorted.
    pub source_episodes: Vec<Id>,
    /// The model that authored the note's content, when a `Distill` audit recorded
    /// one; `None` for a deterministic rule summary.
    pub consolidating_model: Option<ConsolidatingModel>,
    /// The writer families behind the note's sources (the guard's comparison input).
    pub writer_families: WriterFamilySet,
    /// Always `true`: a `Note` is structurally non-canonical (02 §4.6) — it never
    /// enters the current-fact path. Surfaced so the acceptance property is
    /// queryable rather than implicit.
    pub non_canonical: bool,
}

/// A mutable collection pass over writer families: raw trimmed strings plus the
/// sticky unverifiable flag.
#[derive(Default)]
struct FamilyCollector {
    families: BTreeSet<String>,
    unverifiable: bool,
}

impl FamilyCollector {
    /// Record one source's resolution outcome: `None` or an empty string marks the
    /// set unverifiable; anything else joins the distinct set.
    fn record(&mut self, family: Option<&str>) {
        match family {
            Some(raw) if !raw.trim().is_empty() => {
                self.families.insert(raw.trim().to_string());
            }
            _ => self.unverifiable = true,
        }
    }

    fn finish(self) -> WriterFamilySet {
        WriterFamilySet {
            families: self.families.into_iter().collect(),
            unverifiable: self.unverifiable,
        }
    }
}

impl Store {
    /// The distinct writer model families behind a set of facts, for the cross-family
    /// guard's per-cluster check (07 §3, M6.T01).
    ///
    /// Each fact resolves through `Fact -DERIVED_FROM-> Episode` and then the
    /// fail-closed chain: the signed `ProvenanceRecord` first, the unsigned
    /// `Episode.origin` copy when no record exists, the agent's current declaration
    /// last. A fact with no source episode, or any source that resolves to nothing
    /// or an empty string, sets `unverifiable` instead of being dropped — the guard
    /// must see that it cannot vouch.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a stored property cannot be decoded.
    pub fn writer_families_for_facts(
        &self,
        fact_ids: &[Id],
    ) -> Result<WriterFamilySet, StoreError> {
        let snapshot = self.graph().read();
        let mut collector = FamilyCollector::default();
        for fact_id in fact_ids {
            let Some(fact_node) = node_by_id(&snapshot, Fact::LABEL, fact_id)? else {
                // A source we cannot even see is a source we cannot vouch for.
                collector.unverifiable = true;
                continue;
            };
            collect_fact_writer_families(&snapshot, fact_node, &mut collector)?;
        }
        Ok(collector.finish())
    }

    /// The distinct writer model families behind one note, for the link-evolution
    /// guard (07 §3, M6.T01: "every rule that calls inference").
    ///
    /// Unions two chains, because a note has two kinds of author: (a) the episode
    /// writers behind its sources, via `Note -DERIVED_FROM-> Fact -DERIVED_FROM->
    /// Episode` (plus any direct `Note -DERIVED_FROM-> Episode` wiring), and (b) for
    /// a **distilled** note, the model that authored the note's content, recorded in
    /// its paired `Distill` audit payload. Omitting (b) would let a two-hop launder
    /// pass: distill with family X, then evolve links with X against a note whose
    /// underlying writers were some other family.
    ///
    /// A note with **no author evidence at all** — zero `DERIVED_FROM` sources and
    /// zero `Distill` events — is unverifiable, not unauthored: an empty writer set
    /// must never read as "differs from everyone".
    ///
    /// # Errors
    /// Returns [`StoreError`] if a stored property or audit row cannot be decoded.
    pub fn writer_families_for_note(&self, note_id: &Id) -> Result<WriterFamilySet, StoreError> {
        let mut collector = FamilyCollector::default();
        let sourced = {
            let snapshot = self.graph().read();
            let Some(note_node) = node_by_id(&snapshot, Note::LABEL, note_id)? else {
                collector.unverifiable = true;
                return Ok(collector.finish());
            };
            let (facts, episodes) = collect_note_sources(&snapshot, note_node, &mut collector)?;
            !facts.is_empty() || !episodes.is_empty()
        };
        // The distilling model union reads the audit index, outside the snapshot scope.
        let models = self.distill_models_for(note_id)?;
        for model in &models {
            collector.record(model.family.as_deref());
        }
        if !sourced && models.is_empty() {
            collector.unverifiable = true;
        }
        Ok(collector.finish())
    }

    /// The distinct, non-empty `Agent.model_family` declarations in the store, for
    /// the single-family startup check (07 §3): when every enrolled agent declares
    /// the same family as the configured consolidator, the deployment is
    /// single-family and the engine surfaces a startup warning.
    ///
    /// Best-effort by design — agents whose family is empty after trimming are
    /// skipped (the startup check is advisory; the per-call guard is the
    /// enforcement and handles unverifiable sources fail-closed).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the label or a property name cannot be encoded.
    pub fn distinct_agent_families(&self) -> Result<Vec<String>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(Agent::LABEL)?;
        let family_key = db_string(MODEL_FAMILY)?;
        let Some(rows) = snapshot.nodes_with_label(&label) else {
            return Ok(Vec::new());
        };
        let mut families: BTreeSet<String> = BTreeSet::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(selene_graph::RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let Some(value) = props.get(&family_key).cloned() else {
                continue;
            };
            let family = as_str(&value)?.trim().to_string();
            if !family.is_empty() {
                families.insert(family);
            }
        }
        Ok(families.into_iter().collect())
    }

    /// A note's lineage bundle — sources, producing model, writer families — or
    /// `None` when no live note carries the id (plan M6.T01: "lineage and model
    /// identity queryable via `DERIVED_FROM` + provenance").
    ///
    /// A point read: the producing model is decoded from the note's `Distill` audit
    /// payload, which is not an indexed column — callers wanting to filter notes
    /// *by* model family should drive the guard surface instead of scanning lineage.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a stored property or audit row cannot be decoded.
    pub fn note_lineage(&self, note_id: &Id) -> Result<Option<NoteLineage>, StoreError> {
        let (source_facts, source_episodes, writer_collector) = {
            let snapshot = self.graph().read();
            let Some(note_node) = node_by_id(&snapshot, Note::LABEL, note_id)? else {
                return Ok(None);
            };
            let mut collector = FamilyCollector::default();
            let (facts, episodes) = collect_note_sources(&snapshot, note_node, &mut collector)?;
            (facts, episodes, collector)
        };
        let mut collector = writer_collector;
        let models = self.distill_models_for(note_id)?;
        for model in &models {
            collector.record(model.family.as_deref());
        }
        Ok(Some(NoteLineage {
            note: *note_id,
            source_facts,
            source_episodes,
            consolidating_model: models.into_iter().last(),
            writer_families: collector.finish(),
            non_canonical: true,
        }))
    }

    /// The models recorded against a note's `Distill` audit events, ascending by
    /// `(occurred_at, id)` — empty for a rule summary, normally one entry for a
    /// distilled note (the event id is content-addressed, so replays dedup).
    ///
    /// Two linkage shapes are unioned, because the distiller's event names the
    /// **cluster subject** (an `Entity`) as its `subject_id` while pointing at the
    /// note through the `AuditEvent -AUDIT-> Note` edge: (a) the incoming `AUDIT`
    /// edge walk (the real distiller's shape), and (b) the `(subject_id, kind)`
    /// index for any event whose subject IS the note. A point read over one node's
    /// edges — the by-index spine caveat in `audit_read` concerns history scans,
    /// not this single-hop linkage that is written atomically with the note.
    fn distill_models_for(&self, note_id: &Id) -> Result<Vec<ConsolidatingModel>, StoreError> {
        let mut events: Vec<AuditEvent> = Vec::new();
        {
            let snapshot = self.graph().read();
            if let Some(note_node) = node_by_id(&snapshot, Note::LABEL, note_id)? {
                let audit_edge = db_string(Audit::LABEL)?;
                if let Some(adjacency) = snapshot.incoming_edges(note_node) {
                    for edge in adjacency.iter_label(&audit_edge) {
                        let Some(props) = snapshot.node_properties(edge.neighbor) else {
                            continue;
                        };
                        let event = audit::from_properties(props)?;
                        if event.kind == AuditKind::Distill {
                            events.push(event);
                        }
                    }
                }
            }
        }
        let history =
            self.audit_by_subject_kind(note_id, AuditKind::Distill, None, MAX_AUDIT_PAGE)?;
        for event in history.events {
            if !events.iter().any(|e| e.identity.id == event.identity.id) {
                events.push(event);
            }
        }
        events
            .sort_by(|a, b| (&a.occurred_at, a.identity.id).cmp(&(&b.occurred_at, b.identity.id)));
        let mut models = Vec::new();
        for event in &events {
            let family = event.payload.get(MODEL_FAMILY).and_then(|v| v.as_str());
            let version = event
                .payload
                .get("model_version")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            models.push(ConsolidatingModel {
                family: family.map(str::to_string),
                version,
            });
        }
        Ok(models)
    }
}

/// Collect the writer families behind one fact node: every `DERIVED_FROM` episode
/// neighbor resolves through [`episode_family_in`]; a fact with no episode source is
/// itself unverifiable (nothing vouches for who wrote it).
fn collect_fact_writer_families(
    snapshot: &SeleneGraph,
    fact_node: NodeId,
    collector: &mut FamilyCollector,
) -> Result<(), StoreError> {
    let derived = db_string(DerivedFrom::LABEL)?;
    let episode_label = db_string(Episode::LABEL)?;
    let mut episode_sources = 0usize;
    if let Some(adjacency) = snapshot.outgoing_edges(fact_node) {
        for edge in adjacency.iter_label(&derived) {
            // DERIVED_FROM is polymorphic; only an Episode carries writer identity.
            let is_episode = snapshot
                .node_labels(edge.neighbor)
                .is_some_and(|labels| labels.contains(&episode_label));
            if !is_episode {
                continue;
            }
            episode_sources += 1;
            collector.record(episode_family_in(snapshot, edge.neighbor)?.as_deref());
        }
    }
    if episode_sources == 0 {
        collector.unverifiable = true;
    }
    Ok(())
}

/// The walk behind [`Store::writer_families_for_note`] and [`Store::note_lineage`]:
/// returns the note's source fact and episode domain ids (each id-sorted, distinct)
/// while feeding every resolved writer family into `collector`.
fn collect_note_sources(
    snapshot: &SeleneGraph,
    note_node: NodeId,
    collector: &mut FamilyCollector,
) -> Result<(Vec<Id>, Vec<Id>), StoreError> {
    let derived = db_string(DerivedFrom::LABEL)?;
    let fact_label = db_string(Fact::LABEL)?;
    let episode_label = db_string(Episode::LABEL)?;
    let id_key = db_string(ID)?;
    let mut facts: BTreeSet<Id> = BTreeSet::new();
    let mut episodes: BTreeSet<Id> = BTreeSet::new();
    if let Some(adjacency) = snapshot.outgoing_edges(note_node) {
        for edge in adjacency.iter_label(&derived) {
            let Some(labels) = snapshot.node_labels(edge.neighbor) else {
                continue;
            };
            if labels.contains(&fact_label) {
                if let Some(value) = snapshot
                    .node_properties(edge.neighbor)
                    .and_then(|props| props.get(&id_key).cloned())
                {
                    facts.insert(as_id(&value)?);
                }
                collect_fact_writer_families(snapshot, edge.neighbor, collector)?;
                collect_fact_source_episodes(snapshot, edge.neighbor, &mut episodes)?;
            } else if labels.contains(&episode_label) {
                if let Some(value) = snapshot
                    .node_properties(edge.neighbor)
                    .and_then(|props| props.get(&id_key).cloned())
                {
                    episodes.insert(as_id(&value)?);
                }
                collector.record(episode_family_in(snapshot, edge.neighbor)?.as_deref());
            }
        }
    }
    Ok((facts.into_iter().collect(), episodes.into_iter().collect()))
}

/// Append the domain ids of a fact's `DERIVED_FROM` episode sources.
fn collect_fact_source_episodes(
    snapshot: &SeleneGraph,
    fact_node: NodeId,
    episodes: &mut BTreeSet<Id>,
) -> Result<(), StoreError> {
    let derived = db_string(DerivedFrom::LABEL)?;
    let episode_label = db_string(Episode::LABEL)?;
    let id_key = db_string(ID)?;
    if let Some(adjacency) = snapshot.outgoing_edges(fact_node) {
        for edge in adjacency.iter_label(&derived) {
            let is_episode = snapshot
                .node_labels(edge.neighbor)
                .is_some_and(|labels| labels.contains(&episode_label));
            if !is_episode {
                continue;
            }
            if let Some(value) = snapshot
                .node_properties(edge.neighbor)
                .and_then(|props| props.get(&id_key).cloned())
            {
                episodes.insert(as_id(&value)?);
            }
        }
    }
    Ok(())
}

/// Resolve one episode's writer family through the fail-closed chain (Q4 of the
/// M6.T01 design synthesis):
///
/// 1. `Episode -HAS_PROVENANCE-> ProvenanceRecord.model_family` — the signed (or at
///    least write-time-recorded) proof. When a record exists its value is final,
///    even when empty: falling through past a signed empty family would let a
///    mutable later declaration launder an unverifiable write.
/// 2. `Episode.origin.model_family` — the unsigned capture copy, when no record.
/// 3. `Agent.model_family` via `Episode.agent_id` — the agent's *current*
///    declaration, weakest because it can drift from what wrote the episode.
/// 4. `None` — unverifiable.
fn episode_family_in(
    snapshot: &SeleneGraph,
    episode_node: NodeId,
) -> Result<Option<String>, StoreError> {
    let provenance = db_string(HasProvenance::LABEL)?;
    let family_key = db_string(MODEL_FAMILY)?;
    if let Some(adjacency) = snapshot.outgoing_edges(episode_node)
        && let Some(edge) = adjacency.iter_label(&provenance).next()
    {
        let family = snapshot
            .node_properties(edge.neighbor)
            .and_then(|props| props.get(&family_key).cloned());
        return match family {
            Some(value) => Ok(Some(as_str(&value)?.to_string())),
            // A provenance record with no family column decodes as recorded-empty.
            None => Ok(Some(String::new())),
        };
    }
    let Some(props) = snapshot.node_properties(episode_node) else {
        return Ok(None);
    };
    let origin_key = db_string(ORIGIN)?;
    if let Some(value) = props.get(&origin_key).cloned() {
        let origin: Origin = json_from_value(&value)?;
        if let Some(family) = origin.model_family {
            return Ok(Some(family));
        }
    }
    let agent_key = db_string(AGENT_ID)?;
    if let Some(value) = props.get(&agent_key).cloned() {
        let agent_id = as_id(&value)?;
        if let Some(agent_node) = node_by_id(snapshot, Agent::LABEL, &agent_id)?
            && let Some(value) = snapshot
                .node_properties(agent_node)
                .and_then(|props| props.get(&family_key).cloned())
        {
            return Ok(Some(as_str(&value)?.to_string()));
        }
    }
    Ok(None)
}
