//! The off-cursor note-link evolver (M3.T09): the driver that runs a [`LinkEvolver`] over a
//! namespace's live notes and writes non-canonical `RELATES_TO` edges — entirely off the
//! consolidation cursor.
//!
//! This is the link half of the optional LLM layer, the sibling of the
//! [`Distiller`](crate::Distiller). The deterministic consolidation passes keep producing the
//! canonical fact and note tier inside the cursor's atomic flip; this driver runs separately — on
//! demand, at session end, or on a timer — pooling the namespace's live notes, offering each
//! source note its nearest embedding neighbors as candidates, asking the evolver which
//! relationships hold, and materializing the survivors through
//! [`Store::materialize_link_edges`]. Because it never touches an episode or the cursor, enabling
//! it cannot perturb the byte-deterministic consolidation replay, and a slow or unavailable model
//! degrades to the deterministic rule tier: the declined call is recorded and no edge lands.
//!
//! Three properties keep an LLM in the loop safe here:
//!
//! - **Closed vocabulary.** A proposed relationship label must be one of
//!   [`RELATIONSHIP_VOCABULARY`] — never free text — an anti-injection and anti-drift constraint
//!   the driver enforces even if a future evolver does not.
//! - **A cascade guard.** Per run the driver bounds the source notes evaluated, the candidates
//!   offered per note, the links created and revised, the distinct notes affected, and the number
//!   of times any one ordered pair may be revised — so a misbehaving model cannot churn the graph.
//! - **Bi-temporal, never lossy.** A relabeling is a *revision*: the prior version is closed
//!   (`valid_to` set) and a new one opened, the same close-and-replace shape as fact supersession,
//!   so the link's history is preserved and the store's one-current-relationship-per-pair
//!   invariant holds.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

use aionforge_domain::contracts::{EvolvedLink, LinkEvolver};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::time::Timestamp;
use aionforge_store::{EdgeId, LinkEdgeWrite, Store};

use crate::audit::{LinkDecision, LinkEvolveProvenance, link_evolve_actor_id, link_evolve_audit};

/// The closed relationship vocabulary for `RELATES_TO` labels (M3.T09). A proposal whose label is
/// not in this set is dropped: the label space is fixed (never model free text) so an injected or
/// drifting model cannot mint arbitrary relationship types.
pub const RELATIONSHIP_VOCABULARY: &[&str] = &[
    "related_to",
    "contradicts",
    "subsumes",
    "elaborates",
    "depends_on",
];

/// The one relationship the deterministic rule tier can infer — mere embedding proximity, not a
/// judged relation.
pub const RELATED_TO: &str = "related_to";

/// How the off-cursor link evolver is tuned. **Off by default** — `enabled` is the binding gate,
/// so a deployment that never sets it pays nothing and writes no links (M3.T09).
///
/// As with [`DistillationConfig`](crate::DistillationConfig), `endpoint` and `seed` are
/// **provenance to record, not behavior to drive**: they describe the completer the injected
/// evolver was built against (which the [`LinkEvolver`] seam does not expose), so the caller
/// supplies them from the same `CompleterConfig` for the `link_evolve` audit. A mismatch only
/// misrecords provenance; it cannot change what the model returns.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkEvolveConfig {
    /// Whether link evolution runs at all. **Default `false`.**
    pub enabled: bool,
    /// The completion endpoint to record in every call's provenance (the base URL — never a
    /// secret). `None` leaves it unrecorded.
    pub endpoint: Option<String>,
    /// The pinned sampling seed to record in every call's provenance. `None` leaves it unrecorded.
    pub seed: Option<i64>,
    /// The most live notes one run pools from the namespace (the candidate universe and the source
    /// set are drawn from this bounded, id-sorted pool).
    pub note_pool: usize,
    /// The most source notes one run evaluates (a bound on the evolver calls per invocation).
    pub max_source_notes_per_run: usize,
    /// The most candidates offered to the evolver per source note (its nearest embedding neighbors).
    pub max_candidates_per_note: usize,
    /// Proposals below this confidence are dropped before any edge decision.
    pub confidence_floor: f64,
    /// The most new links one run opens.
    pub max_links_created_per_run: usize,
    /// The most links one run revises (close-and-reopen).
    pub max_links_revised_per_run: usize,
    /// The most distinct source notes one run may write any edge for (the blast radius).
    pub max_notes_affected_per_run: usize,
    /// The most times any one ordered pair may be revised (counted as its closed versions), so a
    /// flip-flopping model cannot rewrite the same relationship without bound.
    pub max_revisions_per_link: usize,
}

impl Default for LinkEvolveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            seed: None,
            note_pool: 200,
            max_source_notes_per_run: 128,
            max_candidates_per_note: 8,
            confidence_floor: 0.6,
            max_links_created_per_run: 256,
            max_links_revised_per_run: 128,
            max_notes_affected_per_run: 128,
            max_revisions_per_link: 3,
        }
    }
}

/// What one link-evolution run did. Counts only; the per-note decisions and provenance live in the
/// `link_evolve` audit events the run wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkEvolveReport {
    /// Source notes the evolver was consulted on.
    pub notes_seen: usize,
    /// New links opened.
    pub links_created: usize,
    /// Links revised (a prior version closed and a new one opened).
    pub links_revised: usize,
    /// Source-note calls the evolver declined or could not complete (degraded to the rule tier).
    pub declined: usize,
}

/// An error from a link-evolution run. The evolver itself never errors out of the run — an
/// unavailable or failing model degrades to a declined call — so the only hard failures are a
/// store read or the final materializing write.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LinkEvolveError {
    /// A store read or the final materializing write failed.
    #[error("the store operation failed during link evolution")]
    Store(#[from] aionforge_store::StoreError),
}

/// The off-cursor link evolver: a [`LinkEvolver`] plus its tuning, run over the committed graph.
pub struct LinkEvolvePass<L> {
    evolver: L,
    config: LinkEvolveConfig,
}

impl<L: LinkEvolver> LinkEvolvePass<L> {
    /// Build a pass over an evolver and the link-evolution config.
    #[must_use]
    pub fn new(evolver: L, config: LinkEvolveConfig) -> Self {
        Self { evolver, config }
    }

    /// Evolve the live notes of one namespace into non-canonical `RELATES_TO` edges, off the
    /// consolidation cursor. A no-op (empty report) when disabled.
    ///
    /// # Errors
    /// Returns [`LinkEvolveError`] if a store read or the final write fails. A model that is
    /// unavailable or returns nothing usable is **not** an error — the call is recorded as declined
    /// and the run continues — so link evolution degrades to the deterministic rule tier.
    pub async fn evolve_links(
        &self,
        store: &Store,
        namespace: &Namespace,
        now: &Timestamp,
    ) -> Result<LinkEvolveReport, LinkEvolveError> {
        if !self.config.enabled {
            return Ok(LinkEvolveReport::default());
        }

        // The candidate universe and the source set: live notes that carry an embedding (a
        // candidate needs a vector to be scored), id-sorted by `notes_in_namespace` so a run is
        // reproducible.
        let mut pool = store.notes_in_namespace(namespace, self.config.note_pool)?;
        pool.retain(|note| note.embedding.is_some());

        let identity = self.evolver.identity().clone();
        let actor_id = link_evolve_actor_id(&identity);
        let provenance = LinkEvolveProvenance {
            identity: &identity,
            endpoint: self.config.endpoint.as_deref(),
            seed: self.config.seed,
        };

        let mut report = LinkEvolveReport::default();
        let mut creates: Vec<LinkEdgeWrite> = Vec::new();
        let mut closes: Vec<EdgeId> = Vec::new();
        let mut audits: Vec<AuditEvent> = Vec::new();
        let mut affected: HashSet<String> = HashSet::new();

        let source_count = pool.len().min(self.config.max_source_notes_per_run);
        for index in 0..source_count {
            // Once both write budgets are spent, stop consulting the model — nothing more can land.
            if report.links_created >= self.config.max_links_created_per_run
                && report.links_revised >= self.config.max_links_revised_per_run
            {
                tracing::info!("link evolution: per-run write caps reached; stopping early");
                break;
            }

            let source = &pool[index];
            let candidates = self.top_candidates(source, &pool);
            if candidates.is_empty() {
                continue;
            }

            report.notes_seen += 1;
            let proposed = match self.evolver.evolve(source, &candidates).await {
                Ok(Some(links)) => links,
                Ok(None) => {
                    report.declined += 1;
                    audits.push(link_evolve_audit(
                        &actor_id,
                        &source.identity.id,
                        &provenance,
                        "declined",
                        &[],
                        namespace,
                        now,
                    ));
                    continue;
                }
                Err(error) => {
                    // The seam contract degrades model failures to `Ok(None)`; an evolver that
                    // nonetheless errors is treated the same — recorded, never fatal to the run.
                    tracing::warn!(%error, "link evolution: evolver errored; declining source note");
                    report.declined += 1;
                    audits.push(link_evolve_audit(
                        &actor_id,
                        &source.identity.id,
                        &provenance,
                        "declined",
                        &[],
                        namespace,
                        now,
                    ));
                    continue;
                }
            };

            let valid = self.validate_proposals(&candidates, proposed);
            let existing = store.relates_to_links(&source.identity.id)?;
            let decisions = self.decide(
                source,
                &valid,
                &existing,
                &mut creates,
                &mut closes,
                &mut affected,
                &mut report,
                now,
            );
            // Audit material outcomes only: a decline (above) and a call that actually created or
            // revised an edge. A call that ran but drew nothing — the model proposed no
            // relationship, or every proposal was a no-op or filtered out — writes no audit, so the
            // audit trail stays proportional to what changed rather than to what was examined. The
            // examination count lives in `report.notes_seen`; `report.declined` separates declines.
            if !decisions.is_empty() {
                audits.push(link_evolve_audit(
                    &actor_id,
                    &source.identity.id,
                    &provenance,
                    "evolved",
                    &decisions,
                    namespace,
                    now,
                ));
            }
        }

        store.materialize_link_edges(&creates, &closes, &audits, now)?;
        Ok(report)
    }

    /// The source note's nearest embedding neighbors in the pool (excluding itself), best-first,
    /// at most `max_candidates_per_note`. Ties break by id so the candidate set is deterministic.
    fn top_candidates(&self, source: &Note, pool: &[Note]) -> Vec<Note> {
        let Some(source_vec) = source.embedding.as_ref() else {
            return Vec::new();
        };
        let source_id = source.identity.id.as_str();
        let mut scored: Vec<(f64, &Note)> = Vec::new();
        for note in pool {
            if note.identity.id.as_str() == source_id {
                continue;
            }
            let Some(other) = note.embedding.as_ref() else {
                continue;
            };
            scored.push((cosine(source_vec.as_slice(), other.as_slice()), note));
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.identity.id.as_str().cmp(b.1.identity.id.as_str()))
        });
        scored
            .into_iter()
            .take(self.config.max_candidates_per_note)
            .map(|(_, note)| note.clone())
            .collect()
    }

    /// Drop proposals that fail the floor, the closed vocabulary, or candidate membership, then
    /// keep the highest-confidence proposal per target. Returns them id-sorted (deterministic).
    fn validate_proposals(
        &self,
        candidates: &[Note],
        proposed: Vec<EvolvedLink>,
    ) -> Vec<EvolvedLink> {
        let candidate_ids: HashSet<&str> =
            candidates.iter().map(|c| c.identity.id.as_str()).collect();
        let mut best: BTreeMap<String, EvolvedLink> = BTreeMap::new();
        for link in proposed {
            if link.confidence < self.config.confidence_floor {
                continue;
            }
            if !RELATIONSHIP_VOCABULARY.contains(&link.relationship_label.as_str()) {
                continue;
            }
            if !candidate_ids.contains(link.target_id.as_str()) {
                continue;
            }
            match best.get(link.target_id.as_str()) {
                Some(kept) if kept.confidence >= link.confidence => {}
                _ => {
                    best.insert(link.target_id.as_str().to_string(), link);
                }
            }
        }
        best.into_values().collect()
    }

    /// Turn validated proposals into create/close writes for one source note, enforcing the
    /// per-run caps and the per-pair revision cap, and return the decisions for the audit.
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        source: &Note,
        valid: &[EvolvedLink],
        existing: &[aionforge_store::RelatesToLink],
        creates: &mut Vec<LinkEdgeWrite>,
        closes: &mut Vec<EdgeId>,
        affected: &mut HashSet<String>,
        report: &mut LinkEvolveReport,
        now: &Timestamp,
    ) -> Vec<LinkDecision> {
        let source_id = source.identity.id.as_str().to_string();
        let mut decisions = Vec::new();
        for link in valid {
            // The blast-radius guard: once the affected-notes budget is full, write nothing for a
            // source not already touched.
            if !affected.contains(&source_id)
                && affected.len() >= self.config.max_notes_affected_per_run
            {
                tracing::info!("link evolution: notes-affected cap reached; skipping source note");
                break;
            }

            let live = existing
                .iter()
                .find(|edge| edge.live && edge.target_id == link.target_id);
            match live {
                // Already current with this label — idempotent no-op (the store would skip it too).
                Some(edge) if edge.relationship_label == link.relationship_label => {}
                // Current with a different label — a revision: close the prior, open the new.
                Some(edge) => {
                    let revisions = existing
                        .iter()
                        .filter(|e| !e.live && e.target_id == link.target_id)
                        .count();
                    if revisions >= self.config.max_revisions_per_link {
                        tracing::info!("link evolution: per-pair revision cap reached; skipping");
                        continue;
                    }
                    if report.links_revised >= self.config.max_links_revised_per_run {
                        continue;
                    }
                    closes.push(edge.edge_id);
                    creates.push(LinkEdgeWrite {
                        source_id: source.identity.id.clone(),
                        target_id: link.target_id.clone(),
                        relationship_label: link.relationship_label.clone(),
                        valid_from: now.clone(),
                    });
                    report.links_revised += 1;
                    affected.insert(source_id.clone());
                    decisions.push(LinkDecision {
                        action: "revised",
                        target: link.target_id.as_str().to_string(),
                        label: link.relationship_label.clone(),
                        confidence: link.confidence,
                    });
                }
                // No current link — open a new one.
                None => {
                    if report.links_created >= self.config.max_links_created_per_run {
                        continue;
                    }
                    creates.push(LinkEdgeWrite {
                        source_id: source.identity.id.clone(),
                        target_id: link.target_id.clone(),
                        relationship_label: link.relationship_label.clone(),
                        valid_from: now.clone(),
                    });
                    report.links_created += 1;
                    affected.insert(source_id.clone());
                    decisions.push(LinkDecision {
                        action: "created",
                        target: link.target_id.as_str().to_string(),
                        label: link.relationship_label.clone(),
                        confidence: link.confidence,
                    });
                }
            }
        }
        decisions
    }
}

/// Cosine similarity of two equal-length vectors, clamped to `[0, 1]`. A length mismatch or a
/// zero-norm operand scores `0.0`; a negative cosine (dissimilar) is clamped to `0.0` so the score
/// reads as a relatedness confidence the driver's floor can threshold directly.
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::cosine;

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        assert!((cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn a_negative_cosine_clamps_to_zero() {
        // Opposed vectors have cosine -1; clamped so the score reads as a relatedness floor.
        assert_eq!(cosine(&[1.0, 0.0], &[-1.0, 0.0]), 0.0);
    }

    #[test]
    fn a_zero_norm_operand_scores_zero() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 1.0], &[0.0, 0.0]), 0.0);
    }

    #[test]
    fn a_length_mismatch_scores_zero() {
        assert_eq!(cosine(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
