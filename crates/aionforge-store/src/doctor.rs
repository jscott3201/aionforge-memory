//! Read-only doctor report for schema, index, provider, lag, and graph occupancy.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::catalog::{NODE_TYPES, SCHEMA_VERSION};
use crate::indexes::{SCALAR_INDEXES, TEXT_INDEXES, VECTOR_INDEXES};
use crate::providers::{CANDIDATE_STATE_NAMES, CandidateStateInfo};
use crate::store::Store;
use crate::{LagSnapshot, StoreError, VectorIndexInfo};

const EXPECTED_VECTOR_KIND: &str = "HnswCosine";
const EXPECTED_COMPOSITE_INDEXES: &[(&str, &[&str])] = &[
    ("Fact", &["subject_id", "predicate"]),
    ("Fact", &["subject_id", "status"]),
    ("Skill", &["name", "version"]),
    ("AuditEvent", &["subject_id", "occurred_at"]),
    ("AuditEvent", &["kind", "occurred_at"]),
];

/// Presence check for a catalog-backed inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryCheck<T> {
    /// Expected entries, sorted for stable output.
    pub expected: Vec<T>,
    /// Actual entries, sorted for stable output.
    pub actual: Vec<T>,
    /// Expected entries not found in the actual inventory.
    pub missing: Vec<T>,
    /// Actual entries not declared by the compiled catalog.
    pub unexpected: Vec<T>,
}

impl<T> InventoryCheck<T> {
    /// True when the actual inventory matches the expected inventory exactly.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.unexpected.is_empty()
    }
}

/// A single-label single-property index key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IndexKey {
    /// Indexed node label.
    pub label: String,
    /// Indexed property name.
    pub property: String,
}

/// A single-label composite index key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CompositeIndexKey {
    /// Indexed node label.
    pub label: String,
    /// Indexed property names, in composite order.
    pub properties: Vec<String>,
}

/// A vector index whose dimension does not match the configured embedder dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorDimensionMismatch {
    /// Indexed node label.
    pub label: String,
    /// Indexed vector property.
    pub property: String,
    /// Dimension the running deployment expects.
    pub expected_dimension: u32,
    /// Dimension pinned on the vector index.
    pub actual_dimension: u32,
}

/// A vector index whose kind does not match the catalog's HNSW/cosine posture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorKindMismatch {
    /// Indexed node label.
    pub label: String,
    /// Indexed vector property.
    pub property: String,
    /// Expected vector index kind.
    pub expected_kind: String,
    /// Actual vector index kind.
    pub actual_kind: String,
}

/// Schema-version and type-shape health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaDoctorReport {
    /// True when the store is bound to a closed graph type.
    pub schema_bound: bool,
    /// Applied schema version, or `0` before migration.
    pub current_version: i64,
    /// Compiled schema version this binary expects.
    pub target_version: i64,
    /// Declared node type count in the bound graph type.
    pub node_type_count: usize,
    /// Declared edge type count in the bound graph type.
    pub edge_type_count: usize,
    /// True when the applied schema is current and a closed schema is bound.
    pub ok: bool,
}

/// Index catalog health, including vector dimension/kind checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDoctorReport {
    /// Dimension expected by this store's configuration.
    pub expected_embedder_dimension: u32,
    /// Vector index presence check.
    pub vector_indexes: InventoryCheck<IndexKey>,
    /// Text index presence check.
    pub text_indexes: InventoryCheck<IndexKey>,
    /// Scalar property index presence check.
    pub property_indexes: InventoryCheck<IndexKey>,
    /// Composite property index presence check.
    pub composite_indexes: InventoryCheck<CompositeIndexKey>,
    /// Registered vector indexes with the wrong dimension.
    pub vector_dimension_mismatches: Vec<VectorDimensionMismatch>,
    /// Registered vector indexes with the wrong index kind.
    pub vector_kind_mismatches: Vec<VectorKindMismatch>,
    /// True when every expected index is present, no unexpected index exists, and vector
    /// dimensions/kinds match.
    pub ok: bool,
}

/// Maintained candidate-state provider health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDoctorReport {
    /// Provider presence check.
    pub candidate_states: InventoryCheck<String>,
    /// Current provider watermarks and set sizes.
    pub candidate_state_infos: Vec<CandidateStateInfo>,
    /// True when the expected provider sets are present and no unexpected sets exist.
    pub ok: bool,
}

/// Current graph occupancy and generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreCapacityReport {
    /// Current graph generation.
    pub generation: u64,
    /// Live node count in the current snapshot.
    pub node_count: usize,
    /// Live edge count in the current snapshot.
    pub edge_count: usize,
}

/// The canonical store doctor snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreDoctorReport {
    /// True when every check represented in this report is healthy.
    pub ok: bool,
    /// Schema-version and type-shape health.
    pub schema: SchemaDoctorReport,
    /// Index catalog health.
    pub indexes: IndexDoctorReport,
    /// Maintained provider health.
    pub providers: ProviderDoctorReport,
    /// Consolidation backlog snapshot.
    pub consolidation_lag: LagSnapshot,
    /// Current graph occupancy and generation.
    pub capacity: StoreCapacityReport,
}

impl Store {
    /// Build the read-only store doctor report.
    ///
    /// Health failures are represented in the returned report (`ok = false`) rather than
    /// as errors. This returns [`StoreError`] only when the store cannot be queried.
    pub fn doctor_report(&self) -> Result<StoreDoctorReport, StoreError> {
        let schema = self.schema_doctor_report()?;
        let indexes = self.index_doctor_report();
        let providers = self.provider_doctor_report()?;
        let consolidation_lag = self.consolidation_lag()?;
        let capacity = self.capacity_report();
        let ok = schema.ok && indexes.ok && providers.ok;
        Ok(StoreDoctorReport {
            ok,
            schema,
            indexes,
            providers,
            consolidation_lag,
            capacity,
        })
    }

    fn schema_doctor_report(&self) -> Result<SchemaDoctorReport, StoreError> {
        let snapshot = self.schema_snapshot();
        let schema_bound = snapshot.is_some();
        let (node_type_count, edge_type_count) = snapshot
            .map(|snapshot| (snapshot.node_types.len(), snapshot.edge_types.len()))
            .unwrap_or((0, 0));
        let current_version = self.schema_version()?;
        let ok = schema_bound && current_version == SCHEMA_VERSION;
        Ok(SchemaDoctorReport {
            schema_bound,
            current_version,
            target_version: SCHEMA_VERSION,
            node_type_count,
            edge_type_count,
            ok,
        })
    }

    fn index_doctor_report(&self) -> IndexDoctorReport {
        let expected_dimension = self.config().embedding_dimension;
        let vectors = self.vector_indexes();
        let vector_dimension_mismatches = vector_dimension_mismatches(&vectors, expected_dimension);
        let vector_kind_mismatches = vector_kind_mismatches(&vectors);
        let vector_indexes = inventory_check(
            VECTOR_INDEXES
                .iter()
                .map(|(label, property)| index_key(label, property)),
            vectors
                .iter()
                .map(|index| index_key(&index.label, &index.property)),
        );
        let text_indexes = inventory_check(
            TEXT_INDEXES
                .iter()
                .map(|(label, property)| index_key(label, property)),
            self.text_indexes()
                .into_iter()
                .map(|(label, property)| index_key(&label, &property)),
        );
        let property_indexes = inventory_check(
            NODE_TYPES
                .iter()
                .map(|type_ddl| index_key(type_ddl.name, "namespace"))
                .chain(
                    SCALAR_INDEXES
                        .iter()
                        .map(|(label, property, _kind)| index_key(label, property)),
                ),
            self.property_indexes()
                .into_iter()
                .map(|(label, property)| index_key(&label, &property)),
        );
        let composite_indexes = inventory_check(
            EXPECTED_COMPOSITE_INDEXES
                .iter()
                .map(|(label, properties)| composite_key(label, properties)),
            self.composite_indexes()
                .into_iter()
                .map(|(label, properties)| CompositeIndexKey { label, properties }),
        );
        let ok = vector_indexes.ok()
            && text_indexes.ok()
            && property_indexes.ok()
            && composite_indexes.ok()
            && vector_dimension_mismatches.is_empty()
            && vector_kind_mismatches.is_empty();
        IndexDoctorReport {
            expected_embedder_dimension: expected_dimension,
            vector_indexes,
            text_indexes,
            property_indexes,
            composite_indexes,
            vector_dimension_mismatches,
            vector_kind_mismatches,
            ok,
        }
    }

    fn provider_doctor_report(&self) -> Result<ProviderDoctorReport, StoreError> {
        let candidate_state_infos = self.candidate_state_infos()?;
        let candidate_states = inventory_check(
            CANDIDATE_STATE_NAMES.iter().map(|name| (*name).to_owned()),
            candidate_state_infos.iter().map(|info| info.name.clone()),
        );
        let ok = candidate_states.ok();
        Ok(ProviderDoctorReport {
            candidate_states,
            candidate_state_infos,
            ok,
        })
    }

    fn capacity_report(&self) -> StoreCapacityReport {
        let snapshot = self.snapshot();
        StoreCapacityReport {
            generation: snapshot.meta.generation,
            node_count: snapshot.node_count(),
            edge_count: snapshot.edge_count(),
        }
    }
}

fn inventory_check<T>(
    expected: impl IntoIterator<Item = T>,
    actual: impl IntoIterator<Item = T>,
) -> InventoryCheck<T>
where
    T: Clone + Ord,
{
    let expected: BTreeSet<T> = expected.into_iter().collect();
    let actual: BTreeSet<T> = actual.into_iter().collect();
    let missing = expected.difference(&actual).cloned().collect();
    let unexpected = actual.difference(&expected).cloned().collect();
    InventoryCheck {
        expected: expected.into_iter().collect(),
        actual: actual.into_iter().collect(),
        missing,
        unexpected,
    }
}

fn index_key(label: &str, property: &str) -> IndexKey {
    IndexKey {
        label: label.to_owned(),
        property: property.to_owned(),
    }
}

fn composite_key(label: &str, properties: &[&str]) -> CompositeIndexKey {
    CompositeIndexKey {
        label: label.to_owned(),
        properties: properties
            .iter()
            .map(|property| (*property).to_owned())
            .collect(),
    }
}

fn vector_dimension_mismatches(
    indexes: &[VectorIndexInfo],
    expected_dimension: u32,
) -> Vec<VectorDimensionMismatch> {
    indexes
        .iter()
        .filter(|index| index.dimension != expected_dimension)
        .map(|index| VectorDimensionMismatch {
            label: index.label.clone(),
            property: index.property.clone(),
            expected_dimension,
            actual_dimension: index.dimension,
        })
        .collect()
}

fn vector_kind_mismatches(indexes: &[VectorIndexInfo]) -> Vec<VectorKindMismatch> {
    indexes
        .iter()
        .filter(|index| index.kind != EXPECTED_VECTOR_KIND)
        .map(|index| VectorKindMismatch {
            label: index.label.clone(),
            property: index.property.clone(),
            expected_kind: EXPECTED_VECTOR_KIND.to_owned(),
            actual_kind: index.kind.clone(),
        })
        .collect()
}
