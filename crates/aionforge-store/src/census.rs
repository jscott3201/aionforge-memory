//! Store-level census readers for live memory and work inventory.
//!
//! These readers are principal-agnostic. Callers must apply namespace authorization
//! before passing namespace lists into this module.

use std::collections::HashSet;

use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::nodes::work::{WorkItem, WorkStatus};
use selene_core::{DbString, NodeId, Value, db_string};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{enum_value, namespace_value};
use crate::error::StoreError;
use crate::store::Store;

/// The selene-db labels that carry a live memory: the canonical public list of the
/// memory-bearing kinds (episodic, semantic, associative, procedural). Cursors, schema
/// versions, audit/provenance, agents, sessions, and anchors are not memories and are
/// deliberately excluded. [`Store::memory_counts`] counts exactly these kinds.
pub const MEMORY_LABELS: [&str; 6] = [
    Episode::LABEL,
    Fact::LABEL,
    Entity::LABEL,
    Note::LABEL,
    Skill::LABEL,
    BadPattern::LABEL,
];

/// A live per-kind memory census (global operator telemetry).
///
/// Every field counts only *live* nodes of that kind: rows whose `expired_at` is
/// unset, so soft-forgotten memories (which keep their row) are excluded. The fields
/// are taken against one pinned snapshot so they and [`MemoryCounts::total`] are
/// mutually consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryCounts {
    /// Live episodes (episodic tier).
    pub episodes: u64,
    /// Live facts (semantic tier).
    pub facts: u64,
    /// Live entities (semantic tier).
    pub entities: u64,
    /// Live notes (associative tier).
    pub notes: u64,
    /// Live skills (procedural tier).
    pub skills: u64,
    /// Live bad patterns (procedural tier).
    pub bad_patterns: u64,
}

impl MemoryCounts {
    /// Total live memories across every kind.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.episodes + self.facts + self.entities + self.notes + self.skills + self.bad_patterns
    }
}

/// A live per-status work-item census (global operator telemetry): the work-tracking
/// counterpart to [`MemoryCounts`].
///
/// Every field counts only *live* work items of that status (rows whose `expired_at` is unset),
/// taken against one pinned snapshot so the per-status fields and [`WorkCounts::total`] agree.
/// This is deliberately SEPARATE from the memory census: work items are exempt from forgetting
/// and are never counted as memories, so a work item never moves [`MemoryCounts::total`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkCounts {
    /// Items awaiting a start (`todo`).
    pub todo: u64,
    /// Items underway (`in_progress`).
    pub in_progress: u64,
    /// Items blocked on a dependency (`blocked`).
    pub blocked: u64,
    /// Completed items (`done`).
    pub done: u64,
    /// Abandoned items (`dropped`).
    pub dropped: u64,
}

impl WorkCounts {
    /// Total live work items across every status.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.todo + self.in_progress + self.blocked + self.done + self.dropped
    }
}

impl Store {
    /// A live per-kind census of every memory-bearing kind (operator telemetry).
    ///
    /// A node is *live* iff its `expired_at` property is unset: soft-forget keeps the
    /// row and merely stamps `expired_at` (02 section 3), so the unset check is the same
    /// liveness predicate as GQL's `IS NULL`. All six labels in [`MEMORY_LABELS`] are
    /// counted against ONE pinned read snapshot, so the per-kind fields and
    /// [`MemoryCounts::total`] are mutually consistent (no row is counted under one
    /// generation and missed under another). This is engine-native: it reads the label
    /// bitmap and subtracts the expired rows, with no bolt-on counter to drift.
    ///
    /// This is an operator-wide census: it takes no principal and counts live nodes
    /// across *all* namespaces and roles, so it is deliberately broader than recall
    /// (which excludes the System namespace and system-role episodes). Only the
    /// aggregate per-kind numbers are surfaced, never content or per-namespace counts,
    /// so it exposes no individual namespace's existence.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or property key cannot be interned.
    pub fn memory_counts(&self) -> Result<MemoryCounts, StoreError> {
        // One pinned snapshot for all six labels: the per-kind fields and total() must
        // agree, which they only can if every label is counted against the same version.
        let snapshot = self.graph().read();
        let expired_key = db_string("expired_at")?;
        let mut counts = MemoryCounts::default();
        let count_label = |label_str: &str| -> Result<u64, StoreError> {
            let label = db_string(label_str)?;
            let Some(rows) = snapshot.nodes_with_label(&label) else {
                return Ok(0);
            };
            let mut live = 0u64;
            for row in rows.iter() {
                let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                    continue;
                };
                let Some(props) = snapshot.node_properties(node) else {
                    continue;
                };
                // `expired_at` is written only on soft-forget (02 section 3); its absence is
                // live, matching GQL `IS NULL` and the store's presence-based liveness
                // convention (forget_read.rs / cooling.rs / forget_write.rs).
                if props.get(&expired_key).is_none() {
                    live += 1;
                }
            }
            Ok(live)
        };
        counts.episodes = count_label(Episode::LABEL)?;
        counts.facts = count_label(Fact::LABEL)?;
        counts.entities = count_label(Entity::LABEL)?;
        counts.notes = count_label(Note::LABEL)?;
        counts.skills = count_label(Skill::LABEL)?;
        counts.bad_patterns = count_label(BadPattern::LABEL)?;
        Ok(counts)
    }

    /// A live per-status census of work items (operator telemetry): the work-tracking
    /// counterpart to [`Store::memory_counts`].
    ///
    /// Each [`WorkStatus`] bucket is counted against ONE pinned snapshot (so the per-status
    /// fields and [`WorkCounts::total`] are mutually consistent) by probing the `work_status`
    /// scalar index per status and keeping only *live* rows (those whose `expired_at` is unset:
    /// the same liveness predicate as `memory_counts`). This census is deliberately SEPARATE
    /// from the memory census: work items are exempt from forgetting and are never memories, so
    /// `work_counts` and `memory_counts` share no rows and a work item never moves
    /// [`MemoryCounts::total`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or property key cannot be interned.
    pub fn work_counts(&self) -> Result<WorkCounts, StoreError> {
        // One pinned snapshot across all five status buckets, so the fields and total() agree.
        let snapshot = self.graph().read();
        let label = db_string(WorkItem::LABEL)?;
        let status_key = db_string("work_status")?;
        let expired_key = db_string("expired_at")?;
        let count_status = |status: WorkStatus| -> Result<u64, StoreError> {
            let value = enum_value(&status)?;
            let Some(rows) = snapshot.nodes_with_property_eq(&label, &status_key, &value) else {
                return Ok(0);
            };
            let mut live = 0u64;
            for row in rows.iter() {
                let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                    continue;
                };
                let Some(props) = snapshot.node_properties(node) else {
                    continue;
                };
                if props.get(&expired_key).is_none() {
                    live += 1;
                }
            }
            Ok(live)
        };
        Ok(WorkCounts {
            todo: count_status(WorkStatus::Todo)?,
            in_progress: count_status(WorkStatus::InProgress)?,
            blocked: count_status(WorkStatus::Blocked)?,
            done: count_status(WorkStatus::Done)?,
            dropped: count_status(WorkStatus::Dropped)?,
        })
    }

    /// Live per-kind memory counts for each requested namespace.
    ///
    /// This is principal-agnostic by design: callers must pass only namespaces they have already
    /// authorized. Every requested namespace is represented in the output, even when all buckets
    /// are zero, so an absent namespace is not an error and callers can distinguish "authorized
    /// but empty" from "filtered before store access" at the higher layer.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or property key cannot be interned.
    pub fn memory_counts_by_namespace(
        &self,
        namespaces: &[Namespace],
    ) -> Result<Vec<(Namespace, MemoryCounts)>, StoreError> {
        let snapshot = self.graph().read();
        let namespace_key = db_string("namespace")?;
        let expired_key = db_string("expired_at")?;
        let mut out: Vec<(Namespace, MemoryCounts)> = namespaces
            .iter()
            .cloned()
            .map(|namespace| (namespace, MemoryCounts::default()))
            .collect();

        for (slot, namespace) in namespaces.iter().enumerate() {
            let namespace_value = namespace_value(namespace)?;
            for label_str in MEMORY_LABELS {
                let label = db_string(label_str)?;
                let live = count_live_rows(
                    &snapshot,
                    &label,
                    &namespace_key,
                    &namespace_value,
                    &expired_key,
                );
                add_memory_count(&mut out[slot].1, label_str, live);
            }
        }
        Ok(out)
    }

    /// Live per-status work-item counts for each requested namespace.
    ///
    /// Work items stay in a separate bucket from memories, mirroring [`Store::work_counts`].
    /// This reader is principal-agnostic; callers own namespace authorization before invoking it.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label, property key, or status value cannot be interned.
    pub fn work_counts_by_namespace(
        &self,
        namespaces: &[Namespace],
    ) -> Result<Vec<(Namespace, WorkCounts)>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(WorkItem::LABEL)?;
        let namespace_key = db_string("namespace")?;
        let status_key = db_string("work_status")?;
        let expired_key = db_string("expired_at")?;
        let statuses = [
            WorkStatus::Todo,
            WorkStatus::InProgress,
            WorkStatus::Blocked,
            WorkStatus::Done,
            WorkStatus::Dropped,
        ];
        let mut out: Vec<(Namespace, WorkCounts)> = namespaces
            .iter()
            .cloned()
            .map(|namespace| (namespace, WorkCounts::default()))
            .collect();

        for (slot, namespace) in namespaces.iter().enumerate() {
            let namespace_value = namespace_value(namespace)?;
            for status in statuses {
                let status_value = enum_value(&status)?;
                let Some(rows) =
                    snapshot.nodes_with_property_eq(&label, &status_key, &status_value)
                else {
                    continue;
                };
                let mut live = 0u64;
                for row in rows.iter() {
                    let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                        continue;
                    };
                    let Some(props) = snapshot.node_properties(node) else {
                        continue;
                    };
                    if props.get(&namespace_key) == Some(&namespace_value)
                        && props.get(&expired_key).is_none()
                    {
                        live += 1;
                    }
                }
                add_work_count(&mut out[slot].1, status, live);
            }
        }
        Ok(out)
    }

    /// Live memory node ids for the requested labels and namespaces.
    ///
    /// The caller provides authorized namespaces and memory labels. This reads one pinned
    /// snapshot, probes the indexed `namespace` scalar for each `(label, namespace)` cell, filters
    /// out `expired_at`-stamped rows, and de-duplicates ids across overlapping namespace inputs.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or namespace value cannot be encoded.
    pub fn live_memory_nodes_in_namespaces(
        &self,
        labels: &[&str],
        namespaces: &[Namespace],
    ) -> Result<Vec<NodeId>, StoreError> {
        let snapshot = self.graph().read();
        let namespace_key = db_string("namespace")?;
        let expired_key = db_string("expired_at")?;
        let mut nodes = Vec::new();
        let mut seen = HashSet::new();
        for label_str in labels {
            let label = db_string(label_str)?;
            for namespace in namespaces {
                let namespace_value = namespace_value(namespace)?;
                let Some(rows) =
                    snapshot.nodes_with_property_eq(&label, &namespace_key, &namespace_value)
                else {
                    continue;
                };
                for row in rows.iter() {
                    let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                        continue;
                    };
                    let Some(props) = snapshot.node_properties(node) else {
                        continue;
                    };
                    if props.get(&expired_key).is_some() {
                        continue;
                    }
                    if seen.insert(node) {
                        nodes.push(node);
                    }
                }
            }
        }
        Ok(nodes)
    }
}

fn count_live_rows(
    snapshot: &SeleneGraph,
    label: &DbString,
    property: &DbString,
    value: &Value,
    expired_key: &DbString,
) -> u64 {
    let Some(rows) = snapshot.nodes_with_property_eq(label, property, value) else {
        return 0;
    };
    let mut live = 0u64;
    for row in rows.iter() {
        let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
            continue;
        };
        let Some(props) = snapshot.node_properties(node) else {
            continue;
        };
        if props.get(expired_key).is_none() {
            live += 1;
        }
    }
    live
}

fn add_memory_count(counts: &mut MemoryCounts, label: &str, value: u64) {
    match label {
        Episode::LABEL => counts.episodes = value,
        Fact::LABEL => counts.facts = value,
        Entity::LABEL => counts.entities = value,
        Note::LABEL => counts.notes = value,
        Skill::LABEL => counts.skills = value,
        BadPattern::LABEL => counts.bad_patterns = value,
        _ => {}
    }
}

fn add_work_count(counts: &mut WorkCounts, status: WorkStatus, value: u64) {
    match status {
        WorkStatus::Todo => counts.todo = value,
        WorkStatus::InProgress => counts.in_progress = value,
        WorkStatus::Blocked => counts.blocked = value,
        WorkStatus::Done => counts.done = value,
        WorkStatus::Dropped => counts.dropped = value,
    }
}

#[cfg(test)]
mod tests {
    //! The liveness filter must hold on every memory kind, not just `Episode`. The store
    //! layer can insert both an `Episode` and a `Fact` directly, so this proves the
    //! `expired_at`-unset predicate excludes soft-forgotten rows on a NON-`Episode` label
    //! too: the one behaviour the per-kind census hangs on.

    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::{ContentHash, Id};
    use aionforge_domain::nodes::episodic::{ConsolidationState, Role};
    use aionforge_domain::nodes::semantic::FactStatus;
    use aionforge_domain::value::ObjectValue;

    use super::*;
    use crate::config::StoreConfig;

    fn ts(text: &str) -> aionforge_domain::time::Timestamp {
        text.parse().expect("valid zoned datetime literal")
    }

    fn store() -> Store {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: 4,
        })
        .expect("open store");
        store
            .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
            .expect("migrate store");
        store
    }

    fn stats() -> Stats {
        Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        }
    }

    fn episode(content: &str, expired_at: Option<aionforge_domain::time::Timestamp>) -> Episode {
        episode_in(content, Namespace::Agent("alice".to_string()), expired_at)
    }

    fn episode_in(
        content: &str,
        namespace: Namespace,
        expired_at: Option<aionforge_domain::time::Timestamp>,
    ) -> Episode {
        Episode {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
                namespace,
                expired_at,
            },
            stats: stats(),
            content: content.to_string(),
            role: Role::User,
            captured_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
            agent_id: Id::generate(),
            session_id: None,
            content_hash: ContentHash::of(content.as_bytes()),
            embedding: None,
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        }
    }

    fn fact(statement: &str, expired_at: Option<aionforge_domain::time::Timestamp>) -> Fact {
        fact_in(statement, Namespace::Global, expired_at)
    }

    fn fact_in(
        statement: &str,
        namespace: Namespace,
        expired_at: Option<aionforge_domain::time::Timestamp>,
    ) -> Fact {
        Fact {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
                namespace,
                expired_at,
            },
            stats: stats(),
            subject_id: Id::from_content_hash(statement.as_bytes()),
            predicate: "asserts".to_string(),
            object: ObjectValue::Text(statement.to_string()),
            confidence: 0.9,
            status: FactStatus::Active,
            statement: statement.to_string(),
            embedding: None,
            embedder_model: None,
            extraction: None,
            cooled_until: None,
        }
    }

    fn work_item(title: &str, namespace: Namespace, status: WorkStatus) -> WorkItem {
        WorkItem {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
                namespace,
                expired_at: None,
            },
            title: title.to_string(),
            body: None,
            level: "task".to_string(),
            work_status: status,
            parent_id: None,
            ordinal: 0,
        }
    }

    #[test]
    fn memory_counts_excludes_expired_and_nonmemory_labels() {
        let store = store();
        let expired = ts("2026-06-07T12:00:00-05:00[America/Chicago]");

        // Two live + one soft-forgotten Episode (the row survives, `expired_at` stamped).
        store
            .insert_episode(&episode("ep-live-1", None))
            .expect("insert live episode 1");
        store
            .insert_episode(&episode("ep-live-2", None))
            .expect("insert live episode 2");
        store
            .insert_episode(&episode("ep-gone", Some(expired.clone())))
            .expect("insert expired episode");

        // One live + one soft-forgotten Fact: the NON-Episode liveness coverage.
        store
            .insert_fact(&fact("fact-live", None))
            .expect("insert live fact");
        store
            .insert_fact(&fact("fact-gone", Some(expired)))
            .expect("insert expired fact");

        let counts = store.memory_counts().expect("memory counts");

        // Expired rows are excluded on BOTH the Episode and the Fact label.
        assert_eq!(counts.episodes, 2, "only the two live episodes count");
        assert_eq!(counts.facts, 1, "the soft-forgotten fact is excluded");
        // Kinds with no inserted nodes are zero: non-memory labels never leak in.
        assert_eq!(counts.entities, 0);
        assert_eq!(counts.notes, 0);
        assert_eq!(counts.skills, 0);
        assert_eq!(counts.bad_patterns, 0);
        // total() is the consistent sum of the per-kind fields.
        assert_eq!(counts.total(), 3);
        assert_eq!(
            counts.total(),
            counts.episodes
                + counts.facts
                + counts.entities
                + counts.notes
                + counts.skills
                + counts.bad_patterns
        );
    }

    #[test]
    fn namespace_counts_are_live_zero_filled_and_consistent_with_global_counts() {
        let store = store();
        let expired = ts("2026-06-07T12:00:00-05:00[America/Chicago]");
        let alice = Namespace::Agent("alice".to_string());
        let team = Namespace::Team("squad".to_string());
        let empty = Namespace::Agent("empty".to_string());

        store
            .insert_episode(&episode_in("alice-live", alice.clone(), None))
            .expect("alice episode");
        store
            .insert_episode(&episode_in(
                "alice-expired",
                alice.clone(),
                Some(expired.clone()),
            ))
            .expect("expired alice episode");
        store
            .insert_fact(&fact_in("team fact", team.clone(), None))
            .expect("team fact");
        store
            .insert_fact(&fact_in(
                "global expired fact",
                Namespace::Global,
                Some(expired),
            ))
            .expect("expired global fact");
        store
            .save_work_item(&work_item("alice todo", alice.clone(), WorkStatus::Todo))
            .expect("alice work");
        store
            .save_work_item(&work_item("team done", team.clone(), WorkStatus::Done))
            .expect("team work");

        let namespaces = vec![
            Namespace::Global,
            alice.clone(),
            team.clone(),
            empty.clone(),
        ];
        let memory_by_namespace = store
            .memory_counts_by_namespace(&namespaces)
            .expect("memory census");
        let work_by_namespace = store
            .work_counts_by_namespace(&namespaces)
            .expect("work census");

        assert_eq!(
            memory_by_namespace
                .iter()
                .map(|(_, counts)| counts.total())
                .sum::<u64>(),
            store.memory_counts().expect("global memory counts").total(),
            "namespace totals sum to store-wide live memories"
        );
        assert_eq!(
            work_by_namespace
                .iter()
                .map(|(_, counts)| counts.total())
                .sum::<u64>(),
            store.work_counts().expect("global work counts").total(),
            "namespace totals sum to store-wide live work items"
        );
        assert_eq!(memory_by_namespace[1].0, alice);
        assert_eq!(memory_by_namespace[1].1.episodes, 1);
        assert_eq!(memory_by_namespace[2].0, team);
        assert_eq!(memory_by_namespace[2].1.facts, 1);
        assert_eq!(memory_by_namespace[3].0, empty);
        assert_eq!(memory_by_namespace[3].1.total(), 0);
        assert_eq!(work_by_namespace[1].1.todo, 1);
        assert_eq!(work_by_namespace[2].1.done, 1);
        assert_eq!(work_by_namespace[3].1.total(), 0);
    }

    #[test]
    fn live_memory_nodes_in_namespaces_filters_expired_and_deduplicates_overlaps() {
        let store = store();
        let expired = ts("2026-06-07T12:00:00-05:00[America/Chicago]");
        let alice = Namespace::Agent("alice".to_string());
        store
            .insert_episode(&episode_in("alice-live", alice.clone(), None))
            .expect("live episode");
        store
            .insert_episode(&episode_in("alice-expired", alice.clone(), Some(expired)))
            .expect("expired episode");
        store
            .insert_fact(&fact_in("alice fact", alice.clone(), None))
            .expect("live fact");

        let nodes = store
            .live_memory_nodes_in_namespaces(
                &[Episode::LABEL, Fact::LABEL],
                &[alice.clone(), alice],
            )
            .expect("live nodes");
        assert_eq!(
            nodes.len(),
            2,
            "one live episode + one live fact, no expired row or duplicate"
        );
    }
}
