//! The L0 read surface for the audit subgraph (06 §6, M4.T06).
//!
//! A subject's full audit history, by-kind scoping, and by-(subject, kind), each ordered by
//! `(occurred_at, id)` and paginated with a keyset cursor. Three things shape the design:
//!
//! - **The spine is the `subject_id` (or `kind`) scalar index, not the `AUDIT` edge.** The edge
//!   is best-effort enrichment: [`Store::commit_audit`](crate::Store::commit_audit) wires no edge
//!   (a `namespace_denied` rejection has no subject node), and `record_reliability_update` skips
//!   the edge when the agent node is absent. `subject_id` is on *every* event and is indexed, so
//!   an edge walk would silently miss events that the index read returns.
//! - **Ordering is an in-Rust `(occurred_at, id)` sort, not GQL `ORDER BY`.** On the pinned
//!   substrate, `ORDER BY occurred_at` while projecting a different column is not honored, and the
//!   reader reads full node property maps anyway (not a projection). `occurred_at` compares by
//!   absolute instant (a zoned `Timestamp`); `id` is the deterministic tiebreak.
//! - **Pagination is a keyset cursor over `(occurred_at, id)`, not an offset.** Continuation is by
//!   ordering key, so it is stable under concurrent appends and a host-clock regression cannot
//!   break a continuation. [`MAX_AUDIT_PAGE`] caps a page to bound the result and token budget.
//!
//! These are committed-graph reads, so they run off any cursor and add no node, edge, or index —
//! [PR-1](crate) registered `AuditEvent.occurred_at` / `actor_id` and the temporal composites.

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use selene_core::{Value, db_string};
use selene_graph::RowIndex;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::convert::{enum_value, id_value};
use crate::error::StoreError;
use crate::store::Store;

/// The `AuditEvent` column probed for a by-subject read.
pub(crate) const SUBJECT_ID: &str = "subject_id";
/// The `AuditEvent` column probed for a by-kind read.
const KIND: &str = "kind";

/// The hard cap on a single audit page, bounding the result set and the token budget. A request
/// for more is clamped to this; a request for zero is raised to one.
pub const MAX_AUDIT_PAGE: usize = 200;

/// A keyset pagination cursor over the `(occurred_at, id)` ordering.
///
/// Continuation is by ordering key, not by offset, so a page boundary is stable under concurrent
/// appends. It is the `(occurred_at, id)` of the last event on a page; the next page returns
/// events strictly greater than it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditCursor {
    /// Event time of the last event on the prior page.
    pub occurred_at: Timestamp,
    /// Id of the last event on the prior page — the tiebreak within one instant.
    pub id: Id,
}

impl AuditCursor {
    /// The cursor pointing at a given event.
    fn of(event: &AuditEvent) -> Self {
        Self {
            occurred_at: event.occurred_at.clone(),
            id: event.identity.id,
        }
    }
}

/// A page of audit history: the events, oldest first, plus the continuation cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditHistory {
    /// The events on this page, ordered `(occurred_at, id)` ascending.
    pub events: Vec<AuditEvent>,
    /// The cursor to pass as `after` for the next page, or `None` when the history is exhausted.
    pub next: Option<AuditCursor>,
}

/// `true` when `event` sorts strictly after `cursor` under the `(occurred_at, id)` order.
fn is_after(event: &AuditEvent, cursor: &AuditCursor) -> bool {
    (&event.occurred_at, &event.identity.id) > (&cursor.occurred_at, &cursor.id)
}

/// Drop everything up to and including `after`, then take one page, reporting a continuation
/// cursor only when at least one event remains beyond the page.
fn paginate(
    mut events: Vec<AuditEvent>,
    after: Option<&AuditCursor>,
    limit: usize,
) -> AuditHistory {
    let limit = limit.clamp(1, MAX_AUDIT_PAGE);
    if let Some(cursor) = after {
        // `events` is sorted ascending, so the `<= cursor` prefix is contiguous.
        let start = events.partition_point(|event| !is_after(event, cursor));
        events.drain(..start);
    }
    let has_more = events.len() > limit;
    events.truncate(limit);
    let next = has_more.then(|| AuditCursor::of(events.last().expect("a full page has a last")));
    AuditHistory { events, next }
}

impl Store {
    /// A subject's full audit history — every `AuditEvent` whose `subject_id` is `subject_id`,
    /// across all kinds, oldest first, one keyset page at a time.
    ///
    /// This is the spec's "dedicated by-subject lookup that returns a subject's full audit
    /// history" (06 §6). Pass `after = None` for the first page and the returned
    /// [`AuditHistory::next`] for each subsequent one.
    ///
    /// # Errors
    /// Returns [`StoreError`] if an event row cannot be decoded.
    pub fn audit_history(
        &self,
        subject_id: &Id,
        after: Option<&AuditCursor>,
        limit: usize,
    ) -> Result<AuditHistory, StoreError> {
        let events = self.audit_events_eq(SUBJECT_ID, &id_value(subject_id)?, None)?;
        Ok(paginate(events, after, limit))
    }

    /// Every `AuditEvent` of a given `kind`, oldest first, one keyset page at a time — the
    /// "scoped to audit kinds" axis (06 §6). Probes the `kind` index directly.
    ///
    /// # Errors
    /// Returns [`StoreError`] if an event row cannot be decoded.
    pub fn audit_by_kind(
        &self,
        kind: AuditKind,
        after: Option<&AuditCursor>,
        limit: usize,
    ) -> Result<AuditHistory, StoreError> {
        let events = self.audit_events_eq(KIND, &enum_value(&kind)?, None)?;
        Ok(paginate(events, after, limit))
    }

    /// A subject's audit history narrowed to one `kind` — the by-subject spine with an in-memory
    /// `kind` filter (the subject axis is the more selective index).
    ///
    /// # Errors
    /// Returns [`StoreError`] if an event row cannot be decoded.
    pub fn audit_by_subject_kind(
        &self,
        subject_id: &Id,
        kind: AuditKind,
        after: Option<&AuditCursor>,
        limit: usize,
    ) -> Result<AuditHistory, StoreError> {
        let events = self.audit_events_eq(SUBJECT_ID, &id_value(subject_id)?, Some(kind))?;
        Ok(paginate(events, after, limit))
    }

    /// How many audit events a subject has — counts index rows without decoding them, for a
    /// "N events total" header alongside a page.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the label or property name cannot be encoded.
    pub fn audit_count_for_subject(&self, subject_id: &Id) -> Result<usize, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(AuditEvent::LABEL)?;
        let prop = db_string(SUBJECT_ID)?;
        let value = id_value(subject_id)?;
        Ok(snapshot
            .nodes_with_property_eq(&label, &prop, &value)
            .map_or(0, |rows| rows.iter().count()))
    }

    /// Probe `AuditEvent.<prop> == value`, decode each match (optionally keeping one `kind`), and
    /// sort by `(occurred_at, id)` ascending. The shared spine under every audit reader and the
    /// reliability fold's event log; sorting is harmless to the order-independent fold.
    ///
    /// # Errors
    /// Returns [`StoreError`] if an event row cannot be decoded.
    pub(crate) fn audit_events_eq(
        &self,
        prop: &str,
        value: &Value,
        kind: Option<AuditKind>,
    ) -> Result<Vec<AuditEvent>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(AuditEvent::LABEL)?;
        let prop = db_string(prop)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, value) else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let event = audit::from_properties(props)?;
            if kind.is_none_or(|wanted| event.kind == wanted) {
                events.push(event);
            }
        }
        events.sort_by(|a, b| {
            (&a.occurred_at, &a.identity.id).cmp(&(&b.occurred_at, &b.identity.id))
        });
        Ok(events)
    }
}
