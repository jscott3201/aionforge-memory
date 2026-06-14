//! Translation between a domain [`Tag`] and a selene-db node, plus the content-addressed
//! tag upsert (work-structure design §3).
//!
//! A tag is the cross-cutting classification label: a small, controlled-vocabulary node any
//! kind points at via the `HAS_TAG` edge. Its id is content-addressed over `(namespace,
//! slug)`, so the same slug in a namespace always resolves to one node — [`Store::ensure_tag`]
//! is idempotent, minting a tag the first time and returning the existing one thereafter,
//! which keeps the vocabulary curated rather than letting it sprawl. A tag is Identity-only
//! and is exempt from decay/forgetting by absence from the maintenance scan sets.

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::work::Tag;
use aionforge_domain::time::Timestamp;
use selene_core::{DbString, LabelSet, NodeId, PropertyMap, Value, db_string};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{
    as_id, as_namespace, as_str, as_timestamp, id_value, key, namespace_value, string_value,
    timestamp_value,
};
use crate::error::StoreError;
use crate::store::Store;

// Identity block (§3).
const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
// Tag per-kind fields (work-structure design §3).
const SLUG: &str = "slug";
const DISPLAY: &str = "display";

/// The deterministic id of the tag for `(namespace, slug)`.
///
/// Content-addressed (a UUIDv8) so the same slug in the same namespace always yields the same
/// id — the idempotency key for [`Store::ensure_tag`]. Each segment (the kind tag, the
/// namespace, the slug) is length-prefixed before its bytes, so the encoding is injective
/// regardless of the segment contents: two distinct `(namespace, slug)` pairs can never hash
/// to the same id by reframing, even if a slug or namespace contained a delimiter byte.
pub(crate) fn content_id(namespace: &Namespace, slug: &str) -> Id {
    let namespace = namespace.to_string();
    let mut bytes = Vec::new();
    for segment in [b"Tag".as_slice(), namespace.as_bytes(), slug.as_bytes()] {
        // A fixed-width length prefix frames each variable-length segment unambiguously.
        bytes.extend_from_slice(&(segment.len() as u64).to_le_bytes());
        bytes.extend_from_slice(segment);
    }
    Id::from_content_hash(&bytes)
}

/// The selene-db node label for a tag (mirrors [`Tag::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Tag::LABEL)?))
}

/// Translate a [`Tag`] into the `(labels, properties)` pair for `create_node`.
pub(crate) fn to_node(tag: &Tag) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(6);

    // Identity block.
    pairs.push((key(ID)?, id_value(&tag.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&tag.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&tag.identity.namespace)?));
    if let Some(expired_at) = &tag.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Per-kind fields.
    pairs.push((key(SLUG)?, string_value(&tag.slug)?));
    if let Some(display) = &tag.display {
        pairs.push((key(DISPLAY)?, string_value(display)?));
    }

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`Tag`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Tag, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };

    let identity = Identity {
        id: as_id(require(ID)?)?,
        ingested_at: as_timestamp(require(INGESTED_AT)?)?,
        namespace: as_namespace(require(NAMESPACE)?)?,
        expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
    };

    Ok(Tag {
        identity,
        slug: as_str(require(SLUG)?)?.to_string(),
        display: get(DISPLAY)?.map(as_str).transpose()?.map(str::to_string),
    })
}

/// The committed node carrying this `Tag.id` against a read snapshot (`id` is `UNIQUE`-indexed
/// → at most one).
fn tag_node_id_in(snapshot: &SeleneGraph, id: &Id) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(Tag::LABEL)?;
    let prop = db_string(ID)?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}

impl Store {
    /// Resolve the tag for `(namespace, slug)`, minting it on first use (work-structure design
    /// §3).
    ///
    /// Idempotent and content-addressed: the tag id is derived from `(namespace, slug)`, so a
    /// repeat call returns the existing node rather than a duplicate — the curated-vocabulary
    /// guarantee. The fast path probes a fresh snapshot; on a miss the create re-probes inside
    /// the write transaction (a concurrent writer may have minted it first), so at most one tag
    /// per `(namespace, slug)` ever exists. `display` is recorded only when the tag is first
    /// created; an existing tag's display is left untouched. Returns the tag's domain id and node
    /// id.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, the create, or the commit fails.
    pub fn ensure_tag(
        &self,
        namespace: &Namespace,
        slug: &str,
        display: Option<&str>,
        ingested_at: &Timestamp,
    ) -> Result<(Id, NodeId), StoreError> {
        let id = content_id(namespace, slug);
        if let Some(node) = tag_node_id_in(&self.graph().read(), &id)? {
            return Ok((id, node));
        }

        let tag = Tag {
            identity: Identity {
                id,
                ingested_at: ingested_at.clone(),
                namespace: namespace.clone(),
                expired_at: None,
            },
            slug: slug.to_string(),
            display: display.map(str::to_string),
        };
        let (labels, props) = to_node(&tag)?;

        let mut txn = self.graph().begin_write();
        let node = {
            let mut mutator = txn.mutator();
            match tag_node_id_in(mutator.read(), &id)? {
                Some(existing) => existing,
                None => mutator.create_node(labels, props)?,
            }
        };
        txn.commit()?;
        Ok((id, node))
    }

    /// Read a tag back by its node id from a fresh snapshot.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into a [`Tag`].
    pub fn tag_by_node_id(&self, id: NodeId) -> Result<Option<Tag>, StoreError> {
        let snapshot = self.graph().read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read the tag for `(namespace, slug)` from a fresh snapshot, if it exists.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or the stored data cannot be decoded.
    pub fn tag_by_slug(
        &self,
        namespace: &Namespace,
        slug: &str,
    ) -> Result<Option<Tag>, StoreError> {
        let snapshot = self.graph().read();
        match tag_node_id_in(&snapshot, &content_id(namespace, slug))? {
            Some(node) => Ok(snapshot
                .node_properties(node)
                .map(from_properties)
                .transpose()?),
            None => Ok(None),
        }
    }
}
