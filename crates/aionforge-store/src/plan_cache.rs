//! Per-store shared GQL plan caches and their read-only stats.
//!
//! Every store request opens a fresh [`Session`](selene_gql::Session) over the one
//! process-lifetime graph and runs fixed, parameter-bound source strings (capture,
//! recall, RRF, pagerank, dedup, …). selene 1.2 lets short-lived sessions share one
//! parse/lower cache per graph: this module builds that pair once per
//! [`Store`] so the same plans are reused across the many requests
//! instead of being re-parsed and re-lowered on every call.

use std::num::NonZeroUsize;
use std::sync::Arc;

use selene_gql::{CallPlanCache, SharedPlanCache};
use serde::{Deserialize, Serialize};

use crate::store::Store;

/// Entry capacity for each per-store plan cache.
///
/// The store issues a small, fixed set of distinct source strings, so a few hundred
/// entries hold every plan with headroom. The caches are keyed by graph id, schema
/// epoch, registry version, and source text, so a schema migration invalidates stale
/// plans rather than serving them.
const PLAN_CACHE_CAPACITY: usize = 256;

/// Build the shared non-CALL and CALL plan caches for one store.
pub(crate) fn new_plan_caches() -> (Arc<SharedPlanCache>, Arc<CallPlanCache>) {
    let capacity = NonZeroUsize::new(PLAN_CACHE_CAPACITY).expect("plan cache capacity is nonzero");
    (
        Arc::new(SharedPlanCache::new(capacity)),
        Arc::new(CallPlanCache::new(capacity)),
    )
}

/// A read-only snapshot of a store's shared plan-cache counters, for tests, the
/// doctor, and metrics. The hit counters grow as repeated request sessions reuse a
/// lowered plan instead of re-parsing the same fixed source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCacheStatsReport {
    /// Shared non-CALL source-plan cache hits.
    pub source_plan_hits: u64,
    /// Shared non-CALL source-plan cache misses.
    pub source_plan_misses: u64,
    /// Shared procedure-CALL plan cache hits.
    pub call_plan_hits: u64,
    /// Shared procedure-CALL plan cache misses.
    pub call_plan_misses: u64,
}

impl Store {
    /// A snapshot of this store's shared plan-cache counters.
    #[must_use]
    pub fn plan_cache_stats(&self) -> PlanCacheStatsReport {
        let source = self.shared_plan_cache.stats();
        let call = self.call_plan_cache.stats();
        PlanCacheStatsReport {
            source_plan_hits: source.hits,
            source_plan_misses: source.misses,
            call_plan_hits: call.hits,
            call_plan_misses: call.misses,
        }
    }
}
