//! The hard-purge write primitive (05 §3, M5.T03) — the **only** destructive write in
//! the system, by design and by inspection: the selene delete APIs appear nowhere in
//! the workspace outside this file, so invariant 13.2 ("only the erasure path
//! destroys") is checkable by one grep.
//!
//! `hard_purge` consumes a closure the read half already computed and bounded
//! ([`crate::PurgeClosure`]) and destroys it in **one transaction**: every node, every
//! incident edge of every type in both directions (the substrate's node delete cascades
//! them and self-maintains the property, composite, text, and vector indexes in the
//! same write), with the caller's `Purge` audit co-committed through the
//! [`crate::audit::ensure_event`] funnel. The whole set is validated before the first
//! row is removed, so a partial cascade is never observable — the purge either happens
//! entirely or not at all.
//!
//! Idempotency is probed under the write lock, the lifecycle-write discipline: a stale
//! closure whose members are already dead is a no-op that emits no audit row, and a
//! partially-dead closure deletes the surviving members. Deliberately **no `AUDIT`
//! edge** is wired: the subject is in the closure, so an edge to it would be severed by
//! the very deletion it documents — the purge audit is reachable by its `subject_id`
//! property instead, which is how the audit-by-subject reads key every lookup.
//!
//! `ATTESTED_BY` removal falls out of the node deletion — the spec's "attestation edges
//! removed only on hard purge" — and shared entities and every existing audit row
//! survive untouched: neither is ever a member of the closure.

use std::collections::{BTreeSet, HashSet};

use aionforge_domain::nodes::forensic::AuditEvent;
use selene_core::EdgeId;

use crate::error::StoreError;
use crate::store::Store;
use crate::{NodeId, audit};

/// The outcome of a hard purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeWrite {
    /// The closure was destroyed and the purge audit co-committed.
    Applied {
        /// Closure members that were live and are now gone.
        deleted_nodes: usize,
        /// Distinct edges severed — incident edges of every label, both directions,
        /// closure-internal edges counted once.
        deleted_edges: usize,
    },
    /// Every closure member was already dead. Nothing was written and no audit row was
    /// emitted — a replay of an applied purge converges instead of minting a second
    /// event.
    Noop,
}

impl Store {
    /// Destroy a computed erasure closure in one transaction and co-commit the caller's
    /// `Purge` audit (05 §3, M5.T03).
    ///
    /// The caller supplies the closure the read half computed (already bounded by the
    /// cascade caps) and the audit event addressed to the erasure seed. Already-dead
    /// members are skipped; an entirely-dead closure is a [`PurgeWrite::Noop`] with no
    /// audit row.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a closure member was never a node, or a read, write,
    /// or commit fails. The set is validated before the first deletion, so an error
    /// leaves the graph untouched.
    pub fn hard_purge(
        &self,
        closure: &[NodeId],
        audit_event: &AuditEvent,
    ) -> Result<PurgeWrite, StoreError> {
        let mut txn = self.graph().begin_write();
        let outcome = {
            let mut mutator = txn.mutator();
            // Probe under the write lock: which members are still live, and every
            // distinct incident edge they carry (severed by the cascade; counted here
            // because the substrate's delete reports nothing back).
            let (live, edge_count) = {
                let graph = mutator.read();
                let mut live: BTreeSet<NodeId> = BTreeSet::new();
                let mut edges: HashSet<EdgeId> = HashSet::new();
                for &node in closure {
                    if graph.node_properties(node).is_none() {
                        continue;
                    }
                    live.insert(node);
                    for adjacency in [graph.incoming_edges(node), graph.outgoing_edges(node)]
                        .into_iter()
                        .flatten()
                    {
                        for edge in adjacency.iter() {
                            edges.insert(edge.edge_id);
                        }
                    }
                }
                (live, edges.len())
            };
            if live.is_empty() {
                PurgeWrite::Noop
            } else {
                let deleted_nodes = live.len();
                mutator.delete_elements(live, BTreeSet::new())?;
                let _ = audit::ensure_event(&mut mutator, audit_event, self.audit_signer())?;
                PurgeWrite::Applied {
                    deleted_nodes,
                    deleted_edges: edge_count,
                }
            }
        };
        txn.commit()?;
        Ok(outcome)
    }
}
