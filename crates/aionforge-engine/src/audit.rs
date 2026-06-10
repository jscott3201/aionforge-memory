//! The agent-facing audit read facade (M4.T06 PR-5f): the L0 audit readers exposed on
//! [`Memory`], namespace-scoped and signature-verified per row.
//!
//! This is the reachable verified read path 06 §6 requires before audit signing can go
//! live: each returned event carries its verification outcome, and verification is a
//! per-row fact, never a query error — a tampered row must surface as `Invalid`, not
//! make a subject's history unreadable.
//!
//! ## Scoping
//!
//! Reads are gated by the same [`VisibleSet`](aionforge_domain::authz::VisibleSet) rule
//! as every other read surface (06 §1,
//! M4.T01): `global` plus the principal's own private and team namespaces; `system` is
//! never agent-visible. Governance audits live in the `system` namespace, so an agent
//! sees its own capture-channel history but not substrate governance forensics — those
//! remain the host's path through the L0 [`Store`](aionforge_store::Store) readers
//! (the M4.T06 PR-5h CLI surface). Filtering happens above the keyset pagination, so a
//! page is refilled until `limit` visible rows are found or the history is exhausted —
//! cursors stay valid across mixed-visibility ranges.
//!
//! ## Verification
//!
//! [`AuditVerification`] keeps "the substrate did not check" structurally distinct from
//! every checked outcome: until audit signing is wired (PR-5g builds the verifier from
//! the keyring), rows read back [`AuditVerification::NotEnabled`] — never a fabricated
//! "unsigned", which is a *checked* claim about a signature-bearing store.

use aionforge_domain::authz::Principal;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_store::{AuditCursor, AuditHistory, MAX_AUDIT_PAGE};
use aionforge_trust::AuditStatus;

use crate::{EngineError, Memory};

/// A row's signature-verification outcome on the read path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditVerification {
    /// Audit signing is not enabled on this instance — the row was not checked. Distinct
    /// from [`AuditStatus::Unsigned`], which is a checked claim about a stored signature.
    NotEnabled,
    /// The configured verifier's per-row verdict (total — never a query error).
    Checked(AuditStatus),
}

/// One audit event with its verification outcome.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    /// The stored event, decoded.
    pub event: AuditEvent,
    /// Whether and how the row's signature verified.
    pub verification: AuditVerification,
}

/// One namespace-scoped, verification-mapped page of audit history.
#[derive(Debug, Clone)]
pub struct AuditPage {
    /// The visible rows, oldest first, at most the requested `limit`.
    pub records: Vec<AuditRecord>,
    /// The keyset cursor for the next page, `None` when the history is exhausted.
    pub next: Option<AuditCursor>,
}

impl<E: Embedder> Memory<E> {
    /// A subject's audit history visible to `principal`, across all kinds, oldest first —
    /// the 06 §6 by-subject lookup on the agent-facing surface.
    ///
    /// # Errors
    /// Returns [`EngineError`] if the underlying read fails.
    pub fn audit_history(
        &self,
        principal: &Principal,
        subject_id: &Id,
        after: Option<&AuditCursor>,
        limit: usize,
    ) -> Result<AuditPage, EngineError> {
        self.scoped_audit_page(principal, after, limit, |cursor, page| {
            Ok(self.store().audit_history(subject_id, cursor, page)?)
        })
    }

    /// Every audit event of `kind` visible to `principal`, oldest first — the 06 §6
    /// "scoped to audit kinds" axis on the agent-facing surface.
    ///
    /// # Errors
    /// Returns [`EngineError`] if the underlying read fails.
    pub fn audit_by_kind(
        &self,
        principal: &Principal,
        kind: AuditKind,
        after: Option<&AuditCursor>,
        limit: usize,
    ) -> Result<AuditPage, EngineError> {
        self.scoped_audit_page(principal, after, limit, |cursor, page| {
            Ok(self.store().audit_by_kind(kind, cursor, page)?)
        })
    }

    /// A subject's audit history narrowed to one `kind`, visible to `principal`.
    ///
    /// # Errors
    /// Returns [`EngineError`] if the underlying read fails.
    pub fn audit_by_subject_kind(
        &self,
        principal: &Principal,
        subject_id: &Id,
        kind: AuditKind,
        after: Option<&AuditCursor>,
        limit: usize,
    ) -> Result<AuditPage, EngineError> {
        self.scoped_audit_page(principal, after, limit, |cursor, page| {
            Ok(self
                .store()
                .audit_by_subject_kind(subject_id, kind, cursor, page)?)
        })
    }

    /// Drive one L0 keyset reader into a visibility-scoped, verification-mapped page.
    ///
    /// Filtering happens above the L0 pagination, so the inner reader is re-driven until
    /// `limit` visible rows accumulate or the history is exhausted: hidden rows advance
    /// the cursor without shortening the page. The keyset cursor is strictly monotone
    /// over a finite history, so the refill loop terminates.
    fn scoped_audit_page(
        &self,
        principal: &Principal,
        after: Option<&AuditCursor>,
        limit: usize,
        read: impl Fn(Option<&AuditCursor>, usize) -> Result<AuditHistory, EngineError>,
    ) -> Result<AuditPage, EngineError> {
        let limit = limit.clamp(1, MAX_AUDIT_PAGE);
        let visible = self.authorizer().visible_namespaces(principal);
        let mut records = Vec::with_capacity(limit);
        let mut cursor = after.cloned();
        loop {
            let history = read(cursor.as_ref(), limit)?;
            let exhausted = history.next.is_none();
            let mut events = history.events.into_iter();
            for event in events.by_ref() {
                // Track the keyset position of every row we CONSUME (visible or hidden),
                // so a continuation never re-reads a hidden range or skips a visible row.
                cursor = Some(AuditCursor::of(&event));
                if !visible.contains(&event.identity.namespace) {
                    continue;
                }
                records.push(self.verified(event));
                if records.len() == limit {
                    // The page is full. Report a continuation only when a row can
                    // actually follow — a remainder on this L0 page or a further L0
                    // page — so a page that exactly exhausts the history reads
                    // `next: None`, matching the L0 paginate contract.
                    let more = events.next().is_some() || !exhausted;
                    return Ok(AuditPage {
                        records,
                        next: if more { cursor } else { None },
                    });
                }
            }
            if exhausted {
                return Ok(AuditPage {
                    records,
                    next: None,
                });
            }
        }
    }

    /// Map one stored event to its verification outcome (total, per row).
    fn verified(&self, event: AuditEvent) -> AuditRecord {
        let verification = match self.audit_verifier() {
            None => AuditVerification::NotEnabled,
            Some(verifier) => AuditVerification::Checked(verifier.status(&event)),
        };
        AuditRecord {
            event,
            verification,
        }
    }
}
