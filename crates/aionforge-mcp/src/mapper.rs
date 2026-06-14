//! Map verified OAuth claims to an in-process [`Principal`] (PR3 of the OAuth workstream).
//!
//! This is the seam between the resource-server token validator
//! ([`aionforge_auth::JwtValidator`], which produces [`VerifiedClaims`]) and the substrate's
//! security model ([`aionforge_domain`]'s [`Principal`]). It deliberately lives in the **MCP
//! crate**, not in `aionforge-auth`:
//!
//! * `aionforge-auth` is the fork#3 *domain-free quarantine* — no `aionforge-domain` type may
//!   enter it, so it cannot mint a [`Principal`]. The mapper is the first place both
//!   `aionforge-auth` ([`VerifiedClaims`]) and `aionforge-domain` ([`Principal`]) meet.
//! * The MCP crate already owns host-principal resolution (`principal.rs`), so claims→principal
//!   resolution is the same layer's concern.
//!
//! # The four locked forks
//!
//! The mapper applies the owner-locked design forks, in order:
//!
//! 1. **ID stability (fork#1).** A principal that may write durable memory needs a *stable* id, so
//!    it keeps its private namespace across an issuer/`sub` migration. The id is resolved in
//!    priority order: the issuer's `agent_id_overrides[sub]` map, then an immutable custom
//!    `agent_id_claim`, and only as a last resort [`Id::from_content_hash`] over `iss ++ sub`. The
//!    content-hash fallback is sound **only for a read-only/ephemeral issuer**; a writer-capable
//!    issuer that reaches it has a misconfiguration that can orphan a namespace, so the mapper
//!    emits a **loud** `tracing::warn!` (mirroring the config layer's startup warning). Because the
//!    fallback id is unstable, the soundness of the read-only case rests on the principal never
//!    writing; the mapper returns a [`WritePosture`] alongside the principal so the write path can
//!    fail closed on a [`ReadOnly`](WritePosture::ReadOnly) identity (the [`Principal`] itself
//!    carries no read-only marker). Enforcing that refusal in the capture path is the PR4 contract
//!    (see [`WritePosture`]).
//! 2. **M2M-vs-SPA classification.** A machine token (`aud` is a JSON array — the RFC 8707
//!    multi-resource shape Auth0 issues to M2M apps) is distinguished from a human SPA token
//!    (`aud` is a scalar) and recorded for audit. The classification does not change authority; it
//!    is provenance.
//! 3. **Teams allow-list.** Only team names present in the issuer's `teams_allowlist` are admitted.
//!    A reserved or spoofed name — `system`, `global`, or any value not on the allow-list — is
//!    **dropped**, granting nothing. An empty allow-list therefore grants no teams (fail-closed).
//! 4. **Operator permission → the operator bit.** The operator bit is set **only** when the token
//!    carries the issuer's configured `operator_permission` in its permissions array, via the
//!    server-only [`Principal::with_operator`] constructor. An issuer with no configured operator
//!    permission mints no operators. The host-asserted principal path (`principal.rs`) never calls
//!    this mapper, so it always yields `operator == false`.

use aionforge_auth::VerifiedClaims;
use aionforge_config::IssuerConfig;
use aionforge_domain::ids::Id;
use aionforge_engine::Principal;

/// The two reserved namespace names that can never be granted as a team, regardless of the
/// allow-list. A token asserting either is a spoof attempt; both are dropped silently (the audit
/// of the drop is the caller's concern). They are blocked *in addition to* the allow-list check —
/// a deployment that mistakenly allow-lists `"system"` still cannot grant it.
const RESERVED_TEAM_NAMES: [&str; 2] = ["system", "global"];

/// The claim conventionally carrying an Auth0 RBAC permissions array. Auth0 emits granted
/// permissions under `permissions` (not URI-namespaced, as it is a registered Auth0 claim).
const PERMISSIONS_CLAIM: &str = "permissions";

/// Whether a verified token presented a machine (M2M) audience or a human (SPA) audience.
///
/// The distinction is provenance, recorded for audit; it does not change authority. An M2M token's
/// `aud` is a JSON array (the RFC 8707 multi-resource shape Auth0 issues to machine clients); a
/// human SPA token's `aud` is a scalar string. A token with no raw `aud` array (already collapsed
/// to the matched audience) is treated as a SPA token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenClass {
    /// A machine-to-machine token (array-valued `aud`).
    Machine,
    /// A human single-page-application token (scalar `aud`).
    Spa,
}

/// Whether the issuer that minted this principal permits it to write durable memory.
///
/// # Why this is returned alongside the [`Principal`], not folded into it
///
/// The [`Principal`] deliberately carries no read-only marker: its `operator` bit is the only
/// capability on it, it is deserialize-locked, and adding a field would either widen its wire
/// shape or need another forced-default hook. Instead the *posture* is returned here so a caller
/// can fail closed **before** routing the principal into a write path.
///
/// # Why the posture must be enforced (the soundness contract)
///
/// The content-hash fallback id (`content_hash_id`, a pure function of `iss ++ sub`) is **not**
/// stable across an issuer/`sub` migration. `resolve_agent_id` therefore mints it **only** when
/// `issuer_config.allows_writes == false` — its soundness rests entirely on the assumption that
/// such a principal never writes durable memory (a write under an unstable id orphans the namespace
/// the moment the issuer rotates or the token shape changes — the very `UnanchoredWriter` condition
/// the writer path refuses to tolerate). A [`ReadOnly`](WritePosture::ReadOnly) principal is
/// byte-for-byte indistinguishable from a writer at the [`Principal`] level, so the write path
/// cannot re-derive this; it MUST consult the posture returned here.
///
/// **PR4 contract (enforced by `a_read_only_principal_is_marked_read_only`):** when PR4 wires the
/// mapper's principal into the capture path, a [`ReadOnly`](WritePosture::ReadOnly) principal MUST
/// be refused write authorization (it may only read). Until that wiring exists, this is the
/// single, testable seam that keeps the unstable-id soundness assumption honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePosture {
    /// The issuer permits durable writes (`allows_writes == true`). Its id is anchored by an
    /// override or a custom claim (an unanchored writer is refused upstream), so writing is sound.
    Writer,
    /// The issuer is read-only/ephemeral (`allows_writes == false`). This principal may have an
    /// unstable content-hash id; it MUST NOT be routed into a write path (see the type docs).
    ReadOnly,
}

/// Why mapping a verified token to a [`Principal`] failed.
///
/// A failure is *fail-closed*: the caller must reject the request, never fall back to a
/// body-asserted identity. No variant embeds a token or a raw claim value, so an error is safe to
/// log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    /// The token presented no usable subject (`sub`), so no stable id could be derived.
    #[error("token has no usable sub claim for principal mapping")]
    MissingSubject,
    /// The configured `agent_id_claim` was present but did not hold a valid agent UUID.
    #[error("the configured agent_id_claim did not hold a valid agent UUID")]
    InvalidAgentIdClaim,
    /// A writer-capable issuer had no durable-writer anchor (no `agent_id_overrides` entry and no
    /// `agent_id_claim`), so a content-hash id would orphan the namespace on an issuer/sub
    /// migration. The mapper refuses rather than silently mint an unstable writer id.
    #[error(
        "a writer-capable issuer must anchor its agent id (set agent_id_overrides or \
         agent_id_claim); refusing to mint an unstable content-hash id for a durable writer"
    )]
    UnanchoredWriter,
}

/// Map a successfully validated token to an in-process [`Principal`], applying the four locked
/// forks (id stability, M2M/SPA classification, teams allow-list, operator permission).
///
/// The returned principal's `operator` bit is set **only** when the token carries the issuer's
/// configured operator permission; teams are filtered to the issuer's allow-list with reserved
/// names dropped; and the agent id is resolved with fork#1 stability. Two posture values are
/// returned alongside the principal:
///
/// * [`TokenClass`] — M2M vs SPA provenance, for the caller's audit trail.
/// * [`WritePosture`] — whether the issuer permits durable writes. A
///   [`ReadOnly`](WritePosture::ReadOnly) principal may carry an unstable content-hash id and MUST
///   NOT be routed into a write path; the caller (PR4) fails closed on it. See [`WritePosture`].
///
/// # Errors
/// Returns [`MapError`] when no stable id can be derived: a missing subject, an `agent_id_claim`
/// present but not a valid UUID, or a writer-capable issuer with no durable-writer anchor.
pub fn map_verified_claims_to_principal(
    verified: &VerifiedClaims,
    issuer_config: &IssuerConfig,
) -> Result<(Principal, TokenClass, WritePosture), MapError> {
    let agent_id = resolve_agent_id(verified, issuer_config)?;
    let class = classify_token(verified);
    let teams = allow_listed_teams(verified, issuer_config);
    let is_operator = token_grants_operator(verified, issuer_config);
    // The write posture travels with the principal so the write path can fail closed on a
    // read-only/ephemeral identity (whose id may be an unstable content hash); it is NOT folded
    // onto the deserialize-locked Principal. See `WritePosture` for the soundness contract.
    let posture = if issuer_config.allows_writes {
        WritePosture::Writer
    } else {
        WritePosture::ReadOnly
    };

    let principal = if is_operator {
        Principal::with_operator(agent_id, teams)
    } else {
        Principal::new(agent_id, teams)
    };
    Ok((principal, class, posture))
}

/// Resolve the agent id with fork#1 stability: override map, then custom claim, then (read-only
/// only) a content-hash fallback.
fn resolve_agent_id(
    verified: &VerifiedClaims,
    issuer_config: &IssuerConfig,
) -> Result<Id, MapError> {
    if verified.sub.is_empty() {
        return Err(MapError::MissingSubject);
    }

    // 1. An explicit sub→agent_id override is the strongest anchor.
    if let Some(id) = issuer_config.agent_id_overrides.get(&verified.sub) {
        return Ok(*id);
    }

    // 2. An immutable custom claim carrying a stable agent UUID.
    if let Some(claim_name) = &issuer_config.agent_id_claim {
        let raw = verified
            .claims
            .get(claim_name)
            .and_then(serde_json::Value::as_str)
            .ok_or(MapError::InvalidAgentIdClaim)?;
        return Id::parse(raw).map_err(|_| MapError::InvalidAgentIdClaim);
    }

    // 3. Content-hash fallback over `iss ++ sub`. Sound only for a read-only/ephemeral issuer; a
    //    writer-capable issuer that reaches here is misconfigured and would orphan its namespace.
    if issuer_config.allows_writes {
        // Loud, mirroring the config layer's startup warning — the issuer is named by its origin,
        // never by a token value, so nothing sensitive is logged.
        tracing::warn!(
            issuer = %issuer_config.issuer,
            "writer-capable issuer has no agent-id anchor (agent_id_overrides/agent_id_claim); \
             refusing to mint an unstable content-hash id for a durable writer"
        );
        return Err(MapError::UnanchoredWriter);
    }
    Ok(content_hash_id(&verified.iss, &verified.sub))
}

/// Derive the read-only/ephemeral fallback id from `iss ++ sub`. A `\u{1f}` unit separator
/// guards against a boundary-collision where one issuer's suffix concatenated with a sub equals
/// another issuer ++ sub (e.g. `iss="a", sub="bc"` vs `iss="ab", sub="c"`).
fn content_hash_id(iss: &str, sub: &str) -> Id {
    let mut bytes = Vec::with_capacity(iss.len() + sub.len() + 1);
    bytes.extend_from_slice(iss.as_bytes());
    bytes.push(0x1f);
    bytes.extend_from_slice(sub.as_bytes());
    Id::from_content_hash(&bytes)
}

/// Classify the token by its raw `aud` shape (array ⇒ machine, scalar/absent ⇒ SPA).
fn classify_token(verified: &VerifiedClaims) -> TokenClass {
    match verified.claims.get("aud") {
        Some(serde_json::Value::Array(_)) => TokenClass::Machine,
        _ => TokenClass::Spa,
    }
}

/// Extract the token's team claims and admit only allow-listed, non-reserved names.
///
/// Fail-closed at every step: a missing/non-array claim yields no teams, a non-string entry is
/// skipped, a reserved name (`system`/`global`) is dropped even if mistakenly allow-listed, and a
/// name absent from `teams_allowlist` is dropped. An empty allow-list therefore grants no teams.
fn allow_listed_teams(verified: &VerifiedClaims, issuer_config: &IssuerConfig) -> Vec<String> {
    let Some(serde_json::Value::Array(values)) = verified.claims.get(&issuer_config.teams_claim)
    else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|name| !RESERVED_TEAM_NAMES.contains(name))
        .filter(|name| issuer_config.teams_allowlist.contains(*name))
        .map(str::to_owned)
        .collect()
}

/// Whether the token grants the operator bit: the issuer must configure an `operator_permission`
/// AND the token's `permissions` array must contain exactly that permission.
fn token_grants_operator(verified: &VerifiedClaims, issuer_config: &IssuerConfig) -> bool {
    let Some(required) = &issuer_config.operator_permission else {
        return false;
    };
    let Some(serde_json::Value::Array(values)) = verified.claims.get(PERMISSIONS_CLAIM) else {
        return false;
    };
    values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .any(|permission| permission == required)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aionforge_config::IssuerConfig;
    use serde_json::json;

    use super::*;

    fn claims(map: BTreeMap<String, serde_json::Value>) -> VerifiedClaims {
        VerifiedClaims {
            sub: "auth0|abc".to_owned(),
            iss: "https://issuer.example/".to_owned(),
            aud: "https://api.aionforge.dev".to_owned(),
            claims: map,
        }
    }

    fn read_only_issuer() -> IssuerConfig {
        IssuerConfig {
            issuer: "https://issuer.example/".into(),
            audience: "https://api.aionforge.dev".into(),
            allows_writes: false,
            ..IssuerConfig::default()
        }
    }

    #[test]
    fn content_hash_fallback_is_stable_and_collision_free_at_the_iss_sub_boundary() {
        // Same inputs ⇒ same id.
        assert_eq!(
            content_hash_id("https://a/", "bob"),
            content_hash_id("https://a/", "bob")
        );
        // The separator defeats the classic concatenation collision: (iss="ab", sub="c") must NOT
        // hash to the same id as (iss="a", sub="bc").
        assert_ne!(content_hash_id("ab", "c"), content_hash_id("a", "bc"));
        // Distinct issuers with the same sub are distinct principals.
        assert_ne!(
            content_hash_id("https://a/", "bob"),
            content_hash_id("https://b/", "bob")
        );
    }

    #[test]
    fn a_read_only_issuer_falls_back_to_a_content_hash_id() {
        let verified = claims(BTreeMap::new());
        let (principal, _class, posture) =
            map_verified_claims_to_principal(&verified, &read_only_issuer()).expect("maps");
        assert_eq!(
            principal.agent_id,
            content_hash_id(&verified.iss, &verified.sub)
        );
        assert!(!principal.operator);
        assert!(principal.teams.is_empty());
        // A read-only issuer's principal carries the read-only posture so the write path can fail
        // closed on its (possibly unstable) content-hash id.
        assert_eq!(posture, WritePosture::ReadOnly);
    }

    #[test]
    fn a_writer_issuer_without_an_anchor_is_refused() {
        let writer = IssuerConfig {
            allows_writes: true,
            ..read_only_issuer()
        };
        let err = map_verified_claims_to_principal(&claims(BTreeMap::new()), &writer)
            .expect_err("an unanchored writer is refused");
        assert_eq!(err, MapError::UnanchoredWriter);
    }

    #[test]
    fn an_override_map_anchors_a_writer() {
        let anchor = Id::from_content_hash(b"durable-writer");
        let mut writer = IssuerConfig {
            allows_writes: true,
            ..read_only_issuer()
        };
        writer.agent_id_overrides.insert("auth0|abc".into(), anchor);
        let (principal, _class, posture) =
            map_verified_claims_to_principal(&claims(BTreeMap::new()), &writer).expect("maps");
        assert_eq!(principal.agent_id, anchor, "the override wins");
        // An anchored writer carries the writer posture.
        assert_eq!(posture, WritePosture::Writer);
    }

    #[test]
    fn a_custom_agent_id_claim_anchors_a_writer_and_a_bad_one_is_refused() {
        let anchor = Id::generate();
        let mut writer = IssuerConfig {
            allows_writes: true,
            agent_id_claim: Some("https://aionforge.dev/agent_id".into()),
            ..read_only_issuer()
        };
        writer.issuer = "https://issuer.example/".into();

        // A valid UUID claim anchors the writer.
        let mut map = BTreeMap::new();
        map.insert(
            "https://aionforge.dev/agent_id".to_string(),
            json!(anchor.to_string()),
        );
        let (principal, _class, _posture) =
            map_verified_claims_to_principal(&claims(map), &writer).expect("maps");
        assert_eq!(principal.agent_id, anchor);

        // A non-UUID claim is refused (not silently hashed).
        let mut bad = BTreeMap::new();
        bad.insert(
            "https://aionforge.dev/agent_id".to_string(),
            json!("not-a-uuid"),
        );
        assert_eq!(
            map_verified_claims_to_principal(&claims(bad), &writer).expect_err("bad claim"),
            MapError::InvalidAgentIdClaim
        );
    }

    #[test]
    fn a_machine_aud_array_classifies_as_m2m_and_a_scalar_as_spa() {
        let mut machine = BTreeMap::new();
        machine.insert("aud".to_string(), json!(["api://a", "api://b"]));
        let (_p, class, _posture) =
            map_verified_claims_to_principal(&claims(machine), &read_only_issuer()).expect("maps");
        assert_eq!(class, TokenClass::Machine);

        let mut spa = BTreeMap::new();
        spa.insert("aud".to_string(), json!("https://api.aionforge.dev"));
        let (_p, class, _posture) =
            map_verified_claims_to_principal(&claims(spa), &read_only_issuer()).expect("maps");
        assert_eq!(class, TokenClass::Spa);
    }

    #[test]
    fn only_allow_listed_non_reserved_teams_are_admitted() {
        let mut issuer = read_only_issuer();
        issuer.teams_claim = "https://aionforge.dev/teams".into();
        issuer.teams_allowlist.insert("platform".into());
        issuer.teams_allowlist.insert("payments".into());
        // Even if a deployment mistakenly allow-lists a reserved name, it is still dropped.
        issuer.teams_allowlist.insert("system".into());

        let mut map = BTreeMap::new();
        map.insert(
            "https://aionforge.dev/teams".to_string(),
            json!([
                "platform",
                "system",
                "global",
                "not-allow-listed",
                "payments"
            ]),
        );
        let (principal, _class, _posture) =
            map_verified_claims_to_principal(&claims(map), &issuer).expect("maps");
        assert_eq!(
            principal.teams,
            vec!["platform".to_string(), "payments".to_string()],
            "only allow-listed, non-reserved names survive"
        );
    }

    #[test]
    fn an_empty_allow_list_grants_no_teams() {
        let issuer = read_only_issuer(); // default allow-list is empty
        let mut map = BTreeMap::new();
        map.insert(issuer.teams_claim.clone(), json!(["platform", "anything"]));
        let (principal, _class, _posture) =
            map_verified_claims_to_principal(&claims(map), &issuer).expect("maps");
        assert!(
            principal.teams.is_empty(),
            "an empty allow-list is fail-closed: no team is granted"
        );
    }

    #[test]
    fn the_operator_bit_requires_the_configured_permission() {
        let mut issuer = read_only_issuer();
        issuer.operator_permission = Some("console:operate".into());

        // Token carrying the permission ⇒ operator.
        let mut with_perm = BTreeMap::new();
        with_perm.insert(
            "permissions".to_string(),
            json!(["read:logs", "console:operate"]),
        );
        let (principal, _class, _posture) =
            map_verified_claims_to_principal(&claims(with_perm), &issuer).expect("maps");
        assert!(
            principal.operator,
            "the configured permission grants operator"
        );

        // Token without the permission ⇒ not operator.
        let mut without = BTreeMap::new();
        without.insert("permissions".to_string(), json!(["read:logs"]));
        let (principal, _class, _posture) =
            map_verified_claims_to_principal(&claims(without), &issuer).expect("maps");
        assert!(!principal.operator);
    }

    #[test]
    fn an_issuer_with_no_operator_permission_mints_no_operators() {
        let issuer = read_only_issuer(); // operator_permission is None
        let mut map = BTreeMap::new();
        // Even a token literally carrying "console:operate" gets nothing if the issuer configures
        // no operator permission.
        map.insert("permissions".to_string(), json!(["console:operate"]));
        let (principal, _class, _posture) =
            map_verified_claims_to_principal(&claims(map), &issuer).expect("maps");
        assert!(!principal.operator);
    }

    #[test]
    fn the_write_posture_tracks_allows_writes() {
        // A read-only issuer's principal is ReadOnly (its content-hash id may be unstable, so the
        // write path must refuse it); an anchored writer's principal is Writer.
        let (_p, _c, read_only_posture) =
            map_verified_claims_to_principal(&claims(BTreeMap::new()), &read_only_issuer())
                .expect("maps");
        assert_eq!(read_only_posture, WritePosture::ReadOnly);

        let mut writer = IssuerConfig {
            allows_writes: true,
            ..read_only_issuer()
        };
        writer
            .agent_id_overrides
            .insert("auth0|abc".into(), Id::from_content_hash(b"anchor"));
        let (_p, _c, writer_posture) =
            map_verified_claims_to_principal(&claims(BTreeMap::new()), &writer).expect("maps");
        assert_eq!(writer_posture, WritePosture::Writer);
    }
}
