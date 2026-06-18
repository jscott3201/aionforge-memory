//! Recovery-time catalog-index reconciliation: idempotently CREATE any catalog index a
//! recovered graph is MISSING (data-model §7–§8).
//!
//! [`Store::recover`] replays a persisted schema and rebuilds its indexes, but the
//! version-guarded [`Store::migrate`](crate::Store::migrate) only registers indexes on a
//! FRESH migration — a store first migrated under an older catalog never gains an index
//! added to the catalog later. This module closes that gap: on open it creates every
//! MISSING catalog index across all four classes (vector, text, scalar, composite) and
//! reports what it created, so a catalog addition converges on the next open instead of
//! forcing a fresh store (the same "remove the greenfield tax" intent as the vector-kind
//! reconciliation, extended from drift-of-existing to missing-entirely).
//!
//! It is the create-side companion to
//! [`Store::reconcile_vector_index_kinds`](crate::Store::reconcile_vector_index_kinds)
//! (which only converges a KIND drift of an index that already exists). Both are non-lossy
//! (selene backfills a created index from the primary data) and idempotent (a second open
//! creates nothing and returns an empty report).
//!
//! DRY: this does NOT iterate the catalogs itself. It reuses the per-class create-if-missing
//! registrars in `indexes.rs` (`register_vector_indexes` / `register_text_indexes` /
//! `register_property_indexes` / `register_composite_indexes`) — the SAME path the
//! migration uses — and only collects the `(label, property)` pairs they report creating.
//! There is exactly one create path per class, so the migration and recovery cannot drift.

use crate::error::StoreError;
use crate::store::Store;

/// The class of a catalog index, used as a low-cardinality metric label and to tag each
/// created index in the recovery report.
const CLASS_VECTOR: &str = "vector";
const CLASS_TEXT: &str = "text";
const CLASS_SCALAR: &str = "scalar";

/// One catalog index that was created (not merely confirmed) during recovery.
///
/// Returned by [`Store::ensure_catalog_indexes`] so the caller can emit one observability
/// line per creation (the "auto-create + metric" policy that mirrors
/// [`Store::reconcile_vector_index_kinds`](crate::Store::reconcile_vector_index_kinds)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogIndexCreated {
    /// The index class: one of `"vector"`, `"text"`, `"scalar"`, `"composite"`.
    pub class: &'static str,
    /// The indexed node label.
    pub label: String,
    /// The indexed property.
    pub property: String,
}

impl Store {
    /// Create every MISSING catalog index, returning what was created.
    ///
    /// Walks all four catalog classes through the shared `indexes.rs` registrars (the
    /// same create-if-missing path the migration uses) and collects the `(label,
    /// property)` pairs each reports creating. An index that already exists is left
    /// untouched, so a second call on a fully-indexed graph creates nothing and returns
    /// an empty `Vec`.
    ///
    /// Non-lossy: selene rebuilds a newly created index from the primary values (the same
    /// guarantee the migration and the vector-kind reconciliation rely on), so creating
    /// an index never drops or re-derives the underlying rows.
    ///
    /// # Dimension safety
    /// A created vector index is built at `dimension` over the existing primary VECTOR
    /// columns. Callers MUST run this only AFTER
    /// [`Store::dimension_consistency_check`](crate::Store::dimension_consistency_check)
    /// has proven the stored vectors match `dimension`; running it before the check could
    /// build a new index at the wrong dimension on a real embedder change.
    /// (Pre-existing vector indexes are already dimension-checked there; this guards a
    /// freshly created one.)
    ///
    /// # Composite indexes
    /// The composite class is idempotent DDL (`CREATE INDEX IF NOT EXISTS`) that does not
    /// report which (if any) it created, so it is run for its effect but not enumerated in
    /// the returned report.
    ///
    /// # Errors
    /// Returns [`StoreError`] if creating any index fails.
    pub(crate) fn ensure_catalog_indexes(
        &self,
        dimension: u32,
    ) -> Result<Vec<CatalogIndexCreated>, StoreError> {
        let mut created = Vec::new();

        for (label, property) in self.register_vector_indexes(dimension)? {
            created.push(CatalogIndexCreated {
                class: CLASS_VECTOR,
                label,
                property,
            });
        }
        for (label, property) in self.register_text_indexes()? {
            created.push(CatalogIndexCreated {
                class: CLASS_TEXT,
                label,
                property,
            });
        }
        for (label, property) in self.register_property_indexes()? {
            created.push(CatalogIndexCreated {
                class: CLASS_SCALAR,
                label,
                property,
            });
        }
        // Idempotent `CREATE INDEX IF NOT EXISTS` DDL; run for its effect, not enumerated.
        self.register_composite_indexes()?;

        Ok(created)
    }
}

/// Record the recovery-time creation of any missing catalog index: a counter per created
/// index plus a human-readable line, so the create is observable. Mirrors
/// `emit_index_kind_reconciliation` in `store.rs`. Silent when nothing was created.
pub(crate) fn emit_catalog_index_created(created: &[CatalogIndexCreated]) {
    for row in created {
        metrics::counter!(
            "aionforge_store_catalog_index_created_total",
            "class" => row.class,
            "label" => row.label.clone(),
            "property" => row.property.clone(),
        )
        .increment(1);
        tracing::info!(
            class = row.class,
            label = %row.label,
            property = %row.property,
            "created missing catalog index on open (non-lossy, engine-backfilled)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StoreConfig;
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::{ContentHash, Id};
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
    use aionforge_domain::time::Timestamp;
    use selene_core::db_string;

    const DIM: u32 = 4;

    fn now() -> Timestamp {
        "2026-06-06T12:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime")
    }

    /// A migrated in-memory store at a small embedding dimension.
    fn migrated() -> Store {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: DIM,
        })
        .expect("open store");
        store.migrate(&now()).expect("migrate");
        store
    }

    /// True if a vector index over `(label, property)` is present.
    fn has_vector_index(store: &Store, label: &str, property: &str) -> bool {
        store
            .vector_indexes()
            .iter()
            .any(|index| index.label == label && index.property == property)
    }

    /// True if a text index over `(label, property)` is present.
    fn has_text_index(store: &Store, label: &str, property: &str) -> bool {
        store
            .text_indexes()
            .iter()
            .any(|(l, p)| l == label && p == property)
    }

    /// True if a scalar/property index over `(label, property)` is present.
    fn has_property_index(store: &Store, label: &str, property: &str) -> bool {
        store
            .property_indexes()
            .iter()
            .any(|(l, p)| l == label && p == property)
    }

    /// Simulate a catalog index that was added AFTER this store was migrated: drop a live
    /// index, the way an older-catalog store would simply not carry it.
    fn drop_vector(store: &Store, label: &str, property: &str) {
        store
            .graph()
            .drop_vector_index(db_string(label).unwrap(), db_string(property).unwrap())
            .expect("drop vector index");
    }
    fn drop_text(store: &Store, label: &str, property: &str) {
        store
            .graph()
            .drop_text_index(db_string(label).unwrap(), db_string(property).unwrap())
            .expect("drop text index");
    }
    fn drop_property(store: &Store, label: &str, property: &str) {
        store
            .graph()
            .drop_property_index(db_string(label).unwrap(), db_string(property).unwrap())
            .expect("drop property index");
    }

    #[test]
    fn ensure_creates_a_missing_vector_index() {
        let store = migrated();
        drop_vector(&store, "Episode", "embedding_v1");
        assert!(
            !has_vector_index(&store, "Episode", "embedding_v1"),
            "the index is missing before ensure"
        );

        let created = store.ensure_catalog_indexes(DIM).expect("ensure");

        assert_eq!(
            created,
            vec![CatalogIndexCreated {
                class: CLASS_VECTOR,
                label: "Episode".to_owned(),
                property: "embedding_v1".to_owned(),
            }],
            "exactly the dropped vector index is reported created: {created:?}"
        );
        assert!(
            has_vector_index(&store, "Episode", "embedding_v1"),
            "the vector index is recreated"
        );
        // The new index is at the asserted dimension, and the full set is intact.
        assert_eq!(store.vector_indexes().len(), 7, "no index was lost");
        assert!(
            store.vector_indexes().iter().all(|v| v.dimension == DIM),
            "the recreated vector index is at the configured dimension"
        );
    }

    #[test]
    fn ensure_creates_a_missing_text_index() {
        let store = migrated();
        drop_text(&store, "Episode", "content");
        assert!(
            !has_text_index(&store, "Episode", "content"),
            "the text index is missing before ensure"
        );

        let created = store.ensure_catalog_indexes(DIM).expect("ensure");

        assert_eq!(
            created,
            vec![CatalogIndexCreated {
                class: CLASS_TEXT,
                label: "Episode".to_owned(),
                property: "content".to_owned(),
            }],
            "exactly the dropped text index is reported created: {created:?}"
        );
        assert!(
            has_text_index(&store, "Episode", "content"),
            "the text index is recreated"
        );
        assert_eq!(store.text_indexes().len(), 5, "no text index was lost");
    }

    #[test]
    fn ensure_creates_a_missing_scalar_index() {
        let store = migrated();
        // `Fact.status` is a catalog scalar index (`SCALAR_INDEXES`).
        drop_property(&store, "Fact", "status");
        assert!(
            !has_property_index(&store, "Fact", "status"),
            "the scalar index is missing before ensure"
        );

        let created = store.ensure_catalog_indexes(DIM).expect("ensure");

        assert_eq!(
            created,
            vec![CatalogIndexCreated {
                class: CLASS_SCALAR,
                label: "Fact".to_owned(),
                property: "status".to_owned(),
            }],
            "exactly the dropped scalar index is reported created: {created:?}"
        );
        assert!(
            has_property_index(&store, "Fact", "status"),
            "the scalar index is recreated"
        );
    }

    #[test]
    fn ensure_is_idempotent() {
        let store = migrated();
        // A freshly migrated store already has every catalog index, so the first ensure
        // creates nothing.
        let first = store.ensure_catalog_indexes(DIM).expect("first ensure");
        assert!(
            first.is_empty(),
            "a fully-indexed store creates nothing: {first:?}"
        );

        // Drop one of each class, ensure once to converge, then ensure again: the second
        // run is a no-op — the all-catalog state is a fixed point.
        drop_vector(&store, "Fact", "embedding_v1");
        drop_text(&store, "Fact", "statement");
        drop_property(&store, "Fact", "predicate");
        let converged = store.ensure_catalog_indexes(DIM).expect("converge");
        assert_eq!(
            converged.len(),
            3,
            "one of each class converged: {converged:?}"
        );

        let second = store.ensure_catalog_indexes(DIM).expect("second ensure");
        assert!(second.is_empty(), "second ensure is a no-op: {second:?}");
    }

    /// A fresh, empty temp directory unique to `label`, removed first so re-runs start
    /// clean. No external temp-dir crate, matching the engine's own durable tests.
    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aionforge-reconcile-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn insert_raw_episode(store: &Store, content: &str) -> Id {
        let id = Id::from_content_hash(content.as_bytes());
        let episode = Episode {
            identity: Identity {
                id,
                ingested_at: now(),
                namespace: Namespace::Agent("test".to_string()),
                expired_at: None,
            },
            stats: Stats {
                importance: 0.5,
                trust: 0.8,
                last_access: now(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: content.to_string(),
            role: Role::User,
            captured_at: now(),
            agent_id: Id::from_content_hash(b"agent:test"),
            session_id: Some(Id::from_content_hash(b"session:test")),
            content_hash: ContentHash::of(content.as_bytes()),
            embedding: None,
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        };
        store.insert_episode(&episode).expect("insert episode");
        id
    }

    fn episode_count(store: &Store) -> usize {
        match store
            .execute(&crate::gql::BoundQuery::new(
                "MATCH (e:Episode) RETURN e.id AS id",
            ))
            .expect("count episodes")
        {
            crate::gql::QueryResult::Rows(rows) => rows.row_count(),
            other => panic!("unexpected query result: {other:?}"),
        }
    }

    /// END-TO-END: build a store on disk with a real row, drop a catalog index, then reopen
    /// from the SAME dir via `Store::open_or_recover`. Recovery's `ensure_catalog_indexes`
    /// must recreate the dropped index AND the data must survive (non-lossy — the engine
    /// backfills the created index from the primary rows the WAL replayed).
    #[test]
    fn recover_recreates_a_dropped_catalog_index_non_lossily() {
        let dir = temp_dir("recover-recreates");
        let config = StoreConfig::default(); // a real (large) embedder dimension on disk

        let episode_id;
        // Write phase: migrate, insert one Episode, then drop a catalog index so the
        // persisted (WAL) state is missing it — the older-catalog shape recovery must fix.
        {
            let store =
                Store::open_persistent_migrated(&dir, config, &now()).expect("open and migrate");
            episode_id = insert_raw_episode(&store, "a recovered episode");
            assert_eq!(episode_count(&store), 1, "the row is present before drop");

            // Drop a scalar catalog index; the drop commits to the WAL, so the recovered
            // graph replays a schema MISSING this index.
            drop_property(&store, "Episode", "role");
            assert!(
                !has_property_index(&store, "Episode", "role"),
                "the index is gone in the persisted store"
            );
            // Drop releases the WAL lock so recovery can reopen it in this process.
            drop(store);
        }

        // Recovery phase: reopen from the same dir. `recover` runs `ensure_catalog_indexes`
        // after the dimension check, so the dropped index is recreated.
        let recovered = Store::open_or_recover(&dir, config, &now()).expect("recover");
        assert!(
            has_property_index(&recovered, "Episode", "role"),
            "the dropped catalog index is recreated on recovery"
        );
        // Non-lossy: the row survived recovery and the created index was backfilled from it.
        assert_eq!(
            episode_count(&recovered),
            1,
            "the row survives recovery (non-lossy)"
        );
        let episode = recovered
            .episode_by_id(&episode_id)
            .expect("episode lookup")
            .expect("episode recovered");
        assert_eq!(episode.content, "a recovered episode", "the data is intact");

        // And recovery is idempotent: a second open creates nothing more — the catalog is
        // already complete.
        drop(recovered);
        let rerecovered = Store::recover(&dir, config, &now()).expect("re-recover");
        let created = rerecovered
            .ensure_catalog_indexes(config.embedding_dimension)
            .expect("ensure on a fully-indexed recovered store");
        assert!(
            created.is_empty(),
            "a re-recovered, fully-indexed store creates nothing: {created:?}"
        );
        drop(rerecovered);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
