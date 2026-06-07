//! Low-level translation between domain values and selene-db [`Value`]s.
//!
//! These are the reusable primitives every kind's node translation builds on. The
//! engine has no embedding normalizer, so callers normalize before reaching
//! [`embedding_value`] (the cosine write-path obligation). Enum vocabularies and
//! JSON shapes round-trip through `serde` so the canonical strings stay in one place
//! (the domain `serde` derives), never duplicated here.

use aionforge_domain::{ContentHash, EmbedderModel, Embedding, Id, Namespace, Timestamp};
use selene_core::{DbString, JsonValue, NodeId, Value, VectorValue, db_string};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::StoreError;

// ---- domain -> engine ----

pub(crate) fn key(name: &str) -> Result<DbString, StoreError> {
    Ok(db_string(name)?)
}

pub(crate) fn id_value(id: &Id) -> Result<Value, StoreError> {
    Ok(Value::String(db_string(id.as_str())?))
}

pub(crate) fn hash_value(hash: &ContentHash) -> Result<Value, StoreError> {
    Ok(Value::String(db_string(hash.as_str())?))
}

pub(crate) fn namespace_value(namespace: &Namespace) -> Result<Value, StoreError> {
    Ok(Value::String(db_string(&namespace.to_string())?))
}

pub(crate) fn string_value(text: &str) -> Result<Value, StoreError> {
    Ok(Value::String(db_string(text)?))
}

pub(crate) fn timestamp_value(at: &Timestamp) -> Value {
    Value::ZonedDateTime(Box::new(at.clone()))
}

pub(crate) fn embedding_value(embedding: &Embedding) -> Result<Value, StoreError> {
    Ok(Value::Vector(VectorValue::new(
        embedding.as_slice().to_vec(),
    )?))
}

pub(crate) fn embedder_model_value(model: &EmbedderModel) -> Result<Value, StoreError> {
    // §4.1 types this column STRING; we store the canonical JSON encoding of the
    // identity so the family/version/dimension round-trips losslessly.
    Ok(Value::String(db_string(&serde_json::to_string(model)?)?))
}

/// Encode a domain enum to its canonical spec string via its `serde` rename.
pub(crate) fn enum_value<T: Serialize>(value: &T) -> Result<Value, StoreError> {
    let json = serde_json::to_value(value)?;
    let tag = json
        .as_str()
        .ok_or_else(|| StoreError::decode("enum did not serialize to a string"))?;
    Ok(Value::String(db_string(tag)?))
}

/// Encode a `serde`-serializable shape as a native JSON value.
pub(crate) fn json_value<T: Serialize>(value: &T) -> Result<Value, StoreError> {
    Ok(Value::Json(JsonValue::new(serde_json::to_value(value)?)?))
}

// ---- engine -> domain ----

pub(crate) fn as_str(value: &Value) -> Result<&str, StoreError> {
    match value {
        Value::String(s) => Ok(s.as_str()),
        other => Err(StoreError::decode(format!(
            "expected a string, found {other:?}"
        ))),
    }
}

pub(crate) fn as_timestamp(value: &Value) -> Result<Timestamp, StoreError> {
    match value {
        Value::ZonedDateTime(z) => Ok((**z).clone()),
        other => Err(StoreError::decode(format!(
            "expected a zoned datetime, found {other:?}"
        ))),
    }
}

pub(crate) fn as_f64(value: &Value) -> Result<f64, StoreError> {
    match value {
        Value::Float(f) => Ok(*f),
        // Widen a single-precision float losslessly: search procedures declare
        // `Float64` scores today, but tolerating `Float32` keeps the decoder robust
        // to any single-precision metric output without a false type error.
        Value::Float32(f) => Ok(f64::from(*f)),
        other => Err(StoreError::decode(format!(
            "expected a float, found {other:?}"
        ))),
    }
}

pub(crate) fn as_u64(value: &Value) -> Result<u64, StoreError> {
    match value {
        Value::Uint(u) => Ok(*u),
        Value::Int(i) => u64::try_from(*i)
            .map_err(|_| StoreError::decode("negative integer for an unsigned field")),
        other => Err(StoreError::decode(format!(
            "expected an unsigned integer, found {other:?}"
        ))),
    }
}

pub(crate) fn as_bool(value: &Value) -> Result<bool, StoreError> {
    match value {
        Value::Bool(b) => Ok(*b),
        other => Err(StoreError::decode(format!(
            "expected a bool, found {other:?}"
        ))),
    }
}

pub(crate) fn as_id(value: &Value) -> Result<Id, StoreError> {
    Ok(Id::parse(as_str(value)?)?)
}

pub(crate) fn as_namespace(value: &Value) -> Result<Namespace, StoreError> {
    Ok(as_str(value)?.parse()?)
}

pub(crate) fn as_content_hash(value: &Value) -> Result<ContentHash, StoreError> {
    // ContentHash has no public parse; it is reconstructed by re-wrapping the stored
    // hex via the typed accessor on read. We round-trip through the hashing-free path
    // by treating the stored hex as authoritative.
    let hex = as_str(value)?;
    Ok(ContentHash::from_hex(hex)?)
}

pub(crate) fn as_node_ref(value: &Value) -> Result<NodeId, StoreError> {
    match value {
        Value::NodeRef(id) => Ok(*id),
        other => Err(StoreError::decode(format!(
            "expected a node reference, found {other:?}"
        ))),
    }
}

pub(crate) fn as_embedding(value: &Value) -> Result<Embedding, StoreError> {
    match value {
        Value::Vector(v) => Ok(Embedding::new(v.as_slice().to_vec())?),
        other => Err(StoreError::decode(format!(
            "expected a vector, found {other:?}"
        ))),
    }
}

pub(crate) fn as_embedder_model(value: &Value) -> Result<EmbedderModel, StoreError> {
    Ok(serde_json::from_str(as_str(value)?)?)
}

/// Decode a domain enum from its canonical spec string via its `serde` rename.
pub(crate) fn enum_from_value<T: DeserializeOwned>(value: &Value) -> Result<T, StoreError> {
    let tag = as_str(value)?;
    Ok(serde_json::from_value(serde_json::Value::String(
        tag.to_string(),
    ))?)
}

/// Decode a `serde`-deserializable shape from a native JSON value.
pub(crate) fn json_from_value<T: DeserializeOwned>(value: &Value) -> Result<T, StoreError> {
    match value {
        Value::Json(j) => Ok(serde_json::from_value(j.as_serde().clone())?),
        other => Err(StoreError::decode(format!(
            "expected JSON, found {other:?}"
        ))),
    }
}
