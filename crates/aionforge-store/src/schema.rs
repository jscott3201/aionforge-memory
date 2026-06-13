//! An owned, engine-free snapshot of the declared schema.
//!
//! [`Store::schema_snapshot`] reads the closed graph's bound type and projects it into
//! these owned shapes, so the schema-mirror and bi-temporal tests (and the future
//! `doctor` surface) can assert against a stable structure without naming selene-db
//! types. It mirrors only what the schema declares — names, value kinds, and the
//! nullability/immutability/uniqueness constraints — not instance data.

use selene_core::PropertyValueType;
use selene_graph::{GraphTypeDef, PropertyTypeDef};

use crate::error::StoreError;
use crate::store::Store;

/// The full declared schema: every node and edge type and its properties.
#[derive(Debug, Clone)]
pub struct SchemaSnapshot {
    /// The declared node types, in declaration order.
    pub node_types: Vec<NodeTypeShape>,
    /// The declared edge types, in declaration order.
    pub edge_types: Vec<EdgeTypeShape>,
}

impl SchemaSnapshot {
    /// The node type with this label, if declared.
    #[must_use]
    pub fn node_type(&self, name: &str) -> Option<&NodeTypeShape> {
        self.node_types.iter().find(|node| node.name == name)
    }

    /// The edge type with this relationship label, if declared.
    #[must_use]
    pub fn edge_type(&self, label: &str) -> Option<&EdgeTypeShape> {
        self.edge_types.iter().find(|edge| edge.label == label)
    }
}

/// A declared node type.
#[derive(Debug, Clone)]
pub struct NodeTypeShape {
    /// The node label.
    pub name: String,
    /// The declared properties.
    pub properties: Vec<PropertyShape>,
}

impl NodeTypeShape {
    /// The property with this name, if declared.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<&PropertyShape> {
        self.properties
            .iter()
            .find(|property| property.name == name)
    }
}

/// A declared edge type.
#[derive(Debug, Clone)]
pub struct EdgeTypeShape {
    /// The edge type name (equals the label for the catalog's single-label kinds).
    pub name: String,
    /// The relationship label.
    pub label: String,
    /// The declared properties.
    pub properties: Vec<PropertyShape>,
}

impl EdgeTypeShape {
    /// The property with this name, if declared.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<&PropertyShape> {
        self.properties
            .iter()
            .find(|property| property.name == name)
    }
}

/// A declared property and its constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyShape {
    /// The property name.
    pub name: String,
    /// The property's value kind.
    pub value_type: PropertyKind,
    /// True when the property is `NOT NULL` (the engine's `required`).
    pub required: bool,
    /// True when the property is `IMMUTABLE`.
    pub immutable: bool,
    /// True when the property is `UNIQUE`.
    pub unique: bool,
}

/// The value kind of a declared property — the subset the data model uses, plus an
/// `Other` catch-all so the snapshot never depends on the engine's full enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    /// `BOOLEAN`.
    Bool,
    /// `INT` (i64).
    Int,
    /// `UINT` (u64).
    Uint,
    /// `FLOAT` (f64).
    Float,
    /// `DECIMAL` (exact fixed-point).
    Decimal,
    /// `STRING`.
    String,
    /// `UUID`.
    Uuid,
    /// `ZONED DATETIME`.
    ZonedDateTime,
    /// `VECTOR`.
    Vector,
    /// `BYTES` (binary string).
    Bytes,
    /// `JSON`.
    Json,
    /// `LIST<T>`.
    List,
    /// `RECORD` (open or typed).
    Record,
    /// Any other engine value kind the data model does not use.
    Other,
}

impl Store {
    /// Project the closed graph's bound type into an owned [`SchemaSnapshot`].
    ///
    /// Returns `None` only if the graph is open (no bound type), which a store opened
    /// through this crate never is.
    #[must_use]
    pub fn schema_snapshot(&self) -> Option<SchemaSnapshot> {
        self.graph().graph_type().as_deref().map(snapshot_from)
    }

    /// Assert the bound type admits the audit signature latch (02 §4.11, M4.T06).
    ///
    /// The audit write funnel heals a blank-signature `AuditEvent` row by upgrading the
    /// `signature` property in place, so the live binding must declare that property
    /// mutable. A store whose persisted DDL predates the latch still declares it
    /// `IMMUTABLE` — recovery replays the persisted statements, not the compiled-in
    /// catalog, and the engine has no `ALTER TYPE`, so the divergence cannot be migrated
    /// forward. On such a binding the heal would surface as an `ImmutablePropertyUpdate`
    /// aborting the whole enclosing write at some arbitrary later commit; this check
    /// turns that into a loud open-time failure instead (pre-1.0 posture: recreate the
    /// store from a fresh migration).
    ///
    /// Quiet on an unbound graph or before the `AuditEvent` type exists (the
    /// pre-migration states — nothing has drifted yet).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the bound `AuditEvent.signature` is declared immutable.
    pub fn audit_signature_latch_check(&self) -> Result<(), StoreError> {
        let Some(snapshot) = self.schema_snapshot() else {
            return Ok(());
        };
        let Some(audit) = snapshot.node_type("AuditEvent") else {
            return Ok(());
        };
        if audit.property("signature").is_some_and(|p| p.immutable) {
            return Err(StoreError::invariant(
                "this store's AuditEvent.signature is declared IMMUTABLE (a schema from \
                 before the M4.T06 audit-signature latch); the blank->signed heal cannot \
                 work on this binding and the engine has no ALTER TYPE to migrate it — \
                 recreate the store from a fresh migration (pre-1.0 schema change)"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

fn snapshot_from(graph_type: &GraphTypeDef) -> SchemaSnapshot {
    let node_types = graph_type
        .node_types
        .iter()
        .map(|node_type| NodeTypeShape {
            name: node_type.name.as_str().to_owned(),
            properties: node_type.properties.iter().map(property_shape).collect(),
        })
        .collect();
    let edge_types = graph_type
        .edge_types
        .iter()
        .map(|edge_type| EdgeTypeShape {
            name: edge_type.name.as_str().to_owned(),
            label: edge_type.label.as_str().to_owned(),
            properties: edge_type.properties.iter().map(property_shape).collect(),
        })
        .collect();
    SchemaSnapshot {
        node_types,
        edge_types,
    }
}

fn property_shape(property: &PropertyTypeDef) -> PropertyShape {
    PropertyShape {
        name: property.name.as_str().to_owned(),
        value_type: property_kind(&property.value_type),
        required: property.required,
        immutable: property.immutable,
        unique: property.unique,
    }
}

fn property_kind(value_type: &PropertyValueType) -> PropertyKind {
    match value_type {
        PropertyValueType::Bool => PropertyKind::Bool,
        PropertyValueType::Int => PropertyKind::Int,
        PropertyValueType::Uint => PropertyKind::Uint,
        PropertyValueType::Float => PropertyKind::Float,
        PropertyValueType::Decimal => PropertyKind::Decimal,
        PropertyValueType::String => PropertyKind::String,
        PropertyValueType::Uuid => PropertyKind::Uuid,
        PropertyValueType::ZonedDateTime => PropertyKind::ZonedDateTime,
        PropertyValueType::Vector => PropertyKind::Vector,
        PropertyValueType::Bytes => PropertyKind::Bytes,
        PropertyValueType::Json => PropertyKind::Json,
        PropertyValueType::List => PropertyKind::List,
        PropertyValueType::Record | PropertyValueType::RecordTyped => PropertyKind::Record,
        // The genuinely-unused tower (Int128/Uint128/Float32, temporal subtypes) still
        // collapses to Other; add an explicit arm here when the data model adopts one.
        _ => PropertyKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use selene_core::PropertyValueType;

    use super::{PropertyKind, property_kind};

    #[test]
    fn decimal_and_bytes_map_to_their_own_kinds() {
        // The forward-insurance arms: a future DECIMAL/BYTES column mirrors faithfully
        // instead of collapsing to Other and losing its real type in the drift guard.
        assert_eq!(
            property_kind(&PropertyValueType::Decimal),
            PropertyKind::Decimal
        );
        assert_eq!(
            property_kind(&PropertyValueType::Bytes),
            PropertyKind::Bytes
        );
    }

    #[test]
    fn the_genuinely_unused_tower_still_collapses_to_other() {
        // Variants the data model does not use stay Other by design, so the mirror does
        // not grow a kind for every engine type the store never declares.
        assert_eq!(
            property_kind(&PropertyValueType::Int128),
            PropertyKind::Other
        );
        assert_eq!(
            property_kind(&PropertyValueType::Float32),
            PropertyKind::Other
        );
    }
}
