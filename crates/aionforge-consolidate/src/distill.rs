//! The off-cursor LLM distiller (M3.T08): the driver that runs an [`LLMSummarizer`] over the
//! committed graph and writes non-canonical distilled notes — entirely off the consolidation
//! cursor.
//!
//! This is the half of distillation the spec keeps off the critical consolidation path (04
//! §*Canonical vs. distilled*, plan.md M3.T08). The deterministic
//! [`RuleSummarizer`](crate::RuleSummarizer) keeps producing the canonical summary notes inside
//! the cursor's atomic flip; this driver runs separately — on demand, at session end, or on a
//! timer — reading the current support facts, condensing each subject's cluster with the model,
//! and materializing the survivors through [`Store::materialize_distilled_notes`]. Because it
//! never touches an episode or the cursor, enabling it cannot perturb the byte-deterministic
//! consolidation replay, and a slow or unavailable model degrades to the canonical tier: each
//! such call is recorded as declined and no note lands.
//!
//! The clustering, the content-addressed note id, and the detail-retention guard are shared with
//! the cursor summary pass ([`crate::summarize`]); the distilled note's id is keyed on the
//! distiller's own rule version, so it occupies an id-space disjoint from the rule summaries and
//! the two tiers coexist. Every call — written, rejected as lossy, or declined — is recorded in a
//! `distill` audit carrying the model identity, endpoint, and seed (M3.T08), wired to the note it
//! produced (or to the subject entity when it produced none).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{Embedder, SummarizationCluster, Summarizer, SummaryOutput};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{CandidateSet, DistilledNoteWrite, FactKey, MaterializedNote, Store};

use crate::audit::{DistillProvenance, distill_actor_id, distill_audit};
use crate::config::SummarizationConfig;
use crate::summarize::{RetentionOutcome, build_clusters, check_detail_retention, note_id};

/// How the off-cursor distiller is tuned. **Off by default** — `enabled` is the binding gate, so
/// a deployment that never sets it pays nothing and writes no distilled notes (M3.T08).
///
/// `endpoint` and `seed` are **provenance to record, not behavior to drive** — they describe the
/// completer the injected summarizer was built against, which the `Summarizer` seam does not
/// expose. The caller therefore supplies them here from the same `CompleterConfig` it used to build
/// the completer (the endpoint base URL and the client's pinned seed), so the `distill` audit can
/// record the full model provenance for the cross-family guard. Keeping them in sync with the
/// completer is the caller's responsibility; a mismatch only misrecords provenance, it cannot
/// change what the model returns.
#[derive(Debug, Clone, PartialEq)]
pub struct DistillationConfig {
    /// Whether distillation runs at all. **Default `false`.**
    pub enabled: bool,
    /// The completion endpoint to record in every call's provenance (the base URL — never a
    /// secret; the API key lives only in the completer). Supplied by the caller from its completer
    /// configuration; `None` leaves it unrecorded.
    pub endpoint: Option<String>,
    /// The pinned sampling seed to record in every call's provenance. Supplied by the caller from
    /// the completer's configured seed; `None` leaves it unrecorded.
    pub seed: Option<i64>,
    /// The most clusters one run will distill (a bound on the model calls a single invocation
    /// can make; the rest wait for the next run).
    pub max_clusters_per_run: usize,
    /// Clustering and detail-retention gates, shared with the cursor summary pass: a cluster must
    /// clear the size gates to be distilled, and a summary that drops too much specificity is
    /// rejected rather than written.
    pub summarization: SummarizationConfig,
}

impl Default for DistillationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            seed: None,
            max_clusters_per_run: 128,
            summarization: SummarizationConfig::default(),
        }
    }
}

/// What one distillation run did. Counts only; the per-cluster verdicts and provenance live in
/// the `distill` audit events the run wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DistillationReport {
    /// Clusters considered (after the size gates, bounded by `max_clusters_per_run`).
    pub clusters_seen: usize,
    /// Notes written (the model produced a summary that cleared the detail-retention guard).
    pub notes_written: usize,
    /// Calls whose summary was rejected as lossy by the detail-retention guard.
    pub rejected_lossy: usize,
    /// Calls the model declined or could not complete (unavailable, truncated, empty).
    pub declined: usize,
}

/// An error from a distillation run. The summarizer itself never errors out of the run — an
/// unavailable or failing model degrades to a declined call — so the only hard failures are a
/// store read/write or the note-body embedding.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DistillError {
    /// A store read or the final materializing write failed.
    #[error("the store operation failed during distillation")]
    Store(#[from] aionforge_store::StoreError),

    /// Embedding the distilled note bodies failed.
    #[error("embedding distilled notes failed: {0}")]
    Embed(String),
}

/// The off-cursor distiller: an [`LLMSummarizer`](crate::LLMSummarizer) (or any [`Summarizer`])
/// plus the embedder for note bodies, run over the committed graph.
pub struct Distiller<S, E> {
    summarizer: S,
    embedder: Arc<E>,
    config: DistillationConfig,
}

impl<S: Summarizer, E: Embedder> Distiller<S, E> {
    /// Build a distiller over a summarizer, the shared embedder, and the distillation config.
    #[must_use]
    pub fn new(summarizer: S, embedder: Arc<E>, config: DistillationConfig) -> Self {
        Self {
            summarizer,
            embedder,
            config,
        }
    }

    /// Distill the current support facts of one namespace into non-canonical notes, off the
    /// consolidation cursor. A no-op (empty report) when disabled.
    ///
    /// # Errors
    /// Returns [`DistillError`] if a store read, the note-body embedding, or the final write
    /// fails. A model that is unavailable or returns an unusable result is **not** an error — the
    /// call is recorded as declined and the run continues — so distillation degrades to the
    /// canonical tier rather than failing.
    pub async fn distill(
        &self,
        store: &Store,
        namespace: &Namespace,
        now: &Timestamp,
    ) -> Result<DistillationReport, DistillError> {
        if !self.config.enabled {
            return Ok(DistillationReport::default());
        }

        let facts = self.current_support_facts(store, namespace)?;
        let name_of = self.name_entities(store, &facts)?;
        let mut clusters = build_clusters(&facts, &name_of, &self.config.summarization);
        if clusters.len() > self.config.max_clusters_per_run {
            tracing::info!(
                seen = clusters.len(),
                cap = self.config.max_clusters_per_run,
                "distiller: more clusters than the per-run cap; the rest wait for the next run"
            );
            clusters.truncate(self.config.max_clusters_per_run);
        }

        let identity = self.summarizer.identity().clone();
        let rule_version = identity.rule_version.clone();
        let actor_id = distill_actor_id(&identity);
        let provenance = DistillProvenance {
            identity: &identity,
            endpoint: self.config.endpoint.as_deref(),
            seed: self.config.seed,
        };

        let mut report = DistillationReport {
            clusters_seen: clusters.len(),
            ..DistillationReport::default()
        };
        let mut declined: Vec<AuditEvent> = Vec::new();
        let mut pending: Vec<Pending> = Vec::new();

        for cluster in clusters {
            let output = match self.summarizer.summarize(&cluster).await {
                Ok(Some(output)) => output,
                Ok(None) => {
                    report.declined += 1;
                    declined.push(distill_audit(
                        &actor_id,
                        &cluster,
                        &provenance,
                        "declined",
                        None,
                        None,
                        namespace,
                        now,
                    ));
                    continue;
                }
                Err(error) => {
                    // The seam contract degrades model failures to `Ok(None)`; a summarizer that
                    // nonetheless errors is treated the same — recorded, never fatal to the run.
                    tracing::warn!(%error, "distiller: summarizer errored; declining cluster");
                    report.declined += 1;
                    declined.push(distill_audit(
                        &actor_id,
                        &cluster,
                        &provenance,
                        "declined",
                        None,
                        None,
                        namespace,
                        now,
                    ));
                    continue;
                }
            };

            let retention = check_detail_retention(&cluster, &output, &self.config.summarization);
            if !retention.passed {
                report.rejected_lossy += 1;
                declined.push(distill_audit(
                    &actor_id,
                    &cluster,
                    &provenance,
                    "rejected_lossy",
                    Some(&retention),
                    None,
                    namespace,
                    now,
                ));
                continue;
            }

            let id = note_id(namespace, &cluster, &rule_version);
            pending.push(Pending {
                cluster,
                output,
                id,
                retention,
            });
        }

        // Embed the surviving note bodies in one batch so the distilled notes are vector-searchable.
        let contents: Vec<String> = pending.iter().map(|p| p.output.content.clone()).collect();
        let embeddings = self.embed(contents).await?;
        // Fail closed on a slot-count mismatch (the embedder contract is one vector per input):
        // a short batch would otherwise silently drop the tail notes when zipped.
        if embeddings.len() != pending.len() {
            return Err(DistillError::Embed(format!(
                "embedder returned {} vectors for {} note bodies",
                embeddings.len(),
                pending.len()
            )));
        }
        let model = self.embedder.model().clone();

        let mut written: Vec<DistilledNoteWrite> = Vec::with_capacity(pending.len());
        for (pending, embedding) in pending.into_iter().zip(embeddings) {
            let Pending {
                cluster,
                output,
                id,
                retention,
            } = pending;
            let source_facts = cluster
                .facts
                .iter()
                .map(|f| FactKey {
                    subject_id: f.subject_id.clone(),
                    predicate: f.predicate.clone(),
                    object: f.object.clone(),
                })
                .collect();
            let audit = distill_audit(
                &actor_id,
                &cluster,
                &provenance,
                "written",
                Some(&retention),
                Some(&id),
                namespace,
                now,
            );
            let note = Note {
                identity: derived_identity(id, namespace, now),
                stats: distilled_stats(&cluster, now),
                content: output.content,
                context: output.context,
                keywords: output.keywords,
                embedding: Some(embedding),
                embedder_model: Some(model.clone()),
                derived_from_episode: None,
            };
            written.push(DistilledNoteWrite {
                note: MaterializedNote { note, source_facts },
                audit,
            });
            report.notes_written += 1;
        }

        store.materialize_distilled_notes(&written, &declined, now)?;
        Ok(report)
    }

    /// Every current-support fact in this namespace, deduped by id. Foreign-namespace facts are
    /// dropped: subject ids are namespace-scoped, so a distilled note never crosses namespaces.
    fn current_support_facts(
        &self,
        store: &Store,
        namespace: &Namespace,
    ) -> Result<Vec<Fact>, DistillError> {
        let members = store.candidate_state_members(CandidateSet::CurrentSupportFacts)?;
        let mut facts = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for node in members {
            let Some(fact) = store.fact_by_node_id(node)? else {
                continue;
            };
            if fact.identity.namespace != *namespace {
                continue;
            }
            if seen.insert(fact.identity.id.as_str().to_string()) {
                facts.push(fact);
            }
        }
        Ok(facts)
    }

    /// Name every entity the facts reference (subjects and entity-typed objects) from the
    /// committed `Entity.id` index, falling back to the id string when an entity is absent — so
    /// `name_of` is complete and the detail-retention guard checks against the exact names.
    fn name_entities(
        &self,
        store: &Store,
        facts: &[Fact],
    ) -> Result<BTreeMap<Id, String>, DistillError> {
        let mut needed: Vec<Id> = Vec::new();
        for fact in facts {
            needed.push(fact.subject_id.clone());
            if let ObjectValue::Entity(id) = &fact.object {
                needed.push(id.clone());
            }
        }
        needed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        needed.dedup();
        let mut name_of: BTreeMap<Id, String> = BTreeMap::new();
        for id in needed {
            let name = match store.entity_by_id(&id)? {
                Some(entity) => entity.canonical_name,
                None => id.as_str().to_string(),
            };
            name_of.insert(id, name);
        }
        Ok(name_of)
    }

    /// Embed the note bodies in one batch, mapping an embedder failure to [`DistillError::Embed`].
    async fn embed(&self, inputs: Vec<String>) -> Result<Vec<Embedding>, DistillError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.embedder
            .embed(&inputs)
            .await
            .map_err(|error| DistillError::Embed(error.to_string()))
    }
}

/// A cluster that cleared the guard, awaiting its note body's embedding before materialization.
struct Pending {
    cluster: SummarizationCluster,
    output: SummaryOutput,
    id: Id,
    /// The guard outcome that admitted it, recorded in the written-note audit for parity with the
    /// rejected-call audits (the metrics that show *why* the note passed).
    retention: RetentionOutcome,
}

/// The identity block for a distilled note: a fresh transaction time, the namespace, and the
/// content-addressed id.
fn derived_identity(id: Id, namespace: &Namespace, now: &Timestamp) -> Identity {
    Identity {
        id,
        ingested_at: now.clone(),
        namespace: namespace.clone(),
        expired_at: None,
    }
}

/// A distilled note's stats: importance and trust inherited as the mean of its source facts (a
/// note is only as trusted as the facts it rolls up), accessed now, never pinned.
fn distilled_stats(cluster: &SummarizationCluster, now: &Timestamp) -> Stats {
    let n = cluster.facts.len().max(1) as f64;
    let importance = cluster
        .facts
        .iter()
        .map(|f| f.stats.importance)
        .sum::<f64>()
        / n;
    let trust = cluster.facts.iter().map(|f| f.stats.trust).sum::<f64>() / n;
    Stats {
        importance,
        trust,
        last_access: now.clone(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.0,
        is_pinned: false,
    }
}
