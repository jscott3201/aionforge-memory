//! An owned, engine-free snapshot of the declared schema.
//!
//! [`Store::schema_snapshot`] reads the closed graph's bound type and projects it into
//! these owned shapes, so the schema-mirror and bi-temporal tests (and the future
//! `doctor` surface) can assert against a stable structure without naming selene-db
//! types. It mirrors only what the schema declares — names, value kinds, and the
//! nullability/immutability/uniqueness constraints — not instance data.

use selene_core::PropertyValueType;
use selene_graph::{GraphTypeDef, PropertyTypeDef};

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
    /// `STRING`.
    String,
    /// `UUID`.
    Uuid,
    /// `ZONED DATETIME`.
    ZonedDateTime,
    /// `VECTOR`.
    Vector,
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
        PropertyValueType::String => PropertyKind::String,
        PropertyValueType::Uuid => PropertyKind::Uuid,
        PropertyValueType::ZonedDateTime => PropertyKind::ZonedDateTime,
        PropertyValueType::Vector => PropertyKind::Vector,
        PropertyValueType::Json => PropertyKind::Json,
        PropertyValueType::List => PropertyKind::List,
        PropertyValueType::Record | PropertyValueType::RecordTyped => PropertyKind::Record,
        _ => PropertyKind::Other,
    }
}
