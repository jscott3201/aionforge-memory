//! Shared MCP principal parsing.
//!
//! The built-in server never derives identity from a transport connection. Hosts that
//! authenticate a caller can pass the verified identity through the explicit `principal`
//! object; older clients can keep using `agent_id` / `viewer` plus top-level `teams`.
//! When both shapes are present they must agree, so there is no silent merge of two
//! authority sources.

use std::collections::BTreeSet;

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_engine::Principal;
use schemars::JsonSchema;
use serde::Deserialize;

/// Host-verified principal parameters shared by read and write tools.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HostPrincipalToolParam {
    /// The authenticated agent id as a UUID. A host may derive this from OAuth or
    /// another verifier, then pass it explicitly here.
    #[schemars(description = "The authenticated agent id as a UUID.")]
    pub agent_id: String,
    /// Teams the host asserts this agent belongs to.
    #[serde(default)]
    #[schemars(description = "Teams the host asserts this agent belongs to. Optional.")]
    pub teams: Vec<String>,
}

/// Resolve capture identity from legacy `agent_id` or the explicit host principal.
///
/// # Errors
/// Returns a structured `ERR_*` string if no identity is supplied, ids are malformed, or
/// legacy and principal fields conflict.
pub(crate) fn resolve_writer(
    raw_agent_id: Option<&str>,
    legacy_teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
) -> Result<(Id, Vec<String>), String> {
    let resolved = resolve_agent_id(raw_agent_id, principal.as_ref(), IdentityField::AgentId)?;
    let teams = resolve_teams(legacy_teams, principal)?;
    Ok((resolved, teams))
}

/// Resolve read identity from legacy `viewer` or the explicit host principal.
///
/// # Errors
/// Returns a structured `ERR_*` string if no identity is supplied, ids are malformed, or
/// legacy and principal fields conflict.
pub(crate) fn resolve_reader(
    raw_viewer: Option<&str>,
    legacy_teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
) -> Result<Principal, String> {
    let agent = resolve_agent_id(raw_viewer, principal.as_ref(), IdentityField::Viewer)?;
    let teams = resolve_teams(legacy_teams, principal)?;
    Ok(Principal::new(agent, teams))
}

#[derive(Clone, Copy)]
enum IdentityField {
    AgentId,
    Viewer,
}

fn resolve_agent_id(
    legacy: Option<&str>,
    principal: Option<&HostPrincipalToolParam>,
    field: IdentityField,
) -> Result<Id, String> {
    let from_legacy = legacy.map(|raw| match field {
        IdentityField::AgentId => parse_agent_id(raw),
        IdentityField::Viewer => parse_viewer(raw),
    });
    let from_principal = principal.map(|principal| parse_agent_id(&principal.agent_id));
    match (from_legacy, from_principal) {
        (None, None) => Err(match field {
            IdentityField::AgentId => {
                "ERR_MISSING_AGENT_ID: provide agent_id or principal.agent_id".to_string()
            }
            IdentityField::Viewer => {
                "ERR_MISSING_PRINCIPAL: provide viewer=agent:<id> or principal.agent_id".to_string()
            }
        }),
        (Some(Err(error)), _) | (_, Some(Err(error))) => Err(error),
        (Some(Ok(legacy)), None) | (None, Some(Ok(legacy))) => Ok(legacy),
        (Some(Ok(legacy)), Some(Ok(principal))) if legacy == principal => Ok(legacy),
        (Some(Ok(_)), Some(Ok(_))) => Err(match field {
            IdentityField::AgentId => {
                "ERR_PRINCIPAL_MISMATCH: agent_id must match principal.agent_id".to_string()
            }
            IdentityField::Viewer => {
                "ERR_PRINCIPAL_MISMATCH: viewer must match principal.agent_id".to_string()
            }
        }),
    }
}

fn parse_agent_id(raw: &str) -> Result<Id, String> {
    Id::parse(raw).map_err(|_| "ERR_INVALID_AGENT_ID: agent_id must be a UUID".to_string())
}

fn parse_viewer(raw: &str) -> Result<Id, String> {
    let viewer: Namespace = raw
        .parse()
        .map_err(|_| "ERR_INVALID_VIEWER: viewer must be agent:<id>".to_string())?;
    let Namespace::Agent(agent_id) = viewer else {
        return Err("ERR_INVALID_VIEWER: a reader must be an agent (agent:<id>)".to_string());
    };
    Id::parse(&agent_id)
        .map_err(|_| "ERR_INVALID_VIEWER: viewer agent id must be a UUID".to_string())
}

fn resolve_teams(
    legacy_teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
) -> Result<Vec<String>, String> {
    let legacy_teams = normalize_teams(legacy_teams);
    let Some(principal) = principal else {
        return Ok(legacy_teams);
    };
    let principal_teams = normalize_teams(principal.teams);
    if legacy_teams.is_empty() {
        return Ok(principal_teams);
    }
    if principal_teams.is_empty() || same_team_set(&legacy_teams, &principal_teams) {
        return Ok(legacy_teams);
    }
    Err(
        "ERR_PRINCIPAL_MISMATCH: teams and principal.teams must match when both are supplied"
            .to_string(),
    )
}

fn normalize_teams(teams: Vec<String>) -> Vec<String> {
    teams.into_iter().filter(|team| !team.is_empty()).collect()
}

fn same_team_set(left: &[String], right: &[String]) -> bool {
    let left: BTreeSet<&str> = left.iter().map(String::as_str).collect();
    let right: BTreeSet<&str> = right.iter().map(String::as_str).collect();
    left == right
}

#[cfg(test)]
mod tests {
    use super::{HostPrincipalToolParam, resolve_reader, resolve_writer};
    use aionforge_domain::ids::Id;

    #[test]
    fn resolves_reader_from_explicit_host_principal() {
        let agent = Id::generate();
        let principal = resolve_reader(
            None,
            Vec::new(),
            Some(HostPrincipalToolParam {
                agent_id: agent.to_string(),
                teams: vec!["core".to_string()],
            }),
        )
        .expect("principal resolves");
        assert_eq!(principal.agent_id, agent);
        assert_eq!(principal.teams, vec!["core"]);
    }

    #[test]
    fn rejects_conflicting_reader_identity_sources() {
        let viewer = Id::generate();
        let principal = Id::generate();
        let err = resolve_reader(
            Some(&format!("agent:{viewer}")),
            Vec::new(),
            Some(HostPrincipalToolParam {
                agent_id: principal.to_string(),
                teams: Vec::new(),
            }),
        )
        .expect_err("mismatch rejected");
        assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    }

    #[test]
    fn rejects_conflicting_team_assertions() {
        let agent = Id::generate();
        let err = resolve_writer(
            Some(&agent.to_string()),
            vec!["alpha".to_string()],
            Some(HostPrincipalToolParam {
                agent_id: agent.to_string(),
                teams: vec!["beta".to_string()],
            }),
        )
        .expect_err("mismatch rejected");
        assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    }
}
