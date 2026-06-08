//! Translation between a domain [`Agent`] and a selene-db node (02 §4.8).
//!
//! A control/identity kind: it carries the reduced [`Identity`] block (no `Stats`) plus
//! the signing public key, the writer model identity, the per-category trust map (as
//! JSON), and the lifecycle status. The substrate stores only the public key — private
//! keys never enter it. `model_version` is nullable, so it is omitted when absent;
//! `trust_scores` is `NOT NULL`, so an empty map is still written (it reads back empty).

use aionforge_domain::blocks::Identity;
use aionforge_domain::nodes::agent::Agent;
use selene_core::{DbString, LabelSet, PropertyMap, Value, db_string};

use crate::convert::{
    as_id, as_namespace, as_str, as_timestamp, enum_from_value, enum_value, id_value,
    json_from_value, json_value, key, namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;

const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
const PUBLIC_KEY: &str = "public_key";
const MODEL_FAMILY: &str = "model_family";
const MODEL_VERSION: &str = "model_version";
const TRUST_SCORES: &str = "trust_scores";
const STATUS: &str = "status";

/// The selene-db node label for an agent.
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Agent::LABEL)?))
}

/// Translate an [`Agent`] into `(labels, properties)` for `create_node`.
pub(crate) fn to_node(agent: &Agent) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(9);

    pairs.push((key(ID)?, id_value(&agent.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&agent.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&agent.identity.namespace)?));
    if let Some(expired_at) = &agent.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }
    pairs.push((key(PUBLIC_KEY)?, string_value(&agent.public_key)?));
    pairs.push((key(MODEL_FAMILY)?, string_value(&agent.model_family)?));
    if let Some(version) = &agent.model_version {
        pairs.push((key(MODEL_VERSION)?, string_value(version)?));
    }
    pairs.push((key(TRUST_SCORES)?, json_value(&agent.trust_scores)?));
    pairs.push((key(STATUS)?, enum_value(&agent.status)?));

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct an [`Agent`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Agent, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };

    Ok(Agent {
        identity: Identity {
            id: as_id(require(ID)?)?,
            ingested_at: as_timestamp(require(INGESTED_AT)?)?,
            namespace: as_namespace(require(NAMESPACE)?)?,
            expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
        },
        public_key: as_str(require(PUBLIC_KEY)?)?.to_string(),
        model_family: as_str(require(MODEL_FAMILY)?)?.to_string(),
        model_version: get(MODEL_VERSION)?
            .map(as_str)
            .transpose()?
            .map(ToString::to_string),
        trust_scores: json_from_value(require(TRUST_SCORES)?)?,
        status: enum_from_value(require(STATUS)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::agent::{AgentStatus, TrustCategory, TrustScores};
    use std::collections::BTreeMap;

    fn ts(text: &str) -> aionforge_domain::time::Timestamp {
        text.parse().expect("valid zoned datetime literal")
    }

    #[test]
    fn round_trips_every_field() {
        let mut scores = BTreeMap::new();
        scores.insert(
            "reliability".to_string(),
            TrustCategory {
                alpha: 3.0,
                beta: 1.0,
                score: 0.75,
            },
        );
        let agent = Agent {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-08T09:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Agent("ops".to_string()),
                expired_at: Some(ts("2026-07-01T00:00:00-05:00[America/Chicago]")),
            },
            public_key: "cHVibGljLWtleQ==".to_string(),
            model_family: "claude".to_string(),
            model_version: Some("opus-4.8".to_string()),
            trust_scores: TrustScores(scores),
            status: AgentStatus::Retired,
        };

        let (_labels, props) = to_node(&agent).expect("to_node");
        let back = from_properties(&props).expect("from_properties");
        assert_eq!(agent, back);
    }

    #[test]
    fn omits_optional_fields_when_absent() {
        let agent = Agent {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-08T09:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Agent("ops".to_string()),
                expired_at: None,
            },
            public_key: "a2V5".to_string(),
            model_family: "claude".to_string(),
            model_version: None,
            trust_scores: TrustScores::default(),
            status: AgentStatus::Active,
        };

        let (_labels, props) = to_node(&agent).expect("to_node");
        let back = from_properties(&props).expect("from_properties");
        assert_eq!(agent, back);
        assert_eq!(back.model_version, None);
    }
}
