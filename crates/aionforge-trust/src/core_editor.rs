//! The core-block edit gate (05 §4, M5.T04): the orchestrator that makes identity
//! "attested writes only".
//!
//! [`CoreEditor`] is the [`Promoter`](crate::promoter::Promoter)'s twin for the
//! identity tier — but a *binary* authorization, not a probabilistic one: an edit
//! either presents enough verified, independent vouchers or it does not, so there is
//! no Beta posterior and no reachability constraint. The single rule (05 §4): a
//! single-writer self-edit is rejected; an edit requires a second attester, and a
//! sensitive block can be configured to require a *human* one.
//!
//! Everything refusable is refused **before** the store write, fail-closed, and every
//! gate rejection is audited (a refused self-edit is exactly the sycophantic-drift
//! signal the threat model wants on the record, 07 §T6). The whole call is
//! one-shot and host-coordinated: the caller collects the editor's and the attesters'
//! signatures out-of-band and presents them together — the substrate holds no pending
//! edit, because a pending-edit surface is the browse-pending trust-laundering path
//! the spec forbids (06 §4). Each attester signs the canonical **core-edit**
//! attestation payload over the block's stable id *and the exact prior-to-new content
//! transition*, at an `attested_at` inside the clock-skew window — a vote authorizes
//! one specific replacement of one block, never "some edit of this block in the
//! window" (a fact's content-addressed id binds content by itself; a core block's
//! deliberately stable id cannot, so the transition rides in the signed bytes). The
//! compare-and-swap precondition then re-checks the prior under the store's write
//! lock and refuses a stale edit whole. Residual, accepted: if a block is edited back
//! to the exact prior bytes inside the skew window, an unexpired vote for that same
//! transition could re-apply — time-bounded, and content-exact, so what re-applies is
//! precisely what was vouched for.
//!
//! Humanness is a **host policy assertion**, not a substrate-verifiable fact: the
//! policy carries the agent ids the deployment certifies as human-controlled keys, the
//! same trust boundary as the caller-asserted [`Principal`] (06 §1). A human attester
//! is an ordinary enrolled agent whose vote verifies like any other; the gate
//! additionally requires it to be on the list and still active.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::Identity;
use aionforge_domain::edges::AttestedBy;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::gate::{GateError, ProvenanceGate};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::AgentStatus;
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{CoreAttestation, CoreBlockReplacement, CoreEditWrite, Store, StoreError};

use crate::attest_gate::{AttestError, AttestationGate};

/// The edit requirement one block resolves to: how many distinct non-editor attesters
/// must vouch, and whether one of them must be a certified human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreEditRule {
    /// Distinct verified attesters required, the editor never counted. At least 1 —
    /// "an edit requires a second attester" (05 §4).
    pub k: u64,
    /// Whether at least one verified attester must be on the policy's human list.
    pub require_human: bool,
}

impl Default for CoreEditRule {
    fn default() -> Self {
        Self {
            k: 1,
            require_human: false,
        }
    }
}

/// The core-block edit policy (05 §4). Always on — there is no switch that re-enables
/// single-writer edits; only the *strictness* is configurable.
#[derive(Debug, Clone, Default)]
pub struct CoreEditPolicy {
    /// The baseline requirement for every block.
    pub default_rule: CoreEditRule,
    /// Whether a `redline` block additionally requires a human attester — the spec's
    /// named sensitive class (05 §4), composable with the sensitivity rules below.
    pub redline_requires_human: bool,
    /// Per-sensitivity overrides, keyed by the block's `sensitivity` string.
    pub rules: BTreeMap<String, CoreEditRule>,
    /// The agent ids this deployment certifies as human-controlled keys. A host
    /// policy assertion (06 §1) — never a property an agent can self-declare.
    pub human_attester_ids: BTreeSet<Id>,
}

impl CoreEditPolicy {
    /// Validate the policy, fail-closed at construction time.
    ///
    /// # Errors
    /// Returns a message naming the offending knob: a zero `k` anywhere (a quorum of
    /// none would re-enable single-writer edits), or a human requirement with an empty
    /// human list (an unsatisfiable gate that bricks every sensitive edit).
    pub fn validate(&self) -> Result<(), String> {
        if self.default_rule.k == 0 {
            return Err("core_block.default_rule.k must be at least 1".to_string());
        }
        for (sensitivity, rule) in &self.rules {
            if rule.k == 0 {
                return Err(format!(
                    "core_block.rules.{sensitivity}.k must be at least 1"
                ));
            }
        }
        let any_human = self.redline_requires_human
            || self.default_rule.require_human
            || self.rules.values().any(|rule| rule.require_human);
        if any_human && self.human_attester_ids.is_empty() {
            return Err(
                "core_block: a human-attestation requirement needs a non-empty \
                 human_attester_ids list, or every sensitive edit is unsatisfiable"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Resolve the requirement for one block: the default, the block's
    /// sensitivity-keyed rule, and the implicit redline rule, composed
    /// **strictest-per-axis** (the max `k`, the OR of `require_human`) — a sensitive
    /// block is never edited under a laxer bar than any rule that applies to it.
    #[must_use]
    pub fn requirement_for(&self, kind: &BlockKind, sensitivity: Option<&str>) -> CoreEditRule {
        let mut rule = self.default_rule;
        if let Some(by_sensitivity) = sensitivity.and_then(|s| self.rules.get(s)) {
            rule.k = rule.k.max(by_sensitivity.k);
            rule.require_human |= by_sensitivity.require_human;
        }
        if *kind == BlockKind::Redline {
            rule.require_human |= self.redline_requires_human;
        }
        rule
    }
}

/// One attester's signed vote, collected by the host and presented with the edit.
#[derive(Debug, Clone)]
pub struct CoreAttesterVote {
    /// The attesting agent.
    pub attester_id: Id,
    /// When the attester signed — must sit inside the clock-skew window.
    pub attested_at: Timestamp,
    /// Base64 Ed25519 signature over the canonical core-edit attestation payload
    /// `(block_id, attester_id, prior_content_hash, new_content_hash, attested_at)` —
    /// the vote vouches for the exact transition, so it can never be replayed onto a
    /// different replacement of the same block.
    pub signature_b64: String,
    /// The trust category the vote is made under, if any.
    pub category: Option<String>,
}

/// One attested whole-value edit, host-coordinated into a single call.
#[derive(Debug, Clone)]
pub struct CoreEditRequest {
    /// The block's stable id.
    pub block_id: Id,
    /// The hash of the content this edit was prepared against — what the attesters
    /// vouched over. Re-checked atomically by the store's compare-and-swap.
    pub expected_prior: ContentHash,
    /// The replacement content, whole.
    pub content: String,
    /// A new drift baseline, or `None` to carry the existing one forward (an ordinary
    /// edit never re-baselines; that is the M5.T05 detector's privileged call).
    pub drift_baseline: Option<serde_json::Value>,
    /// The embedding of the new content with its model identity, or `None` to remove
    /// the stale vector.
    pub embedding: Option<(Embedding, EmbedderModel)>,
    /// The editor's own Ed25519 provenance signature over
    /// `(block_id, editor_id, at)` — required when the engine runs signed writes,
    /// ignored otherwise (editor identity then rests on the host-asserted principal).
    pub editor_signature: Option<String>,
    /// The attester votes.
    pub votes: Vec<CoreAttesterVote>,
    /// The edit instant, the caller's clock.
    pub at: Timestamp,
}

/// What one applied edit recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreEditReceipt {
    /// The `core_edit` audit row's domain id.
    pub audit_id: Id,
    /// Distinct attester votes recorded on the block.
    pub attesters_recorded: usize,
    /// The hash of the content this edit replaced.
    pub prior_content_hash: ContentHash,
    /// The hash of the content now live.
    pub new_content_hash: ContentHash,
}

/// Why the gate refused an edit. Every rejection is audited in the block's own
/// namespace with the principal as actor; the caller's view is deliberately coarse on
/// signature failures (the cause is for the audit, not the caller — the gate is no
/// forge oracle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEditRejection {
    /// Signed writes are on and the editor's provenance signature is absent or does
    /// not verify.
    EditorUnverified,
    /// A presented vote failed verification (skew, unknown attester, or bad
    /// signature). The whole edit is refused — a forged voucher in the set is an
    /// integrity red flag, not a vote to silently drop.
    AttestationFailed,
    /// Fewer distinct verified non-editor attesters than the block's requirement.
    /// A single-writer self-edit lands here with `verified: 0`.
    InsufficientAttesters {
        /// The resolved requirement.
        required: u64,
        /// Distinct verified non-editor attesters presented.
        verified: u64,
    },
    /// The block's requirement includes a human attester and no verified vote came
    /// from an active agent on the policy's human list.
    HumanAttestationRequired,
}

/// The outcome of one edit call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEditOutcome {
    /// The edit was applied, attested, and audited.
    Applied(CoreEditReceipt),
    /// No core block carries this id.
    NotFound,
    /// The block exists but is retired (`expired_at` set); a retired identity is not
    /// edited back to life.
    Retired,
    /// The block's content is no longer what the edit was prepared against — a
    /// concurrent edit landed first. Nothing was written; re-read and re-collect.
    StaleContent,
    /// The gate refused; the rejection is audited and nothing was written.
    Rejected(CoreEditRejection),
}

/// A failure that is neither an outcome nor a refusal: the store read/write itself, or
/// a backend fault resolving a key.
#[derive(Debug, thiserror::Error)]
pub enum CoreEditError {
    /// A store read or write failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A backend fault while resolving a key — an availability problem, not an attack.
    #[error("key resolution backend fault: {0}")]
    Backend(String),
}

/// The core-block edit orchestrator. Always constructed — identity integrity has no
/// off-switch — with the editor-provenance leg present only when signed writes are on.
pub struct CoreEditor {
    store: Arc<Store>,
    attester_gate: AttestationGate,
    editor_gate: Option<Arc<dyn ProvenanceGate>>,
    policy: CoreEditPolicy,
}

impl std::fmt::Debug for CoreEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreEditor")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl CoreEditor {
    /// Build over the store with the attestation gate and the optional
    /// editor-provenance gate (present exactly when signed writes are on). The policy
    /// is validated here, fail-closed — an invalid strictness policy (a zero `k`, a
    /// human requirement with an empty human list) never stands a gate, no matter
    /// which caller composed it.
    ///
    /// # Errors
    /// Returns the policy's validation message, naming the offending knob.
    pub fn new(
        store: Arc<Store>,
        attester_gate: AttestationGate,
        editor_gate: Option<Arc<dyn ProvenanceGate>>,
        policy: CoreEditPolicy,
    ) -> Result<Self, String> {
        policy.validate()?;
        Ok(Self {
            store,
            attester_gate,
            editor_gate,
            policy,
        })
    }

    /// The policy this editor runs.
    #[must_use]
    pub fn policy(&self) -> &CoreEditPolicy {
        &self.policy
    }

    /// Gate and apply one attested whole-value edit (05 §4). Every refusal is typed
    /// and decided before the store write; every gate rejection is audited.
    ///
    /// # Errors
    /// Returns [`CoreEditError`] if a store read/write fails or key resolution hits a
    /// backend fault. Security refusals are never errors — they are the typed
    /// [`CoreEditOutcome`] variants.
    pub fn edit(
        &self,
        principal: &Principal,
        request: &CoreEditRequest,
    ) -> Result<CoreEditOutcome, CoreEditError> {
        let Some(block) = self.store.core_block_by_id(&request.block_id)? else {
            return Ok(CoreEditOutcome::NotFound);
        };
        if block.identity.expired_at.is_some() {
            return Ok(CoreEditOutcome::Retired);
        }
        let prior_hash = ContentHash::of(block.content.as_bytes());
        if prior_hash != request.expected_prior {
            // Decided again, atomically, by the store's compare-and-swap; this early
            // answer just spares the caller the signature work on a known-stale edit.
            return Ok(CoreEditOutcome::StaleContent);
        }

        // The editor leg: when signed writes are on, the host-asserted principal must
        // also prove key possession over (block, editor, instant).
        if let Some(gate) = &self.editor_gate {
            let admitted = match &request.editor_signature {
                Some(signature) => {
                    match gate.admit(
                        &request.block_id,
                        &principal.agent_id,
                        &request.at,
                        signature,
                    ) {
                        Ok(()) => true,
                        Err(GateError::Backend(error)) => {
                            return Err(CoreEditError::Backend(error.to_string()));
                        }
                        Err(_) => false,
                    }
                }
                None => false,
            };
            if !admitted {
                return self.reject(
                    principal,
                    &block,
                    request,
                    CoreEditRejection::EditorUnverified,
                );
            }
        }

        let requirement = self
            .policy
            .requirement_for(&block.block_kind, block.sensitivity.as_deref());

        // Distinct non-editor votes — the editor is never counted toward the quorum,
        // by positive id comparison (a core-block edit has one distinguished author,
        // unlike a fact, so exclusion cannot ride on k alone).
        let mut distinct: Vec<&CoreAttesterVote> = Vec::new();
        for vote in &request.votes {
            if vote.attester_id == principal.agent_id {
                continue;
            }
            if distinct.iter().any(|v| v.attester_id == vote.attester_id) {
                continue;
            }
            distinct.push(vote);
        }

        // Verify every counted vote over the exact transition this request ships
        // (prior hash -> new-content hash); one forged or skewed voucher refuses the
        // whole edit rather than being silently dropped. A vote collected for a
        // different proposed replacement fails here — the transition is in the signed
        // bytes, not merely implied by the block id.
        let new_hash = ContentHash::of(request.content.as_bytes());
        for vote in &distinct {
            match self.attester_gate.admit_core_edit(
                &request.block_id,
                &vote.attester_id,
                &prior_hash,
                &new_hash,
                &vote.attested_at,
                &vote.signature_b64,
            ) {
                Ok(()) => {}
                Err(AttestError::Backend(error)) => return Err(CoreEditError::Backend(error)),
                Err(AttestError::Rejected(_)) => {
                    return self.reject(
                        principal,
                        &block,
                        request,
                        CoreEditRejection::AttestationFailed,
                    );
                }
            }
        }

        if (distinct.len() as u64) < requirement.k {
            return self.reject(
                principal,
                &block,
                request,
                CoreEditRejection::InsufficientAttesters {
                    required: requirement.k,
                    verified: distinct.len() as u64,
                },
            );
        }

        // The human requirement: one verified vote from an agent the deployment
        // certifies as human-controlled AND still active — the gate checks enrollment,
        // not status, so a retired human reviewer fails closed here.
        let mut human_attester_id: Option<Id> = None;
        if requirement.require_human {
            for vote in &distinct {
                if !self.policy.human_attester_ids.contains(&vote.attester_id) {
                    continue;
                }
                let active = self
                    .store
                    .agent_by_id(&vote.attester_id)?
                    .is_some_and(|agent| agent.status == AgentStatus::Active);
                if active {
                    human_attester_id = Some(vote.attester_id);
                    break;
                }
            }
            if human_attester_id.is_none() {
                return self.reject(
                    principal,
                    &block,
                    request,
                    CoreEditRejection::HumanAttestationRequired,
                );
            }
        }

        // Resolve attester nodes; a vote whose agent node vanished between the key
        // check and here is the same coarse refusal as a failed verification. The
        // credited human's node is kept aside: its `Active` status is part of the
        // verdict, so the store re-checks it under the write lock (the status twin of
        // the content precondition — the read above was its own lock acquisition).
        let mut attestations: Vec<CoreAttestation> = Vec::with_capacity(distinct.len());
        let mut required_active = None;
        for vote in &distinct {
            let Some(node) = self.store.agent_node_by_id(&vote.attester_id)? else {
                return self.reject(
                    principal,
                    &block,
                    request,
                    CoreEditRejection::AttestationFailed,
                );
            };
            if human_attester_id == Some(vote.attester_id) {
                required_active = Some(node);
            }
            attestations.push(CoreAttestation {
                attester: node,
                edge: AttestedBy {
                    attested_at: vote.attested_at.clone(),
                    signature: vote.signature_b64.clone(),
                    category: vote.category.clone(),
                },
            });
        }
        let Some(candidate) = self
            .store
            .memory_by_id(&request.block_id, &[CoreBlock::LABEL])?
        else {
            return Ok(CoreEditOutcome::NotFound);
        };

        let mut attester_ids: Vec<String> = distinct
            .iter()
            .map(|vote| vote.attester_id.to_string())
            .collect();
        attester_ids.sort();
        // Content-addressed over the whole applied verdict — block, transition,
        // editor, attester set, instant — so an at-least-once replay of the same edit
        // converges to one audit row (the audit funnel dedups by id, the attester
        // edges are write-when-absent, and the swap is byte-idempotent), while any
        // *different* edit keeps its own row. This matters for the no-op transition
        // (prior == new), the one case the compare-and-swap cannot distinguish a
        // replay from a first apply.
        let fold = format!(
            "core_edit|{}|{}|{}|{}|{}|{}",
            block.identity.id,
            prior_hash.as_str(),
            new_hash.as_str(),
            principal.agent_id,
            attester_ids.join(","),
            request.at.timestamp().as_millisecond()
        );
        let audit = AuditEvent {
            identity: namespace_identity(
                Id::from_content_hash(fold.as_bytes()),
                block.identity.namespace.clone(),
                &request.at,
            ),
            kind: AuditKind::CoreEdit,
            subject_id: request.block_id,
            actor_id: principal.agent_id,
            payload: serde_json::json!({
                "outcome": "applied",
                "editor_id": principal.agent_id.to_string(),
                "attester_ids": attester_ids,
                "human_attester_id": human_attester_id.map(|id| id.to_string()),
                "sensitivity": block.sensitivity,
                "block_kind": block.block_kind,
                "require_human": requirement.require_human,
                "k_required": requirement.k,
                "prior_content_hash": prior_hash.as_str(),
                "new_content_hash": new_hash.as_str(),
            }),
            signature: String::new(),
            occurred_at: request.at.clone(),
        };
        let audit_id = audit.identity.id;
        let replacement = CoreBlockReplacement {
            content: request.content.clone(),
            drift_baseline: request.drift_baseline.clone(),
            embedding: request.embedding.clone(),
        };

        match self.store.edit_core_block(
            candidate.node,
            &request.expected_prior,
            &replacement,
            &attestations,
            required_active,
            &audit,
        )? {
            CoreEditWrite::Applied {
                attesters_recorded, ..
            } => Ok(CoreEditOutcome::Applied(CoreEditReceipt {
                audit_id,
                attesters_recorded,
                prior_content_hash: prior_hash,
                new_content_hash: new_hash,
            })),
            // The block died between the gate and the write: gone is gone.
            CoreEditWrite::NotLive => Ok(CoreEditOutcome::NotFound),
            CoreEditWrite::StaleContent => Ok(CoreEditOutcome::StaleContent),
            // The credited human retired between the gate's status read and the
            // commit; the in-lock re-check refused the whole edit, and the refusal is
            // audited like every human-requirement failure.
            CoreEditWrite::RequiredAttesterInactive => self.reject(
                principal,
                &block,
                request,
                CoreEditRejection::HumanAttestationRequired,
            ),
        }
    }

    /// Audit and return a gate rejection: one row in the block's own namespace, the
    /// principal as actor, the reason a scalar. Content-addressed with the instant
    /// folded in — rejections genuinely recur, and each attempt is its own row.
    fn reject(
        &self,
        principal: &Principal,
        block: &CoreBlock,
        request: &CoreEditRequest,
        rejection: CoreEditRejection,
    ) -> Result<CoreEditOutcome, CoreEditError> {
        let reason = match &rejection {
            CoreEditRejection::EditorUnverified => "editor_unverified",
            CoreEditRejection::AttestationFailed => "attestation_failed",
            CoreEditRejection::InsufficientAttesters { .. } => "insufficient_attesters",
            CoreEditRejection::HumanAttestationRequired => "human_attestation_required",
        };
        let key = format!(
            "core_edit_rejected|{}|{}|{}|{}",
            block.identity.id,
            principal.agent_id,
            reason,
            request.at.timestamp().as_millisecond()
        );
        // The refused transition rides in the payload — what the editor tried to
        // replace and with what — so the rejection row carries the same forensic
        // anchors as an applied row, and a count shortfall records the exact bar.
        let mut payload = serde_json::json!({
            "outcome": "rejected",
            "reason": reason,
            "editor_id": principal.agent_id.to_string(),
            "expected_prior_hash": request.expected_prior.as_str(),
            "new_content_hash": ContentHash::of(request.content.as_bytes()).as_str(),
        });
        if let CoreEditRejection::InsufficientAttesters { required, verified } = &rejection {
            payload["k_required"] = (*required).into();
            payload["verified"] = (*verified).into();
        }
        let audit = AuditEvent {
            identity: namespace_identity(
                Id::from_content_hash(key.as_bytes()),
                block.identity.namespace.clone(),
                &request.at,
            ),
            kind: AuditKind::CoreEdit,
            subject_id: block.identity.id,
            actor_id: principal.agent_id,
            payload,
            signature: String::new(),
            occurred_at: request.at.clone(),
        };
        self.store.commit_audit(&audit)?;
        Ok(CoreEditOutcome::Rejected(rejection))
    }
}

/// The audit identity for a core-edit event, addressed to the block's own namespace —
/// agent-visible through the scoped audit reads, like every lifecycle audit.
fn namespace_identity(id: Id, namespace: Namespace, now: &Timestamp) -> Identity {
    Identity {
        id,
        ingested_at: now.clone(),
        namespace,
        expired_at: None,
    }
}
