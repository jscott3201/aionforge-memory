//! The core-block write surface (05 §4, M5.T04): the un-attested genesis create and
//! the attested whole-value edit.
//!
//! Both writes are **dumb and atomic** — the second-attester gate (distinct attesters,
//! attester-is-not-the-editor, the human requirement for sensitive blocks) is enforced
//! *above* the store by the trust-layer orchestrator, which verifies every signature
//! first and then hands this surface a fully-judged write to persist in one commit.
//! Mirrors the attestation surface: edges are written only when absent (the
//! `ATTESTED_BY` payload is immutable), the audit goes through the single
//! [`crate::audit::ensure_event`] funnel, and nothing publishes if any step fails.
//!
//! The edit is an **in-place whole-value swap on the block's one stable node** — never
//! a version node. The non-lossy history of the replaced content lives in the signed
//! `core_edit` audit trail (prior and new content hashes in the payload the
//! orchestrator builds), so the graph holds exactly one row per block and an erasure
//! of that id destroys the whole block with nothing orphaned.

use aionforge_domain::edges::{AttestedBy, Audit};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::ContentHash;
use aionforge_domain::nodes::agent::AgentStatus;
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::nodes::forensic::AuditEvent;
use selene_core::{DbString, LabelDiff, NodeId, PropertyDiff, PropertyMap, Value, db_string};

use crate::attestation::attested_by_props;
use crate::convert::{
    as_str, embedder_model_value, embedding_value, enum_value, json_value, key, string_value,
};
use crate::error::StoreError;
use crate::store::Store;
use crate::{audit, core_block, materialize};

const EXPIRED_AT: &str = "expired_at";
const CONTENT: &str = "content";
const DRIFT_BASELINE: &str = "drift_baseline";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const STATUS: &str = "status";

/// The whole-value replacement an attested edit applies (06 §2: `CoreBlock.content` is
/// attested whole-value — replaced entire, never merged).
#[derive(Debug, Clone)]
pub struct CoreBlockReplacement {
    /// The new content, replacing the old entirely.
    pub content: String,
    /// A new drift baseline, or `None` to **carry the existing baseline forward
    /// unchanged** — an ordinary content edit never re-baselines drift (re-baselining
    /// under the edit would let drift launder itself past the M5.T05 detector; setting
    /// this is the detector's own privileged call).
    pub drift_baseline: Option<serde_json::Value>,
    /// The embedding of the new content, with its model identity — or `None`, which
    /// **removes** the stored embedding: the old vector indexes the old content, and
    /// serving it would surface this block for queries matching text it no longer
    /// says. Honest absence over a stale match.
    pub embedding: Option<(Embedding, EmbedderModel)>,
}

/// One attester's recorded vote for an edit: the agent's resolved node and the
/// immutable edge payload. The orchestrator has already verified the signature; the
/// store just persists the edge.
#[derive(Debug, Clone)]
pub struct CoreAttestation {
    /// The attesting agent's node.
    pub attester: NodeId,
    /// The immutable `ATTESTED_BY` payload (instant, signature, optional category).
    pub edge: AttestedBy,
}

/// The outcome of an attested core-block edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreEditWrite {
    /// The swap, the attester edges, and the audit were committed together.
    Applied {
        /// The `core_edit` audit row (existing on a replay, freshly written otherwise).
        audit: NodeId,
        /// Distinct attester nodes recorded for this edit (duplicates in the call
        /// collapse to one edge each).
        attesters_recorded: usize,
    },
    /// The target is not a live core block — missing, purged, or retired
    /// (`expired_at` set). Probed under the write lock; nothing was written.
    NotLive,
    /// The block's current content is not the content the caller's precondition named
    /// — a concurrent edit landed between the orchestrator's read and this write. The
    /// whole edit is refused (no swap, no edges, no audit): applying it would record
    /// attester votes and a prior-hash claim against bytes that were never the actual
    /// predecessor, tearing the audit chain that *is* the block's non-lossy history.
    StaleContent,
    /// The edit named a required-active attester (the credited human, 05 §4) whose
    /// agent row is no longer `Active` — re-checked here, under the same write lock as
    /// the swap, because the gate's status read is a separate lock acquisition and a
    /// reviewer retired mid-flight must fail closed rather than ride a stale verdict.
    /// The whole edit is refused; nothing was written.
    RequiredAttesterInactive,
}

impl Store {
    /// Create a core block, atomically: the node plus its `core_edit` genesis audit
    /// and `AUDIT` edge in one commit (05 §4, M5.T04).
    ///
    /// Creation is the **un-attested** half of the contract — the second-attester rule
    /// gates *edits* (a genesis block has no prior identity to drift from), and the
    /// facade's namespace authorization decides who may create where. `CoreBlock.id`
    /// is `UNIQUE`, so a duplicate id fails the commit rather than silently rewriting.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, a mutation, or the commit fails.
    pub fn create_core_block(
        &self,
        block: &CoreBlock,
        audit_event: &AuditEvent,
    ) -> Result<NodeId, StoreError> {
        let (labels, props) = core_block::to_node(block)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let node = {
            let mut mutator = txn.mutator();
            let node = mutator.create_node(labels, props)?;
            let ensured = audit::ensure_event(&mut mutator, audit_event, self.audit_signer())?;
            if ensured.created {
                mutator.create_edge(
                    audit_edge,
                    ensured.node,
                    node,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
            node
        };
        txn.commit()?;
        Ok(node)
    }

    /// Apply an attested whole-value edit to a live core block, atomically (05 §4):
    /// the in-place content swap, every attester's immutable `ATTESTED_BY` edge
    /// (written only when absent), and the `core_edit` audit with its `AUDIT` edge —
    /// one commit under the write lock, so the liveness probe, the content
    /// precondition, the swap, and the attestation record can never interleave with
    /// another writer.
    ///
    /// `expected_prior` is the compare-and-swap precondition: the hash of the content
    /// the orchestrator read, hashed into the audit payload, and handed to the
    /// attesters to vouch over. It is re-checked here, under the same lock as the
    /// swap — the orchestrator's read and this write are separate lock acquisitions,
    /// so without the in-lock check a racing edit could land between them and this
    /// write would silently tear the audit chain. A mismatch is the typed
    /// [`CoreEditWrite::StaleContent`] whole-op refusal.
    ///
    /// The gate has already run: the orchestrator verified every attester signature,
    /// excluded the editor, and resolved the requirement for this block's sensitivity.
    /// A missing or retired target is the typed [`CoreEditWrite::NotLive`], not an
    /// error — the orchestrator pre-resolves the block, so reaching it is a benign
    /// race with a concurrent purge or retirement.
    ///
    /// `required_active` names the one attester whose `Active` status is part of the
    /// gate's verdict (the credited human, 05 §4) and so must still hold *at commit*:
    /// the orchestrator's status read is its own lock acquisition, and this re-check
    /// under the write lock is what keeps a reviewer retired mid-flight from carrying
    /// an edit — the status twin of the content precondition. A mismatch is the typed
    /// [`CoreEditWrite::RequiredAttesterInactive`] whole-op refusal.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, a mutation, or the commit fails. The
    /// whole edit is one transaction: an error leaves the block untouched.
    pub fn edit_core_block(
        &self,
        block: NodeId,
        expected_prior: &ContentHash,
        replacement: &CoreBlockReplacement,
        attestations: &[CoreAttestation],
        required_active: Option<NodeId>,
        audit_event: &AuditEvent,
    ) -> Result<CoreEditWrite, StoreError> {
        let expired_key = db_string(EXPIRED_AT)?;
        let content_key = db_string(CONTENT)?;
        let status_key = db_string(STATUS)?;
        let active = enum_value(&AgentStatus::Active)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let outcome = {
            let mut mutator = txn.mutator();
            // Probe under the write lock: the target must be a live, unretired block
            // whose current content is the precondition's content.
            enum Probe {
                NotLive,
                Stale,
                AttesterInactive,
                Live { has_embedding: bool },
            }
            let probe = {
                let graph = mutator.read();
                // The status precondition first: the credited attester must still be
                // an `Active` agent row in this same locked view as the swap.
                let attester_active = match required_active {
                    None => true,
                    Some(node) => graph
                        .node_properties(node)
                        .and_then(|props| props.get(&status_key).cloned())
                        .is_some_and(|status| status == active),
                };
                if !attester_active {
                    Probe::AttesterInactive
                } else {
                    match graph.node_properties(block) {
                        None => Probe::NotLive,
                        Some(props) if props.get(&expired_key).is_some() => Probe::NotLive,
                        Some(props) => {
                            let current = props.get(&content_key).ok_or_else(|| {
                                StoreError::decode(
                                    "core block missing required property `content`".to_string(),
                                )
                            })?;
                            if ContentHash::of(as_str(current)?.as_bytes()) != *expected_prior {
                                Probe::Stale
                            } else {
                                Probe::Live {
                                    has_embedding: props.get(&db_string(EMBEDDING)?).is_some(),
                                }
                            }
                        }
                    }
                }
            };
            match probe {
                Probe::NotLive => CoreEditWrite::NotLive,
                Probe::Stale => CoreEditWrite::StaleContent,
                Probe::AttesterInactive => CoreEditWrite::RequiredAttesterInactive,
                Probe::Live { has_embedding } => {
                    // The whole-value swap: content always; drift_baseline only when the
                    // caller re-baselines (None = carry forward); the embedding pair swaps
                    // with fresh-content vectors or is removed as stale.
                    let mut sets: Vec<(DbString, Value)> =
                        vec![(key(CONTENT)?, string_value(&replacement.content)?)];
                    if let Some(baseline) = &replacement.drift_baseline {
                        sets.push((key(DRIFT_BASELINE)?, json_value(baseline)?));
                    }
                    let mut removes: Vec<DbString> = Vec::new();
                    match &replacement.embedding {
                        Some((embedding, model)) => {
                            sets.push((key(EMBEDDING)?, embedding_value(embedding)?));
                            sets.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
                        }
                        None if has_embedding => {
                            removes.push(key(EMBEDDING)?);
                            removes.push(key(EMBEDDER_MODEL)?);
                        }
                        None => {}
                    }
                    mutator.update_node(
                        block,
                        LabelDiff::new([], [])?,
                        PropertyDiff::new(sets, removes)?,
                    )?;

                    // One immutable edge per distinct attester; duplicates in the call and
                    // re-attests of an already-voting agent collapse through the same
                    // write-when-absent discipline as fact attestation.
                    let mut recorded: Vec<NodeId> = Vec::new();
                    for attestation in attestations {
                        if recorded.contains(&attestation.attester) {
                            continue;
                        }
                        recorded.push(attestation.attester);
                        let edge_props = attested_by_props(&attestation.edge)?;
                        materialize::ensure_edge(
                            &mut mutator,
                            AttestedBy::LABEL,
                            block,
                            attestation.attester,
                            edge_props,
                        )?;
                    }

                    let ensured =
                        audit::ensure_event(&mut mutator, audit_event, self.audit_signer())?;
                    if ensured.created {
                        mutator.create_edge(
                            audit_edge,
                            ensured.node,
                            block,
                            PropertyMap::from_pairs(Vec::new())?,
                        )?;
                    }
                    CoreEditWrite::Applied {
                        audit: ensured.node,
                        attesters_recorded: recorded.len(),
                    }
                }
            }
        };
        txn.commit()?;
        Ok(outcome)
    }
}
