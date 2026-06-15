//! The fact-extraction consolidation pass (write-and-consolidation §2, M2.T04).
//!
//! This is the first rule that derives memory: it runs the injected [`FactExtractor`]
//! over an episode, resolves every subject and entity-object surface to a canonical
//! entity (see [`crate::resolve`]), and assembles the [`PassOutput`] the scheduler
//! materializes atomically with the flip. It reads only a snapshot — every write is the
//! scheduler's — so an interrupted pass leaves the episode `raw` to be re-run, and the
//! content-derived fact and entity ids make that re-run a no-op.
//!
//! Each derived fact carries its extraction provenance (extractor identity, source
//! spans, rule version) and its support edges; each resolution decision is recorded as a
//! `canonicalize` audit event. The pass embeds entity names and fact statements through
//! the injected [`Embedder`] so the derived nodes are immediately retrievable.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{
    Embedder, EntitySurface, ExtractedObject, FactExtractor, SummarizationCluster, Summarizer,
    SummaryOutput,
};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::{Episode, Role};
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::nodes::semantic::{Entity, Extraction, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{CandidateSet, FactKey, MaterializedFact, MaterializedNote};

use crate::config::{DetectionConfig, PassConfig, SummarizationConfig};
use crate::detect::{CurrentFact, detect};
use crate::pass::{ConsolidationPass, PassContext, PassError, PassOutput, PassRun};
use crate::profile::{
    PassProfile, STAGE_DETECTION, STAGE_RESOLUTION, STAGE_SUMMARIZATION, StageProfile,
};
use crate::resolve::{CorefTable, Resolution, resolve_surface};
use crate::summarize::{build_clusters, check_detail_retention, note_id};

/// The fact-extraction pass: extract triples, resolve entities, derive facts, then detect
/// supersession/contradiction and summarize the touched subjects' facts into notes.
///
/// Generic over the [`FactExtractor`] (a deterministic rule extractor in M2, the
/// model-backed client in M4), the [`Embedder`] (the real client in production, a fake in
/// tests), and the [`Summarizer`] (the deterministic rule summarizer in M2, model-backed in
/// M4); all three are shared so one instance backs the whole consolidator.
pub struct FactExtractionPass<X, E, S> {
    extractor: Arc<X>,
    embedder: Arc<E>,
    summarizer: Arc<S>,
    resolution: crate::config::ResolutionConfig,
    detection: DetectionConfig,
    summarization: SummarizationConfig,
    /// The substrate actor id stamped on this pass's audit events.
    actor_id: Id,
}

impl<X, E, S> FactExtractionPass<X, E, S>
where
    X: FactExtractor + 'static,
    E: Embedder + 'static,
    S: Summarizer + 'static,
{
    /// Build the pass over a shared extractor, embedder, and summarizer.
    #[must_use]
    pub fn new(
        extractor: Arc<X>,
        embedder: Arc<E>,
        summarizer: Arc<S>,
        config: PassConfig,
    ) -> Self {
        // A stable, content-derived actor id over the pass configuration, so the pass's audit
        // events attribute to the same actor across process restarts (see [`crate::audit`]).
        let actor_id = crate::audit::actor_id(
            extractor.identity(),
            embedder.model(),
            summarizer.identity(),
        );
        Self {
            extractor,
            embedder,
            summarizer,
            resolution: config.resolution,
            detection: config.detection,
            summarization: config.summarization,
            actor_id,
        }
    }

    /// Embed a batch of strings, mapping the (possibly fatal) embedder error to a
    /// transient pass failure — a down embedder is retryable, not a bad episode.
    async fn embed(&self, inputs: Vec<String>) -> Result<Vec<Embedding>, PassError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.embedder
            .embed(&inputs)
            .await
            .map_err(|error| PassError::Transient(format!("embedder failed: {error}")))
    }

    /// Summarize the subjects this episode touched: cluster each touched subject's facts
    /// (the just-extracted ones plus the committed current support), condense via the
    /// summarizer, gate on the detail-retention guard, and emit a note per surviving
    /// cluster with `DERIVED_FROM` lineage to its source facts. Returns the notes, the
    /// `summarize` audit events (one per cluster, written-or-skipped), and the content-free
    /// stage counts for the verbose profile.
    async fn summarize_subjects(
        &self,
        store: &aionforge_store::Store,
        new_facts: &[MaterializedFact],
        resolutions: &HashMap<(String, String), Resolution>,
        namespace: &Namespace,
        episode: &Episode,
        now: &Timestamp,
    ) -> Result<SummarizationRun, PassError> {
        if !self.summarization.enabled || new_facts.is_empty() {
            return Ok(SummarizationRun::default());
        }

        // Subjects this episode touched, and every fact about them: the just-extracted ones
        // plus the committed current-support facts (so a note rolls up accumulated, not just
        // this-episode, knowledge). Dedup by fact id — a re-extracted fact equals its
        // committed self, so a replay clusters the same set.
        let touched: HashSet<String> = new_facts
            .iter()
            .map(|m| m.fact.subject_id.to_string())
            .collect();
        let mut facts: Vec<Fact> = new_facts.iter().map(|m| m.fact.clone()).collect();
        let mut seen: HashSet<String> = facts.iter().map(|f| f.identity.id.to_string()).collect();
        let members = store
            .candidate_state_members(CandidateSet::CurrentSupportFacts)
            .map_err(|error| {
                PassError::Transient(format!("current-support read failed: {error}"))
            })?;
        for node in members {
            let Some(fact) = store
                .fact_by_node_id(node)
                .map_err(|error| PassError::Transient(format!("fact read failed: {error}")))?
            else {
                continue;
            };
            // Stay within the episode's namespace. Subject ids are already namespace-scoped
            // (`resolve::new_entity_id` folds the namespace into the hash), so a foreign-
            // namespace fact can never match `touched` — but this explicit guard makes that
            // isolation a property of the loop rather than an emergent one, and drops foreign
            // facts before the subject probe (02 §11, 06 §1, 07 §9).
            if fact.identity.namespace != *namespace {
                continue;
            }
            if touched.contains(&fact.subject_id.to_string())
                && seen.insert(fact.identity.id.to_string())
            {
                facts.push(fact);
            }
        }

        let name_of = self.name_entities(store, &facts, resolutions)?;
        let clusters = build_clusters(&facts, &name_of, &self.summarization);
        // The clusters considered are the summarization stage's candidates; track how many
        // the detail-retention guard turns away so the verbose profile can explain a
        // "considered clusters, wrote no note" outcome.
        let candidates_considered = clusters.len() as u64;
        let mut rejected_by_guard: u64 = 0;
        if clusters.is_empty() {
            return Ok(SummarizationRun::default());
        }

        let rule_version = self.summarizer.identity().rule_version.clone();
        let model = self.embedder.model().clone();
        let mut audits = Vec::new();
        let mut pending: Vec<(SummarizationCluster, SummaryOutput, Id)> = Vec::new();
        let mut contents: Vec<String> = Vec::new();
        for cluster in clusters {
            let Some(output) = self
                .summarizer
                .summarize(&cluster)
                .await
                .map_err(|error| PassError::Transient(format!("summarizer failed: {error}")))?
            else {
                continue; // the summarizer conservatively declined this cluster
            };
            let retention = check_detail_retention(&cluster, &output, &self.summarization);
            audits.push(crate::audit::summarize_audit(
                &self.actor_id,
                &episode.identity.id,
                &cluster,
                &rule_version,
                namespace,
                now,
                &retention,
            ));
            if !retention.passed {
                rejected_by_guard += 1;
                continue; // over-summarized — skip the note, keep the raw facts
            }
            let id = note_id(namespace, &cluster, &rule_version);
            contents.push(output.content.clone());
            pending.push((cluster, output, id));
        }

        // Embed note bodies in one batch so the notes are immediately vector-searchable.
        let embeddings = self.embed(contents).await?;
        let mut notes = Vec::with_capacity(pending.len());
        for ((cluster, output, id), embedding) in pending.into_iter().zip(embeddings) {
            let source_facts = cluster
                .facts
                .iter()
                .map(|f| FactKey {
                    subject_id: f.subject_id,
                    predicate: f.predicate.clone(),
                    object: f.object.clone(),
                })
                .collect();
            notes.push(MaterializedNote {
                note: Note {
                    identity: identity(id, namespace, now),
                    stats: derived_stats(episode, now),
                    content: output.content,
                    context: output.context,
                    keywords: output.keywords,
                    embedding: Some(embedding),
                    embedder_model: Some(model.clone()),
                    derived_from_episode: Some(episode.identity.id),
                },
                source_facts,
            });
        }
        Ok(SummarizationRun {
            notes,
            audits,
            candidates_considered,
            rejected_by_guard,
        })
    }

    /// Name every entity the cluster facts reference (subjects and entity-typed objects):
    /// from this episode's resolutions first, then the committed `Entity.id` index.
    fn name_entities(
        &self,
        store: &aionforge_store::Store,
        facts: &[Fact],
        resolutions: &HashMap<(String, String), Resolution>,
    ) -> Result<BTreeMap<Id, String>, PassError> {
        let mut name_of: BTreeMap<Id, String> = BTreeMap::new();
        for resolution in resolutions.values() {
            name_of
                .entry(resolution.id)
                .or_insert_with(|| resolution.canonical_name.clone());
        }
        let mut needed: Vec<Id> = Vec::new();
        for fact in facts {
            if !name_of.contains_key(&fact.subject_id) {
                needed.push(fact.subject_id);
            }
            if let ObjectValue::Entity(id) = &fact.object
                && !name_of.contains_key(id)
            {
                needed.push(*id);
            }
        }
        needed.sort();
        needed.dedup();
        for id in needed {
            // Resolve the name from the committed index, falling back to the id string when
            // the entity is absent. The fallback is made explicit here — not deferred to a
            // reader — so `name_of` is complete and every cluster sees the exact entity names
            // the detail-retention guard will check the summary against.
            let name = match store
                .entity_by_id(&id)
                .map_err(|error| PassError::Transient(format!("entity read failed: {error}")))?
            {
                Some(entity) => entity.canonical_name,
                None => id.to_string(),
            };
            name_of.insert(id, name);
        }
        Ok(name_of)
    }
}

#[async_trait::async_trait]
impl<X, E, S> ConsolidationPass for FactExtractionPass<X, E, S>
where
    X: FactExtractor + 'static,
    E: Embedder + 'static,
    S: Summarizer + 'static,
{
    fn name(&self) -> &'static str {
        "extract_facts"
    }

    fn version(&self) -> u32 {
        // 3: M2.T06b added conservative summary-note output to this pass.
        // 4: M6.T02 skips system-role episodes so a system directive cannot launder
        //    into a role-less fact (the gate is version-keyed for replay determinism).
        4
    }

    async fn apply(&self, cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        let episode = cx.episode;

        // System-role episodes never produce facts (07 §4, M6.T02). A Fact carries no
        // role and inherits the episode's namespace, so a fact extracted from a system
        // directive would be excluded by neither the recall-side role gate (facts have no
        // role) nor the namespace gate (if the episode sat in a visible namespace) — it
        // would launder the directive into default recall. Mirrors the skill-induction
        // role gate. A capture-path system-role write is already refused (M6.T02 PR-2), and
        // a substrate-internal system episode that has not yet been consolidated is skipped
        // here too. This gate prevents NEW extraction only: facts already extracted from a
        // system-role episode on a pre-gate build are not retracted — a one-time backfill
        // that quarantines them is an owner gap (07 §4).
        if episode.role == Role::System {
            return Ok(PassRun::unprofiled(PassOutput::default()));
        }

        let extracted = self
            .extractor
            .extract(episode)
            .await
            .map_err(|error| PassError::Transient(format!("extractor failed: {error}")))?;
        if extracted.is_empty() {
            // Nothing extracted: every stage ran (gates aside) but had no candidate. Report
            // each enabled stage with zero counts so a verbose receipt distinguishes "ran,
            // saw nothing" from a disabled stage.
            return Ok(PassRun {
                output: PassOutput::default(),
                profile: self.empty_profile(),
            });
        }

        // Embed each distinct surface TEXT once. The embedder is a pure function of the
        // input string — an entity's type never reaches it — so two surfaces with the
        // same text but different types share one embedding, and keying by text is exact.
        // The resolution-time type filter (resolve.rs) is what keeps those two apart.
        let surfaces = distinct_surfaces(&extracted);
        let mut distinct_texts: Vec<String> = Vec::new();
        let mut seen_text: HashSet<&str> = HashSet::new();
        for surface in &surfaces {
            if seen_text.insert(surface.surface.as_str()) {
                distinct_texts.push(surface.surface.clone());
            }
        }
        let text_embeddings = self.embed(distinct_texts.clone()).await?;
        let embedding_of: HashMap<&str, &Embedding> = distinct_texts
            .iter()
            .map(String::as_str)
            .zip(text_embeddings.iter())
            .collect();

        // Resolve each surface (read-only) within the episode's namespace.
        let namespace = &episode.identity.namespace;
        let store: &aionforge_store::Store = cx.store;
        let mut coref = CorefTable::default();
        let mut resolutions: HashMap<(String, String), Resolution> = HashMap::new();
        let mut audit_events = Vec::new();
        // The resolution stage's candidates are the distinct surfaces; a surface that matched
        // a committed entity is `merged`, a fresh one feeds the `derived` new-entity count.
        let resolution_candidates = surfaces.len() as u64;
        let mut resolution_merged: u64 = 0;
        for surface in &surfaces {
            let embedding = embedding_of
                .get(surface.surface.as_str())
                .ok_or_else(|| PassError::Fatal("surface embedding missing".to_string()))?;
            let resolution = resolve_surface(
                store,
                &self.resolution,
                namespace,
                surface,
                embedding,
                &mut coref,
            )
            .map_err(|error| PassError::Transient(format!("entity resolution failed: {error}")))?;
            if !resolution.is_new {
                resolution_merged += 1;
            }
            audit_events.push(crate::audit::canonicalize_audit(
                &self.actor_id,
                &episode.identity.id,
                surface,
                &resolution,
                namespace,
                &cx.now,
            ));
            resolutions.insert(surface_key(surface), resolution);
        }

        // Build the new entities the resolver discovered, embedded by canonical name.
        let model = self.embedder.model().clone();
        let mut new_entities = Vec::new();
        for (id, canonical_name, entity_type, aliases) in coref.new_entities() {
            let embedding = embedding_of
                .get(canonical_name.as_str())
                .map(|e| (*e).clone());
            new_entities.push(Entity {
                identity: identity(id, namespace, &cx.now),
                stats: derived_stats(episode, &cx.now),
                canonical_name,
                entity_type,
                aliases,
                description: None,
                embedding,
                embedder_model: Some(model.clone()),
                attributes: None,
            });
        }

        // Build the facts, resolving subject and entity objects to canonical ids.
        let rule_version = self.extractor.identity().rule_version.clone();
        let identity_ref = self.extractor.identity();
        let mut facts = Vec::new();
        let mut statements = Vec::new();
        let mut mentioned: Vec<Id> = Vec::new();
        let mut mentioned_seen: HashSet<String> = HashSet::new();
        for extracted_fact in &extracted {
            let Some(subject) = resolutions.get(&surface_key(&extracted_fact.subject)) else {
                continue;
            };
            note_mention(&mut mentioned, &mut mentioned_seen, &subject.id);
            let object = match &extracted_fact.object {
                ExtractedObject::Entity(object_surface) => {
                    let Some(resolved) = resolutions.get(&surface_key(object_surface)) else {
                        continue;
                    };
                    note_mention(&mut mentioned, &mut mentioned_seen, &resolved.id);
                    ObjectValue::Entity(resolved.id)
                }
                ExtractedObject::Literal(value) => value.clone(),
            };

            let fact_id = fact_id(
                namespace,
                &subject.id,
                &extracted_fact.predicate,
                &object,
                &episode.identity.id,
                &rule_version,
            );
            let extraction = Extraction {
                extractor_model_family: identity_ref.model_family.clone(),
                extractor_model_version: identity_ref.model_version.clone(),
                source_spans: extracted_fact.source_spans.clone(),
                extraction_rule_version: Some(rule_version.clone()),
            };
            statements.push(extracted_fact.statement.clone());
            facts.push(MaterializedFact {
                fact: Fact {
                    identity: identity(fact_id, namespace, &cx.now),
                    stats: derived_stats(episode, &cx.now),
                    subject_id: subject.id,
                    predicate: extracted_fact.predicate.clone(),
                    object,
                    confidence: extracted_fact.confidence,
                    status: FactStatus::Active,
                    statement: extracted_fact.statement.clone(),
                    embedding: None,
                    embedder_model: Some(model.clone()),
                    extraction: Some(extraction),
                    cooled_until: None,
                },
                about: about_window(episode, &cx.now),
            });
        }

        // Embed fact statements so the derived facts are immediately vector-searchable.
        let statement_embeddings = self.embed(statements).await?;
        for (materialized, embedding) in facts.iter_mut().zip(statement_embeddings) {
            materialized.fact.embedding = Some(embedding);
        }

        // Detect supersession/contradiction of the new facts against the committed current
        // set (read-only); the store materializes the resulting edges in the flip txn. The
        // detection stage's candidates are the new facts it checks (only when enabled).
        let detection_candidates = if self.detection.enabled {
            facts.len() as u64
        } else {
            0
        };
        let detection = self.detect_conflicts(store, &facts, namespace, episode, &cx.now)?;
        let quarantined = detection
            .contradictions
            .iter()
            .filter(|c| c.quarantine_source)
            .count() as u64;

        // The canonicalize decisions (one per resolved surface) plus the quarantine
        // reconcile signals from detection.
        audit_events.extend(detection.audits);

        // Summarize the touched subjects' facts (committed current plus the just-extracted
        // ones) into conservative notes; the detail-retention guard skips any summary that
        // would drop too much specificity. Raw facts are untouched (non-lossy).
        let summarization = self
            .summarize_subjects(store, &facts, &resolutions, namespace, episode, &cx.now)
            .await?;

        // Build the content-free per-stage profile before the artifacts move into `out`.
        // Resolution always ran (it has no config gate); detection/summarization carry their
        // config gate so the receipt can tell a disabled stage from one that saw no input.
        let profile = PassProfile::from_stages(vec![
            StageProfile::enabled(
                STAGE_RESOLUTION,
                resolution_candidates,
                new_entities.len() as u64,
                resolution_merged,
                0,
                0,
            ),
            self.detection_stage(
                detection_candidates,
                detection.supersessions.len() as u64,
                quarantined,
            ),
            self.summarization_stage(&summarization),
        ]);

        audit_events.extend(summarization.audits);
        let mut out = PassOutput::default();
        out.new_entities = new_entities;
        out.facts = facts;
        out.mentioned_entities = mentioned;
        out.supersessions = detection.supersessions;
        out.contradictions = detection.contradictions;
        out.notes = summarization.notes;
        out.audit_events = audit_events;
        emit_detection_metrics(&out);
        emit_summarization_metrics(&out);
        Ok(PassRun {
            output: out,
            profile,
        })
    }
}

impl<X, E, S> FactExtractionPass<X, E, S> {
    /// The all-zero stage profile for an episode that extracted nothing: resolution always
    /// ran (it has no config gate), and detection/summarization are reported enabled-or-not
    /// per their config so the receipt can tell a disabled stage from one that saw no input.
    fn empty_profile(&self) -> PassProfile {
        PassProfile::from_stages(vec![
            StageProfile::enabled(STAGE_RESOLUTION, 0, 0, 0, 0, 0),
            self.detection_stage(0, 0, 0),
            self.summarization_stage(&SummarizationRun::default()),
        ])
    }

    /// The detection stage profile, gated on [`DetectionConfig::enabled`].
    fn detection_stage(&self, candidates: u64, derived: u64, quarantined: u64) -> StageProfile {
        if self.detection.enabled {
            StageProfile::enabled(STAGE_DETECTION, candidates, derived, 0, quarantined, 0)
        } else {
            StageProfile::disabled(STAGE_DETECTION)
        }
    }

    /// The summarization stage profile, gated on [`SummarizationConfig::enabled`].
    fn summarization_stage(&self, run: &SummarizationRun) -> StageProfile {
        if self.summarization.enabled {
            StageProfile::enabled(
                STAGE_SUMMARIZATION,
                run.candidates_considered,
                run.notes.len() as u64,
                0,
                0,
                run.rejected_by_guard,
            )
        } else {
            StageProfile::disabled(STAGE_SUMMARIZATION)
        }
    }

    /// Detect supersession/contradiction of the new facts against the committed current
    /// set (read-only). Reads `current_support_facts`, scopes it to the `(subject,
    /// predicate)` pairs the new facts touch, then runs the pure [`detect`] decision.
    fn detect_conflicts(
        &self,
        store: &aionforge_store::Store,
        facts: &[MaterializedFact],
        namespace: &Namespace,
        episode: &Episode,
        now: &Timestamp,
    ) -> Result<crate::detect::DetectionOutput, PassError> {
        if !self.detection.enabled || facts.is_empty() {
            return Ok(crate::detect::DetectionOutput::default());
        }
        let touched: HashSet<(String, String)> = facts
            .iter()
            .map(|m| (m.fact.subject_id.to_string(), m.fact.predicate.clone()))
            .collect();
        // The episode's writer-asserted supersedes hint (04 §1 step 3): resolve the hinted
        // episode to the fact ids it SUPPORTS, so those incumbents become supersession-
        // eligible even off the functional registry. The hint acts only through fact
        // overlap — a hinted fact whose (subject, predicate) the new facts never touch is
        // untouched, and a hint that resolves to no facts is a recorded no-op. The
        // namespace guard below still applies: a hinted episode in another namespace
        // (a trusted cross-namespace capture) contributes nothing here.
        let hinted: HashSet<Id> = match episode.origin.as_ref().and_then(|o| o.supersedes) {
            None => HashSet::new(),
            Some(target) => store
                .fact_ids_supported_by_episode(&target)
                .map_err(|error| {
                    PassError::Transient(format!("hinted-episode fact read failed: {error}"))
                })?
                .into_iter()
                .collect(),
        };
        let members = store
            .candidate_state_members(CandidateSet::CurrentSupportFacts)
            .map_err(|error| {
                PassError::Transient(format!("current-support read failed: {error}"))
            })?;
        let mut current = Vec::new();
        for node in members {
            let Some(fact) = store
                .fact_by_node_id(node)
                .map_err(|error| PassError::Transient(format!("fact read failed: {error}")))?
            else {
                continue;
            };
            // Detection only ever compares facts within the episode's namespace, so a
            // supersession/contradiction edge can never bridge namespaces. Subject ids are
            // namespace-scoped (`resolve::new_entity_id`), so a foreign fact cannot match
            // `touched`; this explicit guard makes that boundary local rather than emergent
            // (02 §11, 06 §1, 07 §9).
            if fact.identity.namespace != *namespace {
                continue;
            }
            if !touched.contains(&(fact.subject_id.to_string(), fact.predicate.clone())) {
                continue;
            }
            let Some(about) = store.fact_about(node).map_err(|error| {
                PassError::Transient(format!("fact window read failed: {error}"))
            })?
            else {
                continue;
            };
            current.push(CurrentFact {
                id: fact.identity.id,
                hint_eligible: hinted.contains(&fact.identity.id),
                key: FactKey {
                    subject_id: fact.subject_id,
                    predicate: fact.predicate,
                    object: fact.object,
                },
                valid_from: about.temporal.valid_from,
                trust: fact.stats.trust,
            });
        }
        Ok(detect(
            &current,
            facts,
            &self.detection,
            namespace,
            &episode.captured_at,
            now,
            &self.actor_id,
        ))
    }
}

/// The summarization stage's output plus its content-free counts for the verbose profile.
///
/// `candidates_considered` is the number of fact clusters examined; `rejected_by_guard` is
/// how many the detail-retention guard turned away (the `!retention.passed` arm), which is
/// what lets a verbose receipt explain a "clusters considered, no note written" outcome.
#[derive(Default)]
struct SummarizationRun {
    notes: Vec<MaterializedNote>,
    audits: Vec<AuditEvent>,
    candidates_considered: u64,
    rejected_by_guard: u64,
}

/// Emit per-tick detection counters so supersession/quarantine rates are observable.
fn emit_detection_metrics(out: &PassOutput) {
    if !out.supersessions.is_empty() {
        metrics::counter!("consolidation_supersessions_total")
            .increment(out.supersessions.len() as u64);
    }
    if !out.contradictions.is_empty() {
        metrics::counter!("consolidation_contradictions_total")
            .increment(out.contradictions.len() as u64);
        let quarantines = out
            .contradictions
            .iter()
            .filter(|c| c.quarantine_source)
            .count() as u64;
        if quarantines > 0 {
            metrics::counter!("consolidation_quarantines_total").increment(quarantines);
        }
    }
}

/// Emit a per-tick counter of summary notes written, so the summarization rate is
/// observable (skips are surfaced as `summarize` audit events, not a counter).
fn emit_summarization_metrics(out: &PassOutput) {
    if !out.notes.is_empty() {
        metrics::counter!("consolidation_summaries_total").increment(out.notes.len() as u64);
    }
}

/// The distinct surface forms (subjects plus entity objects) in appearance order.
fn distinct_surfaces(facts: &[aionforge_domain::contracts::ExtractedFact]) -> Vec<EntitySurface> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut surfaces = Vec::new();
    let push = |surface: &EntitySurface,
                surfaces: &mut Vec<EntitySurface>,
                seen: &mut HashSet<(String, String)>| {
        if seen.insert(surface_key(surface)) {
            surfaces.push(surface.clone());
        }
    };
    for fact in facts {
        push(&fact.subject, &mut surfaces, &mut seen);
        if let ExtractedObject::Entity(object) = &fact.object {
            push(object, &mut surfaces, &mut seen);
        }
    }
    surfaces
}

/// The map key identifying a surface form: its text and provisional type.
fn surface_key(surface: &EntitySurface) -> (String, String) {
    (
        surface.surface.trim().to_string(),
        surface.entity_type.clone(),
    )
}

/// Record an entity id as mentioned by the episode, once.
fn note_mention(mentioned: &mut Vec<Id>, seen: &mut HashSet<String>, id: &Id) {
    if seen.insert(id.to_string()) {
        mentioned.push(*id);
    }
}

/// The deterministic fact id: a content hash over the canonical triple key plus source
/// episode and rule version, so re-extracting an episode yields the same id (04 §2).
fn fact_id(
    namespace: &Namespace,
    subject_id: &Id,
    predicate: &str,
    object: &ObjectValue,
    episode_id: &Id,
    rule_version: &str,
) -> Id {
    let key = format!(
        "{}|{}|{}|{}|{}|{}",
        namespace,
        subject_id,
        predicate,
        object_canonical(object),
        episode_id,
        rule_version,
    );
    Id::from_content_hash(key.as_bytes())
}

/// A stable canonical string for an object value, for the fact id key.
fn object_canonical(object: &ObjectValue) -> String {
    match object {
        ObjectValue::Entity(id) => format!("entity:{id}"),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The identity block for a derived node: a fresh transaction time, the episode's
/// namespace, and a content-derived id.
fn identity(id: Id, namespace: &Namespace, now: &Timestamp) -> Identity {
    Identity {
        id,
        ingested_at: now.clone(),
        namespace: namespace.clone(),
        expired_at: None,
    }
}

/// The stats block for a derived node: derivation trust/importance inherited from the
/// source episode, accessed now, never pinned.
fn derived_stats(episode: &Episode, now: &Timestamp) -> Stats {
    Stats {
        importance: episode.stats.importance,
        trust: episode.stats.trust,
        last_access: now.clone(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.0,
        is_pinned: false,
    }
}

/// The `ABOUT` validity window: event time opens at the episode's `captured_at`,
/// transaction time at `now`, both open-ended.
fn about_window(episode: &Episode, now: &Timestamp) -> aionforge_domain::edges::About {
    aionforge_domain::edges::About {
        temporal: BiTemporal {
            valid_from: episode.captured_at.clone(),
            valid_to: None,
            ingested_at: now.clone(),
            expired_at: None,
        },
    }
}
