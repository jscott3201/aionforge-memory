//! Committed-graph dedup probes for consolidation materialization.
//!
//! Materialization is non-destructive and replay-idempotent: before writing a derived node it
//! asks whether an equal one already exists in the committed graph, so re-consolidating an
//! episode (after a crash, or because a later episode revisits the same entity) writes nothing
//! new. These are the read-only probes that answer that question — entities by their
//! content-addressed id (then an exact-name fallback) and facts by their `(subject, predicate,
//! object)` value.

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::value::ObjectValue;
use selene_core::{NodeId, db_string};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{id_value, string_value};
use crate::error::StoreError;
use crate::{entity, fact};

/// The committed node carrying this `Entity.id`, if any (`id` is UNIQUE-indexed → at most one).
pub(crate) fn entity_node_by_id(
    snapshot: &SeleneGraph,
    id: &Id,
) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(Entity::LABEL)?;
    let prop = db_string("id")?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}

/// Find an entity already in the committed graph that this `entity` should dedup into.
/// Probes the content-addressed `Entity.id` first (authoritative; a resolution gate-miss can
/// re-mint an existing id, which must collapse onto its node or collide with the `UNIQUE`
/// constraint and poison the flip — the id and the gate share one `normalize`, resolve.rs),
/// then falls back to an exact `canonical_name` + type + namespace probe (cross-id-scheme bridge).
pub(crate) fn find_existing_entity(
    snapshot: &SeleneGraph,
    entity: &Entity,
) -> Result<Option<(Id, NodeId)>, StoreError> {
    if let Some(node) = entity_node_by_id(snapshot, &entity.identity.id)? {
        return Ok(Some((entity.identity.id, node)));
    }

    // Fallback: exact canonical name + type + namespace (cross-id-scheme bridge).
    let label = db_string(Entity::LABEL)?;
    let name_prop = db_string("canonical_name")?;
    let value = string_value(&entity.canonical_name)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &name_prop, &value) else {
        return Ok(None);
    };
    for row in rows.iter() {
        let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
            continue;
        };
        let Some(props) = snapshot.node_properties(node) else {
            continue;
        };
        let candidate = entity::from_properties(props)?;
        if candidate.entity_type == entity.entity_type
            && candidate.identity.namespace == entity.identity.namespace
        {
            return Ok(Some((candidate.identity.id, node)));
        }
    }
    Ok(None)
}

/// Find a fact already asserted with this `(subject_id, predicate)` and object value.
/// `subject_id` is indexed, so this probes the bounded subject set and compares in Rust
/// (`Fact.id` is unique but not indexed, so dedup is by value, not by an id scan).
pub(crate) fn find_existing_fact(
    snapshot: &SeleneGraph,
    subject_id: &Id,
    predicate: &str,
    object: &ObjectValue,
) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(Fact::LABEL)?;
    let subject_prop = db_string("subject_id")?;
    let value = id_value(subject_id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &subject_prop, &value) else {
        return Ok(None);
    };
    for row in rows.iter() {
        let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
            continue;
        };
        let Some(props) = snapshot.node_properties(node) else {
            continue;
        };
        let candidate = fact::from_properties(props)?;
        if candidate.predicate == predicate && candidate.object == *object {
            return Ok(Some(node));
        }
    }
    Ok(None)
}
