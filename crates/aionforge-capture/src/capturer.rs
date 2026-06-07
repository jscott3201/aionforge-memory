//! The fast capture path (04 §1).
//!
//! [`Capturer`] runs the millisecond-time ADD path: filter, hash-dedup, embed,
//! near-duplicate check, provenance, and a single-funnel commit. It is generic over
//! the [`PrivacyFilter`] and [`Embedder`] domain seams, so it names neither the
//! concrete security filter nor the HTTP embedder — only the contracts.
//!
//! Failure shape (04 §1, §8.1): a filter or store failure aborts the capture (fail
//! closed); an embedder failure does not — the episode is written without a vector
//! and embedded later by consolidation, recorded as [`EmbeddingOutcome::Skipped`].

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{Capture, Embedder, PrivacyFilter};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Origin};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, ProvenanceRecord};
use aionforge_store::Store;

use crate::config::CaptureConfig;
use crate::error::CaptureError;
use crate::receipt::{CaptureReceipt, CaptureVerdict, EmbeddingOutcome};
use crate::request::CaptureRequest;

/// The default importance assigned at capture; consolidation recomputes it (04 §2).
const CAPTURE_IMPORTANCE: f64 = 0.5;

/// How many nearest neighbors the near-duplicate check scans before giving up. A
/// small window so it can skip a few soft-forgotten episodes and still find the
/// nearest active one without scanning deeply on the hot path (04 §1 step 2).
const NEAR_DUPLICATE_CANDIDATES: usize = 8;

/// The fast capture path over a shared [`Store`], a privacy filter, and an embedder.
#[derive(Debug, Clone)]
pub struct Capturer<F, E> {
    store: Arc<Store>,
    filter: F,
    embedder: E,
    config: CaptureConfig,
}

impl<F, E> Capturer<F, E>
where
    F: PrivacyFilter,
    E: Embedder,
{
    /// Build a capturer over a shared store, a privacy filter, and an embedder.
    #[must_use]
    pub fn new(store: Arc<Store>, filter: F, embedder: E, config: CaptureConfig) -> Self {
        Self {
            store,
            filter,
            embedder,
            config,
        }
    }

    /// Run the capture path for one request.
    async fn run(&self, request: CaptureRequest) -> Result<CaptureReceipt, CaptureError> {
        // 1. Privacy and injection filtering. Fail closed: if the filter errors we do
        //    not fall back to writing the raw content.
        let outcome = self
            .filter
            .filter(&request.content)
            .map_err(CaptureError::filter)?;

        // 2. Deduplication, exact half: the hash is over the *cleaned* content, so the
        //    redacted form is the dedup key and the embedder never sees secrets.
        let content_hash = ContentHash::of(outcome.cleaned.as_bytes());
        let namespace = enforce_namespace(&request);

        if let Some(existing) = self.store.episode_id_by_content_hash(&content_hash)? {
            return Ok(CaptureReceipt {
                episode_id: existing,
                verdict: CaptureVerdict::ExactDuplicate,
                audit_id: None,
                namespace,
                redactions: outcome.redactions,
                injection_flags: outcome.injection_flags,
                embedding: EmbeddingOutcome::NotRequested,
            });
        }

        // 4. Embedding. Degradable: a failure leaves the episode vector-less for
        //    consolidation to embed later, never blocking capture (§8.1).
        let (embedding, embedding_outcome) = self.embed(&outcome.cleaned).await;

        // 2. Deduplication, near half. Without a vector we cannot judge similarity, so
        //    the verdict is `New`. Episodes are immutable, so a near-duplicate is still
        //    written — the similarity is only surfaced for consolidation.
        let verdict = self.near_duplicate_verdict(embedding.as_ref())?;

        let trust = request.writer.trust.clamp(0.0, 1.0);
        let embedder_model = embedding.as_ref().map(|_| self.embedder.model().clone());
        let episode_id = Id::generate();

        let episode = Episode {
            identity: Identity {
                id: episode_id.clone(),
                ingested_at: request.captured_at.clone(),
                namespace: namespace.clone(),
                expired_at: None,
            },
            stats: Stats {
                importance: CAPTURE_IMPORTANCE,
                trust,
                last_access: request.captured_at.clone(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: outcome.cleaned,
            role: request.role,
            captured_at: request.captured_at.clone(),
            agent_id: request.agent_id.clone(),
            session_id: request.session_id.clone(),
            content_hash,
            embedding,
            embedder_model,
            consolidation_state: ConsolidationState::Raw,
            origin: Some(Origin {
                model_family: Some(request.writer.model_family.clone()),
                model_version: request.writer.model_version.clone(),
                transport: request.writer.transport.clone(),
                request_id: request.writer.request_id.clone(),
                redactions: outcome.redactions.clone(),
                injection_flags: outcome.injection_flags.clone(),
                // End-to-end capture latency is a surface-level SLA metric (04 §3); it
                // cannot be measured from inside the record being committed.
                capture_latency_ms: None,
            }),
        };

        // 5. Provenance. Unsigned in non-signed deployments (the empty signature);
        //    signed-write deployments fill this in (04 §1).
        let provenance = ProvenanceRecord {
            identity: Identity {
                id: Id::generate(),
                ingested_at: request.captured_at.clone(),
                namespace,
                expired_at: None,
            },
            subject_id: episode_id.clone(),
            writer_agent_id: request.agent_id.clone(),
            signature: String::new(),
            source_episode_ids: Vec::new(),
            model_family: request.writer.model_family,
            model_version: request.writer.model_version,
            trust_at_write: trust,
        };

        // 6. The capture audit event lives in the system namespace (02 §11).
        let audit = AuditEvent {
            identity: Identity {
                id: Id::generate(),
                ingested_at: request.captured_at.clone(),
                namespace: Namespace::System,
                expired_at: None,
            },
            kind: AuditKind::Capture,
            subject_id: episode_id.clone(),
            actor_id: request.agent_id,
            payload: serde_json::json!({
                "dedup": verdict_tag(&verdict),
                "redactions": outcome.redactions.len(),
                "injection_flags": outcome.injection_flags.clone(),
            }),
            signature: String::new(),
            occurred_at: request.captured_at,
        };
        let audit_id = audit.identity.id.clone();

        // Write the episode, its provenance, and the audit event as one commit.
        self.store.commit_capture(&episode, &provenance, &audit)?;

        Ok(CaptureReceipt {
            episode_id,
            verdict,
            audit_id: Some(audit_id),
            namespace: episode.identity.namespace,
            redactions: outcome.redactions,
            injection_flags: outcome.injection_flags,
            embedding: embedding_outcome,
        })
    }

    /// Embed the cleaned content, degrading to a recorded skip on any failure.
    async fn embed(&self, cleaned: &str) -> (Option<Embedding>, EmbeddingOutcome) {
        if !self.config.embed_on_capture {
            return (None, EmbeddingOutcome::NotRequested);
        }
        let inputs = [cleaned.to_string()];
        match self.embedder.embed(&inputs).await {
            Ok(vectors) => match vectors.into_iter().next() {
                Some(vector) => (Some(vector), EmbeddingOutcome::Embedded),
                None => (
                    None,
                    EmbeddingOutcome::Skipped("the embedder returned no vector".to_string()),
                ),
            },
            Err(error) => (None, EmbeddingOutcome::Skipped(error.to_string())),
        }
    }

    /// The near-duplicate verdict for a freshly embedded episode (04 §1 step 2).
    fn near_duplicate_verdict(
        &self,
        embedding: Option<&Embedding>,
    ) -> Result<CaptureVerdict, CaptureError> {
        let Some(embedding) = embedding else {
            return Ok(CaptureVerdict::New);
        };
        // The store returns the nearest *active* episode and its cosine distance
        // (smaller is more similar). The threshold is a similarity, so the boundary
        // distance is `1 - threshold`.
        let max_distance = 1.0 - self.config.near_duplicate_threshold;
        match self
            .store
            .nearest_active_episode(embedding, NEAR_DUPLICATE_CANDIDATES)?
        {
            Some((similar_to, distance)) if distance <= max_distance => {
                Ok(CaptureVerdict::NearDuplicate {
                    similar_to,
                    distance,
                })
            }
            _ => Ok(CaptureVerdict::New),
        }
    }
}

impl<F, E> Capture for Capturer<F, E>
where
    F: PrivacyFilter,
    E: Embedder,
{
    type Request = CaptureRequest;
    type Receipt = CaptureReceipt;
    type Error = CaptureError;

    fn capture(
        &self,
        request: Self::Request,
    ) -> impl Future<Output = Result<Self::Receipt, Self::Error>> + Send {
        self.run(request)
    }
}

/// Resolve the namespace a write lands in, enforcing the untrusted-write rule: an
/// untrusted write is always placed in the writer's private agent namespace,
/// regardless of what it requested (04 §1, 07).
fn enforce_namespace(request: &CaptureRequest) -> Namespace {
    let private = Namespace::Agent(request.agent_id.as_str().to_string());
    if request.trusted {
        request.namespace.clone().unwrap_or(private)
    } else {
        private
    }
}

/// The dedup verdict's stable tag for the capture audit payload.
fn verdict_tag(verdict: &CaptureVerdict) -> &'static str {
    match verdict {
        CaptureVerdict::New => "new",
        CaptureVerdict::ExactDuplicate => "exact_duplicate",
        CaptureVerdict::NearDuplicate { .. } => "near_duplicate",
    }
}
