//! Read-only candidate surfaces for the active-forgetting sweep (05 §2, M5.T02).
//!
//! Two readers, no mutations: [`Store::forgettable_candidates`] enumerates the
//! sweep-scoped population (`Episode` and `Fact` — the kinds the design rules sweepable)
//! page by page, and [`Store::has_protecting_reference`] answers the "unreferenced" axis
//! with a short-circuiting probe over a node's live incoming edges. Both read only the
//! committed graph, so they run off any cursor and add no node, edge, or index.
//!
//! The candidate page filters `expired_at IS NULL` at the source: an already-forgotten or
//! demotion-quarantined node never re-enters a page, which is half of the sweep's
//! idempotency (the write-side under-lock guard is the other half). Pagination is keyset
//! over the `(label, id)` ordering — labels in the fixed scan order, ids byte-ordered
//! within a label — so a page boundary is stable under concurrent writes and a resumed
//! sweep visits exactly the nodes a single full scan would have.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::RelatesTo;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::nodes::work::{Tag, WorkItem};
use selene_core::{PropertyMap, Value, db_string};
use selene_graph::RowIndex;
use serde::{Deserialize, Serialize};

use crate::audit_read::MAX_AUDIT_PAGE;
use crate::convert::{as_f64, as_id, as_namespace, as_timestamp, as_u64};
use crate::error::StoreError;
use crate::store::Store;
use crate::{NodeId, convert};

const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
const IMPORTANCE: &str = "importance";
const TRUST: &str = "trust";
const LAST_ACCESS: &str = "last_access";
const ACCESS_COUNT_RECENT: &str = "access_count_recent";
const REFERENCED_COUNT: &str = "referenced_count";
const SURPRISE: &str = "surprise";
const IS_PINNED: &str = "is_pinned";
const VALID_TO: &str = "valid_to";

/// The kinds the batch sweep enumerates, in the fixed scan order the cursor is keyed on.
///
/// Deliberately only `Episode` and `Fact` (05 §2): `CoreBlock` is hard-exempt, `Skill`
/// lifecycle belongs to deprecate-never-delete, `BadPattern` is protected negative
/// knowledge behind its own config toggle, and `Entity`/`Note` are deferred. Point ops
/// may reach further; the *sweep* never does.
pub const FORGET_SCAN_LABELS: [&str; 2] = [Episode::LABEL, Fact::LABEL];

/// A keyset pagination cursor over the `(label, id)` scan ordering.
///
/// Continuation is by ordering key, not by offset, so a page boundary is stable under
/// concurrent writes. It is the `(label, id)` of the last candidate on a page; the next
/// page returns candidates strictly after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetCursor {
    /// Label of the last candidate on the prior page.
    pub label: String,
    /// Id of the last candidate on the prior page — the tiebreak within one label.
    pub id: Id,
}

/// One sweep candidate: the node handle plus exactly the blocks the eligibility axes
/// read. The label travels with it so the caller selects the tier half-life per node.
#[derive(Debug, Clone)]
pub struct ForgetCandidate {
    /// The committed node.
    pub node: NodeId,
    /// The candidate's kind label, for per-node tier selection.
    pub label: String,
    /// Identity block — `ingested_at` feeds the min-age axis, `expired_at` is `None` by
    /// construction (already-expired nodes never enter a page).
    pub identity: Identity,
    /// Stats block — `importance`, `trust`, `last_access`, and `is_pinned` feed the
    /// pure axes.
    pub stats: Stats,
}

/// One page of sweep candidates plus the continuation cursor.
///
/// `next` is `Some` exactly when the page filled to the limit; a scan that ends flush on
/// a page boundary costs one extra empty page that returns `next: None`.
#[derive(Debug, Clone, Default)]
pub struct ForgetCandidatePage {
    /// The candidates, in `(label, id)` scan order.
    pub candidates: Vec<ForgetCandidate>,
    /// Where the next page resumes, or `None` when the scan is complete.
    pub next: Option<ForgetCursor>,
}

/// A memory resolved to its full typed body, tagged by lifecycle kind — the read-side
/// counterpart to [`ForgetCandidate`].
///
/// Where [`ForgetCandidate`] carries only the [`Identity`]/[`Stats`] blocks the forgetting
/// gates read, this carries the *whole* decoded node so a by-id read (`read_memory`) can
/// render it per kind. Every variant composes [`Identity`], so [`ResolvedMemory::identity`]
/// yields the namespace/expiry a visibility gate needs without re-matching the kind.
#[derive(Debug, Clone)]
pub enum ResolvedMemory {
    /// An episodic turn.
    Episode(Episode),
    /// A canonical semantic fact.
    Fact(Fact),
    /// A canonical entity.
    Entity(Entity),
    /// An associative note.
    Note(Note),
    /// A versioned procedural skill.
    Skill(Skill),
    /// A negative procedural memory (recorded failure mode).
    BadPattern(BadPattern),
    /// An identity-tier core block (persona/commitment/redline).
    Core(CoreBlock),
    /// A work-tracking item (Identity-only; not a forgettable memory).
    WorkItem(WorkItem),
    /// A classification tag (Identity-only; not a forgettable memory).
    Tag(Tag),
}

impl ResolvedMemory {
    /// The shared [`Identity`] block, regardless of kind — the namespace and expiry a
    /// visibility gate reads.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        match self {
            Self::Episode(memory) => &memory.identity,
            Self::Fact(memory) => &memory.identity,
            Self::Entity(memory) => &memory.identity,
            Self::Note(memory) => &memory.identity,
            Self::Skill(memory) => &memory.identity,
            Self::BadPattern(memory) => &memory.identity,
            Self::Core(memory) => &memory.identity,
            Self::WorkItem(memory) => &memory.identity,
            Self::Tag(memory) => &memory.identity,
        }
    }
}

impl Store {
    /// One page of not-yet-expired sweep candidates over the in-scope kinds, with the
    /// blocks the eligibility axes read (05 §2, M5.T02).
    ///
    /// `limit` is clamped to [`MAX_AUDIT_PAGE`] like every paged read. The enumeration is
    /// the L0 spine authority — namespace scoping is not a sweep concern; each forget is
    /// audited in the memory's own namespace by the write side.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a row decode fails or the cursor names a label outside
    /// the scan order (a continuation from a different build, not a resumable position).
    pub fn forgettable_candidates(
        &self,
        after: Option<&ForgetCursor>,
        limit: usize,
    ) -> Result<ForgetCandidatePage, StoreError> {
        let limit = limit.clamp(1, MAX_AUDIT_PAGE);
        let after_rank = after
            .map(|cursor| {
                FORGET_SCAN_LABELS
                    .iter()
                    .position(|label| *label == cursor.label)
                    .ok_or_else(|| {
                        StoreError::decode(format!(
                            "forget cursor label `{}` is not in the sweep scan order",
                            cursor.label
                        ))
                    })
            })
            .transpose()?;

        let snapshot = self.graph().read();
        let mut candidates = Vec::new();
        'labels: for (rank, label) in FORGET_SCAN_LABELS.iter().enumerate() {
            // Labels wholly before the cursor are already swept.
            if after_rank.is_some_and(|cursor_rank| rank < cursor_rank) {
                continue;
            }
            let db_label = db_string(label)?;
            let Some(rows) = snapshot.nodes_with_label(&db_label) else {
                continue;
            };
            let mut page: Vec<ForgetCandidate> = Vec::new();
            for row in rows.iter() {
                let row = RowIndex::new(row);
                let Some(node) = snapshot.node_id_for_row(row) else {
                    continue;
                };
                let Some(props) = snapshot.node_properties(node) else {
                    continue;
                };
                // Already-expired nodes (prior soft-forget, demotion quarantine) never
                // enter a page: re-scans converge instead of re-forgetting.
                if props.get(&db_string(EXPIRED_AT)?).is_some() {
                    continue;
                }
                // Within the cursor's own label, trim on the raw id *before* the full
                // block decode: the consumed prefix is re-scanned on every resumed page,
                // and decoding it each time is the O(N²/limit) shape the audit reader
                // deliberately avoids. A missing id falls through to the decoder, which
                // owns the error message.
                if let Some(cursor) = after
                    && cursor.label == *label
                    && let Some(value) = props.get(&db_string(ID)?)
                    && as_id(value)? <= cursor.id
                {
                    continue;
                }
                let (identity, stats) = blocks_from_properties(props)?;
                page.push(ForgetCandidate {
                    node,
                    label: (*label).to_string(),
                    identity,
                    stats,
                });
            }
            // Byte-ordered ids make the page order deterministic regardless of row order.
            page.sort_by_key(|candidate| candidate.identity.id);
            for candidate in page {
                if candidates.len() == limit {
                    break 'labels;
                }
                candidates.push(candidate);
            }
        }

        let next = (candidates.len() == limit)
            .then(|| {
                candidates.last().map(|last| ForgetCursor {
                    label: last.label.clone(),
                    id: last.identity.id,
                })
            })
            .flatten();
        Ok(ForgetCandidatePage { candidates, next })
    }

    /// The node carrying `id` under the first of `labels` that has it, with the blocks
    /// the forgetting gates read — the point-op resolver (05 §2, M5.T02).
    ///
    /// Unlike the candidate page this does **not** filter `expired_at`: un-forget must
    /// find an already-forgotten node, and a point-forget of an expired node must be
    /// able to report "already forgotten" rather than "not found". Ids are unique per
    /// kind and the caller's label set is disjoint by construction, so first-hit is the
    /// only hit.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a lookup or block decode fails.
    pub fn memory_by_id(
        &self,
        id: &Id,
        labels: &[&str],
    ) -> Result<Option<ForgetCandidate>, StoreError> {
        let snapshot = self.graph().read();
        for label in labels {
            let Some(node) = convert::node_by_id(&snapshot, label, id)? else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let (identity, stats) = blocks_from_properties(props)?;
            return Ok(Some(ForgetCandidate {
                node,
                label: (*label).to_string(),
                identity,
                stats,
            }));
        }
        Ok(None)
    }

    /// The full typed memory carrying `id` under the first of `labels` that has it,
    /// decoded under a **single** snapshot — the read-side point resolver behind
    /// `read_memory`.
    ///
    /// Two properties make this the read counterpart to [`Store::memory_by_id`] rather than
    /// a thin wrapper over it:
    ///
    /// 1. It returns the whole decoded body ([`ResolvedMemory`]), not just the gate blocks,
    ///    so the caller can render per kind.
    /// 2. It resolves the node **and** decodes its body within one `graph().read()`
    ///    snapshot. A resolve-then-decode chain across two snapshots (e.g. `memory_by_id`
    ///    followed by a `*_by_node_id` decode) opens a window in which a node forgotten
    ///    between the two reads still decodes — the visibility gate would then read
    ///    `expired_at` from a *different* snapshot than the one that produced the body. One
    ///    snapshot makes the gate's expiry check provably consistent with the rendered body.
    ///
    /// Like `memory_by_id` it does **not** itself filter `expired_at`: the caller's
    /// visibility gate owns that decision (a by-id read must drop an expired node without
    /// leaking that it merely "exists but is forgotten"). Ids are unique per kind and the
    /// label set is disjoint by construction, so first-hit is the only hit; an unrecognized
    /// label is skipped.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a lookup or block decode fails.
    pub fn resolved_memory_by_id(
        &self,
        id: &Id,
        labels: &[&str],
    ) -> Result<Option<ResolvedMemory>, StoreError> {
        let snapshot = self.graph().read();
        for label in labels {
            let Some(node) = convert::node_by_id(&snapshot, label, id)? else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            // Decode the kind's full body off the props already in hand, under this same
            // snapshot. The match is exhaustive over MCP-surfaced labels; any other label
            // (an edge kind, an internal node) is not a readable memory and is skipped.
            let resolved = match *label {
                Episode::LABEL => ResolvedMemory::Episode(crate::episode::from_properties(props)?),
                Fact::LABEL => ResolvedMemory::Fact(crate::fact::from_properties(props)?),
                Entity::LABEL => ResolvedMemory::Entity(crate::entity::from_properties(props)?),
                Note::LABEL => ResolvedMemory::Note(crate::note::from_properties(props)?),
                Skill::LABEL => ResolvedMemory::Skill(crate::skill::from_properties(props)?),
                BadPattern::LABEL => {
                    ResolvedMemory::BadPattern(crate::bad_pattern::from_properties(props)?)
                }
                CoreBlock::LABEL => {
                    ResolvedMemory::Core(crate::core_block::from_properties(props)?)
                }
                // Identity-only kinds: decoded by their own `from_properties` (never
                // `blocks_from_properties`, which requires a Stats block these lack).
                WorkItem::LABEL => ResolvedMemory::WorkItem(crate::work::from_properties(props)?),
                Tag::LABEL => ResolvedMemory::Tag(crate::tag::from_properties(props)?),
                _ => continue,
            };
            return Ok(Some(resolved));
        }
        Ok(None)
    }

    /// Whether any edge with one of `labels` touches this node, in **either** direction —
    /// the attestation and promotion-lineage exemption probe (05 §2, M5.T02).
    ///
    /// Presence-based on purpose: an attestation is write-once with no de-attest path,
    /// and promotion lineage marks a node as governance's territory even after the
    /// window closes (a closed lineage edge still means `needs_resurrection` and the
    /// demotion guard can collide with a soft-forget). Sparing on a closed edge is the
    /// conservative side.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or edge read fails.
    pub fn has_adjacent_edge(&self, node: NodeId, labels: &[&str]) -> Result<bool, StoreError> {
        let snapshot = self.graph().read();
        for adjacency in [snapshot.incoming_edges(node), snapshot.outgoing_edges(node)]
            .into_iter()
            .flatten()
        {
            for label in labels {
                if adjacency.iter_label(&db_string(label)?).next().is_some() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether any **live** incoming edge from the protecting allowlist points at this
    /// node — the "unreferenced" eligibility axis, inverted (05 §2, M5.T02).
    ///
    /// The authoritative signal is the edges themselves, never the loss-tolerant
    /// `Stats::referenced_count` cache (§13.7). Most reference edges are structural and
    /// live by existence; `RELATES_TO` is bi-temporally versioned, so a closed or expired
    /// version does not protect. Short-circuits on the first protecting hit.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or edge read fails.
    pub fn has_protecting_reference(
        &self,
        node: NodeId,
        allowlist: &[&str],
    ) -> Result<bool, StoreError> {
        let snapshot = self.graph().read();
        let Some(adjacency) = snapshot.incoming_edges(node) else {
            return Ok(false);
        };
        for label in allowlist {
            let db_label = db_string(label)?;
            for edge in adjacency.iter_label(&db_label) {
                if *label != RelatesTo::LABEL {
                    return Ok(true);
                }
                // A RELATES_TO version protects only while current: open window, not
                // expired.
                let Some(props) = snapshot.edge_properties(edge.edge_id) else {
                    continue;
                };
                if props.get(&db_string(VALID_TO)?).is_none()
                    && props.get(&db_string(EXPIRED_AT)?).is_none()
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// The `Identity` and `Stats` blocks off a sweep candidate's property map.
///
/// Every `Stats`-bearing kind serializes the two blocks under the same property names
/// (the schema mirrors domain serialization exactly), so one decoder covers the scan.
fn blocks_from_properties(props: &PropertyMap) -> Result<(Identity, Stats), StoreError> {
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
        is_pinned: convert::as_bool(require(IS_PINNED)?)?,
    };
    Ok((identity, stats))
}

#[cfg(test)]
mod tests {
    //! The `RELATES_TO` liveness arm needs a closed link version, and link versions live
    //! between `Note`s — a kind with no public id-to-node surface — so this one test sits
    //! in-module where `convert::node_by_id` is reachable. Everything else about the
    //! probe is covered on the public surface in `tests/forget_read.rs`.

    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::associative::Note;
    use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
    use aionforge_domain::time::Timestamp;

    use super::*;
    use crate::config::StoreConfig;
    use crate::distill::DistilledNoteWrite;
    use crate::note::MaterializedNote;
    use crate::relates_to::LinkEdgeWrite;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime literal")
    }

    fn now() -> Timestamp {
        ts("2026-06-06T12:00:00-05:00[America/Chicago]")
    }

    fn seed_note(store: &Store, seed: &[u8]) -> Id {
        let id = Id::from_content_hash(seed);
        let identity = |id: Id| Identity {
            id,
            ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
            namespace: Namespace::Global,
            expired_at: None,
        };
        let note = Note {
            identity: identity(id),
            stats: Stats {
                importance: 0.5,
                trust: 0.8,
                last_access: now(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.1,
                is_pinned: false,
            },
            content: format!("note {}", String::from_utf8_lossy(seed)),
            context: None,
            keywords: Vec::new(),
            embedding: None,
            embedder_model: None,
            derived_from_episode: None,
        };
        let audit = AuditEvent {
            identity: identity(Id::from_content_hash(&[seed, b"-audit"].concat())),
            kind: AuditKind::Distill,
            subject_id: id,
            actor_id: Id::from_content_hash(b"seed"),
            payload: serde_json::json!({"outcome": "written"}),
            signature: String::new(),
            occurred_at: now(),
        };
        store
            .materialize_distilled_notes(
                &[DistilledNoteWrite {
                    note: MaterializedNote {
                        note,
                        source_facts: Vec::new(),
                    },
                    audit,
                }],
                &[],
                &now(),
            )
            .expect("seed note");
        id
    }

    #[test]
    fn a_closed_relates_to_version_does_not_protect() {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: 4,
        })
        .expect("open store");
        store
            .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
            .expect("migrate");

        let source = seed_note(&store, b"forget-probe-source");
        let second = seed_note(&store, b"forget-probe-second");
        let target = seed_note(&store, b"forget-probe-target");
        let link = |from: Id| LinkEdgeWrite {
            source_id: from,
            target_id: target,
            relationship_label: "refines".to_string(),
            valid_from: now(),
        };
        store
            .materialize_link_edges(&[link(source), link(second)], &[], &[], &now())
            .expect("open links");

        let target_node = convert::node_by_id(&store.graph().read(), Note::LABEL, &target)
            .expect("lookup")
            .expect("target exists");
        assert!(
            store
                .has_protecting_reference(target_node, &[RelatesTo::LABEL])
                .expect("probe"),
            "a current link version protects its target"
        );

        // Close the first version (the M3.T09 revision shape). The probe must skip the
        // closed edge and keep walking — the second, still-live link protects.
        let first = store.relates_to_links(&source).expect("links")[0].clone();
        store
            .materialize_link_edges(&[], &[first.edge_id], &[], &now())
            .expect("close first link");
        assert!(
            store
                .has_protecting_reference(target_node, &[RelatesTo::LABEL])
                .expect("probe"),
            "a closed version does not mask a live sibling"
        );

        // Close the second as well; nothing live remains.
        let rest = store.relates_to_links(&second).expect("links")[0].clone();
        store
            .materialize_link_edges(&[], &[rest.edge_id], &[], &now())
            .expect("close second link");
        assert!(
            !store
                .has_protecting_reference(target_node, &[RelatesTo::LABEL])
                .expect("probe"),
            "closed link versions no longer protect"
        );
    }
}
