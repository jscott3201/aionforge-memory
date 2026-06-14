//! The work-tracking tier: a general work item and a cross-cutting classification tag
//! (work-structure design, 2026-06-14).
//!
//! [`WorkItem`] is the substrate's primitive for *what an agent is doing*: a unit of work
//! at a caller-defined `level` (e.g. `milestone` / `iteration` / `task` / `subtask`, but
//! any vocabulary a harness chooses — `book` → `chapter` → `section`, `study` →
//! `experiment` → `run`), wired into a hierarchy by a self-referential `parent_id` scalar
//! and advanced through a small `work_status` lifecycle. It is deliberately NOT a
//! retrievable memory: it carries only the [`Identity`] block (no [`crate::blocks::Stats`]),
//! so it is exempt from decay/forgetting by construction — an active plan must never be
//! summarized away — earning that exemption the way the control/anchor kinds do, by absence
//! from the maintenance scan sets. Its lifecycle history lives in the signed audit trail,
//! not in version nodes.
//!
//! [`Tag`] is the orthogonal classification facet: a small, content-addressed,
//! controlled-vocabulary label any kind can point at via the `HAS_TAG` edge. Tags are the
//! cross-cutting (horizontal) axis; the work tree's `parent_id` is the containment
//! (vertical) axis — neither subsumes the other.

use serde::{Deserialize, Serialize};

use crate::blocks::Identity;
use crate::ids::Id;

/// The lifecycle status of a [`WorkItem`] (work-structure design §2).
///
/// A small, substrate-owned state machine: work is `todo` until started, `in_progress`
/// while active, `blocked` when it cannot proceed, and terminal at `done` or `dropped`.
/// The storage layer applies the DB `DEFAULT 'todo'`; [`Default`] mirrors it for in-Rust
/// construction. Stored as the bare snake_case discriminant under the `work_status`
/// property — deliberately NOT `status`, which the forgetting layer reads as a
/// [`crate::nodes::semantic::FactStatus`] and refuses when non-`active`, so a work item's
/// lifecycle sits on its own orthogonal axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    /// Not yet started.
    #[default]
    Todo,
    /// Actively being worked.
    InProgress,
    /// Cannot proceed — waiting on a dependency, a decision, or an external event.
    Blocked,
    /// Completed.
    Done,
    /// Abandoned without completion.
    Dropped,
}

/// A unit of work an agent is doing: the general work-tracking primitive (work-structure
/// design §2).
///
/// Harness-agnostic by design — `level` is a free caller-defined label, not a closed enum,
/// so a coding agent's `milestone → iteration → task → subtask` and a research or writing
/// agent's own hierarchy use the same primitive without recompiling the substrate. The
/// tree is expressed by the indexed self-referential `parent_id` scalar (a containment
/// pointer, not an edge), ordered among siblings by `ordinal`. A work item carries only the
/// [`Identity`] block — it is not a retrievable memory, takes no [`crate::blocks::Stats`],
/// and is exempt from decay/forgetting by its absence from the maintenance scan sets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Shared identity block.
    pub identity: Identity,
    /// Short human-facing title of the unit of work.
    pub title: String,
    /// Optional free-text detail or nuance.
    pub body: Option<String>,
    /// The caller-defined hierarchy level (e.g. `milestone` / `task`, or any harness
    /// vocabulary). Open by design — never a closed enum.
    pub level: String,
    /// The lifecycle status.
    pub work_status: WorkStatus,
    /// The parent work item this is contained by; `None` for a root. The indexed
    /// self-referential containment spine.
    pub parent_id: Option<Id>,
    /// Sibling order under `parent_id`; lower sorts first.
    pub ordinal: u64,
}

impl WorkItem {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "WorkItem";
}

/// A cross-cutting classification label any memory or work item can carry (work-structure
/// design §3).
///
/// The horizontal, many-to-many classification axis, orthogonal to the work tree's vertical
/// `parent_id` containment. A `Tag` is content-addressed over its `(namespace, slug)` so the
/// same slug in a namespace dedups to one node — a curated controlled vocabulary, not
/// free-text sprawl — and kinds point at it via the `HAS_TAG` edge. Carries only the
/// [`Identity`] block: it is metadata, not a retrievable memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tag {
    /// Shared identity block.
    pub identity: Identity,
    /// The canonical, normalized tag key (e.g. `auth`, `pr6`, `blocked-on-owner`).
    pub slug: String,
    /// Optional human-facing display form; `None` falls back to the slug.
    pub display: Option<String>,
}

impl Tag {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Tag";
}
