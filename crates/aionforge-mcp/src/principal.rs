//! Shared MCP principal parsing and the auth precedence rules (PR4 of the OAuth workstream).
//!
//! The built-in server never derives identity from a transport connection. There are three
//! identity sources, in strict precedence:
//!
//! 1. **The validated extension** ([`ValidatedPrincipal`]), inserted by the token-validator
//!    layer. When auth is enabled and the extension is present it is **authoritative**: the
//!    body-asserted fields may only *restate* it (absent, or exactly equal) — they can never
//!    contradict or extend it — and teams (and the operator bit) come from the extension.
//! 2. **The explicit `principal` object** and **legacy `agent_id` / `viewer`** body fields. When
//!    auth is disabled (the default) these are the only sources, with the long-standing
//!    must-agree discipline: when both shapes are present they must agree, so there is no silent
//!    merge of two authority sources.
//!
//! # Auth posture
//!
//! [`AuthEnabled`] selects the posture:
//!
//! * **Disabled** (default) — today's exact behavior. The extension is ignored and identity
//!   resolves from the body. Every pre-existing test in this module exercises this path unchanged.
//! * **Enabled + extension present** — the extension is authoritative (rule 1 above). For a
//!   *write*, a [`ReadOnly`](WritePosture::ReadOnly) posture is refused with
//!   `ERR_READ_ONLY_PRINCIPAL` (the read-only/ephemeral content-hash id must never write).
//! * **Enabled + extension absent** — `ERR_PRINCIPAL_REQUIRED`. The resolver **never** falls back
//!   to a body/legacy identity; this is the reject-on-absent defense-in-depth.

use std::collections::BTreeSet;

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_engine::Principal;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::WritePosture;
use crate::validated::ValidatedPrincipal;

/// Whether the OAuth resource-server posture is enabled for this server.
///
/// Threaded from `auth.enabled` in the deployment config onto the MCP server and into every
/// identity resolver. When `false` (the default) the resolvers reproduce today's body-only
/// behavior byte-for-byte; when `true` they require and trust the validated request extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthEnabled(pub bool);

impl AuthEnabled {
    /// Whether auth is enabled.
    #[must_use]
    pub fn is_enabled(self) -> bool {
        self.0
    }
}

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

/// Resolve capture identity from the validated extension (authoritative when auth is enabled) or,
/// when auth is disabled, the legacy `agent_id` / explicit host principal.
///
/// When auth is enabled and the extension is present, a [`ReadOnly`](WritePosture::ReadOnly)
/// write posture is refused: a read-only/ephemeral identity may carry an unstable content-hash id
/// and MUST NOT write durable memory.
///
/// # Errors
/// Returns a structured `ERR_*` string if: auth is enabled but the extension is absent
/// (`ERR_PRINCIPAL_REQUIRED`); the extension is read-only (`ERR_READ_ONLY_PRINCIPAL`); no identity
/// is supplied; ids are malformed; or legacy/principal/body fields conflict with each other or with
/// the authoritative extension (`ERR_PRINCIPAL_MISMATCH`).
pub(crate) fn resolve_writer(
    raw_agent_id: Option<&str>,
    legacy_teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<(Id, Vec<String>), String> {
    let resolved = resolve_identity(
        raw_agent_id,
        legacy_teams,
        principal,
        extension,
        auth_enabled,
        IdentityField::AgentId,
        Operation::Write,
    )?;
    Ok((resolved.agent_id, resolved.teams))
}

/// Resolve read identity from the validated extension (authoritative when auth is enabled) or,
/// when auth is disabled, the legacy `viewer` / explicit host principal.
///
/// A read never consults the write posture: a read-only identity may always read.
///
/// # Errors
/// Returns a structured `ERR_*` string if: auth is enabled but the extension is absent
/// (`ERR_PRINCIPAL_REQUIRED`); no identity is supplied; ids are malformed; or legacy/principal/body
/// fields conflict with each other or with the authoritative extension (`ERR_PRINCIPAL_MISMATCH`).
pub(crate) fn resolve_reader(
    raw_viewer: Option<&str>,
    legacy_teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<Principal, String> {
    let resolved = resolve_identity(
        raw_viewer,
        legacy_teams,
        principal,
        extension,
        auth_enabled,
        IdentityField::Viewer,
        Operation::Read,
    )?;
    Ok(resolved.into_principal())
}

#[derive(Clone, Copy)]
enum IdentityField {
    AgentId,
    Viewer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Read,
    Write,
}

/// The resolved identity carried out of the precedence helper: the operator bit is preserved so an
/// extension-authoritative reader keeps its system-level capability through to the authorizer.
struct ResolvedIdentity {
    agent_id: Id,
    teams: Vec<String>,
    operator: bool,
}

impl ResolvedIdentity {
    fn into_principal(self) -> Principal {
        if self.operator {
            Principal::with_operator(self.agent_id, self.teams)
        } else {
            Principal::new(self.agent_id, self.teams)
        }
    }
}

/// The single precedence helper shared by [`resolve_reader`] and [`resolve_writer`].
///
/// Implements the three-way posture (auth-disabled body path; auth-enabled extension-authoritative;
/// auth-enabled reject-on-absent) plus the write-only read-only guard. See the module docs.
fn resolve_identity(
    legacy: Option<&str>,
    legacy_teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
    field: IdentityField,
    operation: Operation,
) -> Result<ResolvedIdentity, String> {
    if auth_enabled.is_enabled() {
        let Some(extension) = extension else {
            // Reject-on-absent (defense in depth): with auth enabled, a request that reached a
            // handler without a validated identity is never silently downgraded to a body identity.
            return Err(
                "ERR_PRINCIPAL_REQUIRED: a validated identity is required when auth is enabled"
                    .to_string(),
            );
        };
        return resolve_with_authoritative_extension(
            legacy,
            legacy_teams,
            principal,
            extension,
            field,
            operation,
        );
    }

    // Auth disabled: today's exact body-only behavior. The extension is ignored (always None
    // today), so this path is byte-for-byte unchanged.
    let agent_id = resolve_body_agent_id(legacy, principal.as_ref(), field)?;
    let teams = resolve_body_teams(legacy_teams, principal)?;
    Ok(ResolvedIdentity {
        agent_id,
        teams,
        operator: false,
    })
}

/// The single `ERR_READ_ONLY_PRINCIPAL` refusal text, used by every write-guard site so there is
/// exactly one such message and one mint of it.
const ERR_READ_ONLY_PRINCIPAL: &str =
    "ERR_READ_ONLY_PRINCIPAL: a read-only/ephemeral identity may not write durable memory";

/// The one shared read-only write-guard. Refuses a write when auth is enabled and the validated
/// extension carries a [`ReadOnly`](WritePosture::ReadOnly) posture — the read-only/ephemeral
/// content-hash id must never mutate durable memory.
///
/// This is the *single* guard site. [`resolve_writer`] applies it via [`resolve_identity`]; tools
/// that resolve a write through the read scope (forget/unforget/pin/unpin, which look up the target
/// then namespace-authorize it) call this directly instead of re-implementing the check, so the
/// guard never drifts into a second, untested copy.
///
/// # Errors
/// Returns `ERR_READ_ONLY_PRINCIPAL` when auth is enabled and `extension` is read-only; `Ok(())`
/// otherwise (auth disabled, no extension, or a writer/ephemeral-writer posture).
pub(crate) fn refuse_read_only_write(
    extension: Option<&ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<(), String> {
    if auth_enabled.is_enabled()
        && extension.is_some_and(|extension| extension.write_posture == WritePosture::ReadOnly)
    {
        return Err(ERR_READ_ONLY_PRINCIPAL.to_string());
    }
    Ok(())
}

/// Resolve identity when the validated extension is authoritative: the body may only *restate* it.
///
/// The extension's agent id, teams, and operator bit are the truth. A body-supplied identity
/// (legacy `agent_id`/`viewer` or the explicit `principal` object) is permitted only if it is
/// absent or exactly equal to the extension's; any other value is an `ERR_PRINCIPAL_MISMATCH`. A
/// [`ReadOnly`](WritePosture::ReadOnly) write is refused outright.
fn resolve_with_authoritative_extension(
    legacy: Option<&str>,
    legacy_teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
    extension: ValidatedPrincipal,
    field: IdentityField,
    operation: Operation,
) -> Result<ResolvedIdentity, String> {
    if operation == Operation::Write {
        // The single write-guard, shared with the point-op write path (forget/pin/...).
        refuse_read_only_write(Some(&extension), AuthEnabled(true))?;
    }

    let authoritative_id = extension.principal.agent_id;
    let authoritative_teams = extension.principal.teams;

    // The legacy field may only restate the authoritative id (absent or exactly equal).
    if let Some(raw) = legacy {
        let parsed = parse_identity(raw, field)?;
        if parsed != authoritative_id {
            return Err(mismatch_error(field));
        }
    }
    // The explicit principal object may only restate the authoritative id (absent or exactly equal).
    if let Some(principal) = principal.as_ref() {
        let parsed = parse_agent_id(&principal.agent_id)?;
        if parsed != authoritative_id {
            return Err(
                "ERR_PRINCIPAL_MISMATCH: principal.agent_id must match the validated identity"
                    .to_string(),
            );
        }
    }

    // Teams come from the extension (authoritative). Body teams (legacy or principal.teams) may
    // only restate them: absent, or exactly the same set; anything else cannot widen authority.
    restate_only_teams(&authoritative_teams, &legacy_teams)?;
    if let Some(principal) = principal {
        restate_only_teams(&authoritative_teams, &principal.teams)?;
    }

    Ok(ResolvedIdentity {
        agent_id: authoritative_id,
        teams: authoritative_teams,
        operator: extension.principal.operator,
    })
}

/// Parse a body identity for the given field (an `agent_id` UUID or a `viewer` namespace).
fn parse_identity(raw: &str, field: IdentityField) -> Result<Id, String> {
    match field {
        IdentityField::AgentId => parse_agent_id(raw),
        IdentityField::Viewer => parse_viewer(raw),
    }
}

/// The `ERR_PRINCIPAL_MISMATCH` text for a legacy field that contradicts the resolved identity.
fn mismatch_error(field: IdentityField) -> String {
    match field {
        IdentityField::AgentId => {
            "ERR_PRINCIPAL_MISMATCH: agent_id must match the validated identity".to_string()
        }
        IdentityField::Viewer => {
            "ERR_PRINCIPAL_MISMATCH: viewer must match the validated identity".to_string()
        }
    }
}

/// Auth-disabled body agent-id resolution: legacy field or explicit principal, must agree.
fn resolve_body_agent_id(
    legacy: Option<&str>,
    principal: Option<&HostPrincipalToolParam>,
    field: IdentityField,
) -> Result<Id, String> {
    let from_legacy = legacy.map(|raw| parse_identity(raw, field));
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

/// Auth-disabled body team resolution: legacy teams or principal teams, must agree (no extension).
fn resolve_body_teams(
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
    if same_team_set(&legacy_teams, &principal_teams) {
        return Ok(principal_teams);
    }
    Err(
        "ERR_PRINCIPAL_MISMATCH: teams and principal.teams must match when both are supplied"
            .to_string(),
    )
}

/// A body team list may only restate the authoritative (extension) team set: empty, or the exact
/// same set. A different non-empty set would attempt to widen or contradict authority and is
/// refused with `ERR_PRINCIPAL_MISMATCH`.
fn restate_only_teams(authoritative: &[String], body_teams: &[String]) -> Result<(), String> {
    let body_teams = normalize_teams(body_teams.to_vec());
    if body_teams.is_empty() {
        return Ok(());
    }
    let authoritative = normalize_teams(authoritative.to_vec());
    if same_team_set(&authoritative, &body_teams) {
        return Ok(());
    }
    Err("ERR_PRINCIPAL_MISMATCH: teams may only restate the validated identity's teams".to_string())
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
    use super::{
        AuthEnabled, HostPrincipalToolParam, refuse_read_only_write, resolve_reader, resolve_writer,
    };
    use crate::validated::ValidatedPrincipal;
    use crate::{TokenClass, WritePosture};
    use aionforge_domain::ids::Id;
    use aionforge_engine::Principal;

    /// Auth disabled, no validated extension — the long-standing body-only posture every
    /// pre-existing test exercises.
    const OFF: AuthEnabled = AuthEnabled(false);
    /// Auth enabled — the PR4 extension-authoritative / reject-on-absent posture.
    const ON: AuthEnabled = AuthEnabled(true);

    // ---- Auth-disabled (today's behavior, must pass UNCHANGED) ----

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
            None,
            OFF,
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
            None,
            OFF,
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
            None,
            OFF,
        )
        .expect_err("mismatch rejected");
        assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    }

    #[test]
    fn rejects_legacy_teams_when_principal_omits_them() {
        let agent = Id::generate();
        let err = resolve_writer(
            Some(&agent.to_string()),
            vec!["alpha".to_string()],
            Some(HostPrincipalToolParam {
                agent_id: agent.to_string(),
                teams: Vec::new(),
            }),
            None,
            OFF,
        )
        .expect_err("legacy teams cannot extend explicit principal");
        assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    }

    #[test]
    fn accepts_duplicate_matching_team_assertions() {
        let agent = Id::generate();
        let (_agent, teams) = resolve_writer(
            Some(&agent.to_string()),
            vec!["beta".to_string(), "alpha".to_string()],
            Some(HostPrincipalToolParam {
                agent_id: agent.to_string(),
                teams: vec!["alpha".to_string(), "beta".to_string()],
            }),
            None,
            OFF,
        )
        .expect("matching legacy and principal teams resolve");
        assert_eq!(teams, vec!["alpha", "beta"]);
    }

    #[test]
    fn auth_disabled_ignores_a_validated_extension_entirely() {
        // Defense-in-depth invariant: with auth off, even a present extension is ignored and the
        // body identity is used (the extension is always None today; this proves the posture
        // switch, not a merge). Today's behavior is byte-for-byte preserved.
        let body_agent = Id::generate();
        let extension_agent = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::with_operator(extension_agent, vec!["secret".to_string()]),
            WritePosture::Writer,
            TokenClass::Machine,
        );
        let principal = resolve_reader(
            Some(&format!("agent:{body_agent}")),
            Vec::new(),
            None,
            Some(extension),
            OFF,
        )
        .expect("auth-off uses the body identity");
        assert_eq!(
            principal.agent_id, body_agent,
            "the body identity wins when auth is off"
        );
        assert!(
            !principal.operator,
            "the extension's operator bit is not consulted when auth is off"
        );
    }

    // ---- Auth-enabled: reject-on-absent (defense in depth) ----

    #[test]
    fn auth_enabled_with_no_extension_rejects_rather_than_falling_back() {
        // Even with a perfectly good body identity present, an enabled server with no validated
        // extension refuses — it never downgrades to the body identity.
        let agent = Id::generate();
        let reader_err = resolve_reader(
            Some(&format!("agent:{agent}")),
            Vec::new(),
            Some(HostPrincipalToolParam {
                agent_id: agent.to_string(),
                teams: Vec::new(),
            }),
            None,
            ON,
        )
        .expect_err("reject on absent extension");
        assert!(
            reader_err.starts_with("ERR_PRINCIPAL_REQUIRED"),
            "{reader_err}"
        );
        let writer_err = resolve_writer(Some(&agent.to_string()), Vec::new(), None, None, ON)
            .expect_err("reject on absent extension");
        assert!(
            writer_err.starts_with("ERR_PRINCIPAL_REQUIRED"),
            "{writer_err}"
        );
    }

    // ---- Auth-enabled: extension-authoritative precedence ----

    #[test]
    fn extension_is_authoritative_when_body_is_absent() {
        let agent = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::with_operator(agent, vec!["platform".to_string()]),
            WritePosture::Writer,
            TokenClass::Spa,
        );
        let principal = resolve_reader(None, Vec::new(), None, Some(extension), ON)
            .expect("the extension alone resolves");
        assert_eq!(principal.agent_id, agent);
        assert_eq!(principal.teams, vec!["platform".to_string()]);
        assert!(
            principal.operator,
            "the extension's operator bit is authoritative"
        );
    }

    #[test]
    fn body_may_restate_the_extension_exactly() {
        let agent = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::new(agent, vec!["platform".to_string()]),
            WritePosture::Writer,
            TokenClass::Spa,
        );
        // Legacy viewer + principal object both exactly restate the authoritative identity.
        let principal = resolve_reader(
            Some(&format!("agent:{agent}")),
            vec!["platform".to_string()],
            Some(HostPrincipalToolParam {
                agent_id: agent.to_string(),
                teams: vec!["platform".to_string()],
            }),
            Some(extension),
            ON,
        )
        .expect("an exact restatement is accepted");
        assert_eq!(principal.agent_id, agent);
        assert_eq!(principal.teams, vec!["platform".to_string()]);
    }

    #[test]
    fn body_contradicting_the_extension_id_is_rejected() {
        let agent = Id::generate();
        let other = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::Writer,
            TokenClass::Spa,
        );
        let err = resolve_reader(
            Some(&format!("agent:{other}")),
            Vec::new(),
            None,
            Some(extension),
            ON,
        )
        .expect_err("a contradicting body id is rejected");
        assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    }

    #[test]
    fn body_teams_contradicting_the_extension_are_rejected() {
        let agent = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::new(agent, vec!["platform".to_string()]),
            WritePosture::Writer,
            TokenClass::Spa,
        );
        // Body teams attempt to widen authority beyond the extension's set.
        let err = resolve_writer(
            None,
            vec!["platform".to_string(), "payments".to_string()],
            None,
            Some(extension),
            ON,
        )
        .expect_err("body teams may not widen the extension's teams");
        assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    }

    #[test]
    fn principal_object_id_contradicting_the_extension_is_rejected() {
        let agent = Id::generate();
        let other = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::Writer,
            TokenClass::Spa,
        );
        let err = resolve_writer(
            None,
            Vec::new(),
            Some(HostPrincipalToolParam {
                agent_id: other.to_string(),
                teams: Vec::new(),
            }),
            Some(extension),
            ON,
        )
        .expect_err("a contradicting principal.agent_id is rejected");
        assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    }

    // ---- Auth-enabled: read-only write-guard ----

    #[test]
    fn a_read_only_extension_refuses_a_write() {
        let agent = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::ReadOnly,
            TokenClass::Spa,
        );
        let err = resolve_writer(None, Vec::new(), None, Some(extension), ON)
            .expect_err("a read-only identity may not write");
        assert!(err.starts_with("ERR_READ_ONLY_PRINCIPAL"), "{err}");
    }

    #[test]
    fn a_read_only_extension_still_permits_a_read() {
        // The write-guard is write-only: a read-only identity may always read.
        let agent = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::ReadOnly,
            TokenClass::Spa,
        );
        let principal = resolve_reader(None, Vec::new(), None, Some(extension), ON)
            .expect("a read-only identity may read");
        assert_eq!(principal.agent_id, agent);
    }

    #[test]
    fn a_writer_extension_permits_a_write() {
        let agent = Id::generate();
        let extension = ValidatedPrincipal::new(
            Principal::new(agent, vec!["platform".to_string()]),
            WritePosture::Writer,
            TokenClass::Machine,
        );
        let (resolved_agent, teams) = resolve_writer(None, Vec::new(), None, Some(extension), ON)
            .expect("a writer identity may write");
        assert_eq!(resolved_agent, agent);
        assert_eq!(teams, vec!["platform".to_string()]);
    }

    // ---- The shared read-only write-guard (one guard site for resolve_writer AND the point ops) ----

    #[test]
    fn the_shared_write_guard_refuses_only_a_read_only_extension_under_auth() {
        let agent = Id::generate();
        let read_only = ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::ReadOnly,
            TokenClass::Spa,
        );
        let writer = ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::Writer,
            TokenClass::Spa,
        );

        // Auth on + read-only -> refused with ERR_READ_ONLY_PRINCIPAL.
        let err = refuse_read_only_write(Some(&read_only), ON)
            .expect_err("a read-only identity is refused the write");
        assert!(err.starts_with("ERR_READ_ONLY_PRINCIPAL"), "{err}");

        // Auth on + writer -> permitted.
        refuse_read_only_write(Some(&writer), ON).expect("a writer identity passes the guard");

        // Auth on + no extension -> the guard itself does not reject (reject-on-absent is the
        // resolver's job, not this guard's); the guard only fails closed on a read-only posture.
        refuse_read_only_write(None, ON).expect("the guard is silent on an absent extension");

        // Auth off -> the guard is inert even for a read-only extension (today's posture).
        refuse_read_only_write(Some(&read_only), OFF)
            .expect("auth-off never engages the write-guard");
    }
}
