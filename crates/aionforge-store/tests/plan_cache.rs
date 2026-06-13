//! The shared plan caches reuse lowered plans across short-lived request sessions
//! without changing any observable result.

use aionforge_store::{BoundQuery, QueryResult, Store, Value};

fn now() -> jiff::Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn row_count(result: QueryResult) -> usize {
    match result {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("unexpected query result: {other:?}"),
    }
}

#[test]
fn repeated_executes_reuse_the_shared_source_plan() {
    let store = Store::open_in_memory_migrated(&now()).expect("open and migrate");

    // A distinctive fixed source the migration never runs, so its first plan is a fresh
    // miss regardless of what migration cached. Every caller value is parameter-bound.
    let query = BoundQuery::new("MATCH (e:Episode) WHERE e.importance > $t RETURN count(e) AS n")
        .bind("t", Value::Float(0.25))
        .expect("bind threshold");

    let baseline = store.plan_cache_stats().source_plan_hits;
    let first = store.execute(&query).expect("first execute");
    let after_first = store.plan_cache_stats().source_plan_hits;
    let second = store.execute(&query).expect("second execute");
    let after_second = store.plan_cache_stats().source_plan_hits;

    assert_eq!(
        after_first, baseline,
        "a brand-new fixed source is a cache miss, not a hit"
    );
    assert!(
        after_second > after_first,
        "the second identical execute reuses the cached lowered plan (hits {baseline} -> {after_second})"
    );

    // The cache is invisible to results: both executes return the same rows.
    assert_eq!(
        row_count(first),
        row_count(second),
        "the cached plan returns an identical result"
    );
}
