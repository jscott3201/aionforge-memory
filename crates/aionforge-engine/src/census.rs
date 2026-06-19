//! Principal-scoped memory census readers.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Role;
use aionforge_store::{MemoryCounts, WorkCounts};

use crate::{EngineError, Memory, Principal, ResolvedMemory};

/// Per-namespace memory and work counts visible to a principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceCensus {
    /// Namespace whose buckets were counted.
    pub namespace: Namespace,
    /// Live memory counts by kind.
    pub memories: MemoryCounts,
    /// Live work-item counts by status.
    pub work_items: WorkCounts,
}

/// A principal-scoped census grouped by namespace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryCensusReport {
    /// Namespaces included in the census, in visible-set order.
    pub namespaces: Vec<NamespaceCensus>,
}

impl<E: Embedder> Memory<E> {
    /// Count live memories and work items by namespace for a principal.
    ///
    /// `include_system` is an opt-in request; the system namespace is included only when the
    /// active authorizer also grants [`Authorizer::may_surface_system`](crate::Authorizer::may_surface_system).
    /// If `namespace` is provided but is outside the principal's visible set, the result is empty
    /// rather than an error or existence oracle.
    ///
    /// # Errors
    /// Returns [`EngineError`] if the underlying store read fails.
    pub fn memory_census_counts(
        &self,
        principal: &Principal,
        include_system: bool,
        namespace: Option<Namespace>,
    ) -> Result<MemoryCensusReport, EngineError> {
        let (surface_system, namespaces) = self.census_scope(principal, include_system, namespace);
        let memory_counts = self
            .store
            .memory_counts_with_system_role_episodes_by_namespace(&namespaces)?;
        let work_counts = self.store.work_counts_by_namespace(&namespaces)?;
        let namespaces = memory_counts
            .into_iter()
            .zip(work_counts)
            .map(|(memory_counts, (work_namespace, work_items))| {
                let mut memories = memory_counts.memories;
                if !surface_system {
                    memories.episodes = memories
                        .episodes
                        .saturating_sub(memory_counts.system_role_episodes);
                }
                NamespaceCensus {
                    namespace: debug_assert_same_namespace(memory_counts.namespace, work_namespace),
                    memories,
                    work_items,
                }
            })
            .collect();
        Ok(MemoryCensusReport { namespaces })
    }

    /// List live visible memories for a principal, restricted to labels and an optional namespace.
    ///
    /// As with [`Memory::memory_census_counts`], an out-of-scope explicit namespace returns an
    /// empty list. The store performs the indexed namespace scan; the engine owns the visible-set
    /// narrowing before that scan.
    ///
    /// # Errors
    /// Returns [`EngineError`] if the underlying store read or decode fails.
    pub fn memory_census_records(
        &self,
        principal: &Principal,
        include_system: bool,
        namespace: Option<Namespace>,
        labels: &[&str],
    ) -> Result<Vec<ResolvedMemory>, EngineError> {
        if labels.is_empty() {
            return Ok(Vec::new());
        }
        let (surface_system, namespaces) = self.census_scope(principal, include_system, namespace);
        let nodes = self
            .store
            .live_memory_nodes_in_namespaces(labels, &namespaces)?;
        let mut records = Vec::new();
        for node in nodes {
            if let Some(record) = self.store.resolved_memory_by_node_id(node, labels)?
                && record.identity().expired_at.is_none()
                && (surface_system
                    || !matches!(&record, ResolvedMemory::Episode(episode) if episode.role == Role::System))
            {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn census_scope(
        &self,
        principal: &Principal,
        include_system: bool,
        namespace: Option<Namespace>,
    ) -> (bool, Vec<Namespace>) {
        let surface_system = include_system && self.authorizer().may_surface_system(principal);
        let mut visible = self.authorizer().visible_namespaces(principal);
        if surface_system {
            visible = visible.with_system();
        }
        let namespaces = match namespace {
            Some(namespace) if visible.contains(&namespace) => vec![namespace],
            Some(_) => Vec::new(),
            None => visible.namespaces(),
        };
        (surface_system, namespaces)
    }
}

fn debug_assert_same_namespace(left: Namespace, right: Namespace) -> Namespace {
    debug_assert_eq!(left, right, "store count readers preserve namespace order");
    left
}
