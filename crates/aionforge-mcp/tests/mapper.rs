//! Integration acceptance for the PR3 claims→[`Principal`] mapper at the **MCP layer**.
//!
//! These tests exercise the public `aionforge_mcp::map_verified_claims_to_principal` against the
//! real `aionforge_auth::VerifiedClaims` and `aionforge_config::IssuerConfig` types, pinning the
//! four locked forks end-to-end and the blocking-precondition guarantees the mapper is responsible
//! for:
//!
//! * **(a) host-asserted path always yields `operator == false`.** The mapper is the *only* route
//!   to an operator principal; the host-asserted principal path (`resolve_reader`/`resolve_writer`
//!   in `principal.rs`) never calls it, so a host-asserted principal is never an operator. Pinned
//!   here via `Principal::new`/`Principal::agent`.
//! * **(b) reserved-name-spoof-grants-nothing.** A token asserting `team:"system"`/`"global"` or a
//!   non-allow-listed name confers no membership and no system access.
//! * **(c) reject-on-absent / no-fallback.** When a stable id cannot be derived, the mapper
//!   returns a [`MapError`] — it never falls back to an unanchored or body-asserted identity. Full
//!   HTTP-handler rejection (auth enabled + no `ValidatedPrincipal` in request extensions ⇒ 401,
//!   never a body-asserted fallback) is deferred to PR4; this file pins the mapper-layer invariant
//!   that guarantee will rest on.
//!
//! It also pins the **write-posture** seam (fork#1 soundness): a read-only issuer's principal can
//! carry an *unstable* content-hash id, so it must never be routed into a write path. The mapper
//! returns a [`WritePosture`] alongside the principal; the capture-path enforcement of that posture
//! is the explicit PR4 contract pinned by `a_read_only_issuer_principal_carries_the_read_only_\
//! posture_for_the_pr4_write_guard`.

use std::collections::{BTreeMap, BTreeSet};

use aionforge_auth::VerifiedClaims;
use aionforge_config::IssuerConfig;
use aionforge_domain::ids::Id;
use aionforge_engine::Principal;
use aionforge_mcp::{MapError, TokenClass, WritePosture, map_verified_claims_to_principal};
use serde_json::json;

const ISSUER: &str = "https://issuer.example/";
const TEAMS_CLAIM: &str = "https://aionforge.dev/teams";

fn verified(sub: &str, claims: BTreeMap<String, serde_json::Value>) -> VerifiedClaims {
    VerifiedClaims {
        sub: sub.to_owned(),
        iss: ISSUER.to_owned(),
        aud: "https://api.aionforge.dev".to_owned(),
        claims,
    }
}

/// A read-only issuer (no durable-writer anchor needed) with a `platform` allow-list and a
/// configured operator permission.
fn issuer() -> IssuerConfig {
    let mut allowlist = BTreeSet::new();
    allowlist.insert("platform".to_string());
    IssuerConfig {
        issuer: ISSUER.into(),
        audience: "https://api.aionforge.dev".into(),
        teams_claim: TEAMS_CLAIM.into(),
        teams_allowlist: allowlist,
        operator_permission: Some("console:operate".into()),
        allows_writes: false,
        ..IssuerConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Blocking test (b): reserved-name-spoof-grants-nothing.
// ---------------------------------------------------------------------------

#[test]
fn a_spoofed_reserved_team_grants_no_membership_and_no_system_access() {
    let issuer = issuer();
    let mut claims = BTreeMap::new();
    // A token that tries to assert system/global membership plus a non-allow-listed name.
    claims.insert(
        TEAMS_CLAIM.to_string(),
        json!(["system", "global", "evil-team", "platform"]),
    );
    let (principal, _class, _posture) =
        map_verified_claims_to_principal(&verified("auth0|spoof", claims), &issuer).expect("maps");

    // Only the allow-listed, non-reserved name survives.
    assert_eq!(principal.teams, vec!["platform".to_string()]);
    assert!(!principal.is_member_of("system"), "system was dropped");
    assert!(!principal.is_member_of("global"), "global was dropped");
    assert!(
        !principal.is_member_of("evil-team"),
        "non-allow-listed dropped"
    );
    // A spoofed team never confers the operator/system capability either.
    assert!(!principal.operator, "a team spoof is not an operator");
}

#[test]
fn a_non_allow_listed_name_grants_nothing_when_the_allow_list_is_specific() {
    // allowlist = ["platform"], token asserts only a different name.
    let issuer = issuer();
    let mut claims = BTreeMap::new();
    claims.insert(TEAMS_CLAIM.to_string(), json!(["payments"]));
    let (principal, _class, _posture) =
        map_verified_claims_to_principal(&verified("auth0|x", claims), &issuer).expect("maps");
    assert!(
        principal.teams.is_empty(),
        "a name absent from the allow-list grants nothing"
    );
}

// ---------------------------------------------------------------------------
// Fork#4: operator permission → the operator bit.
// ---------------------------------------------------------------------------

#[test]
fn the_operator_permission_mints_an_operator_principal() {
    let issuer = issuer();
    let mut claims = BTreeMap::new();
    claims.insert("permissions".to_string(), json!(["console:operate"]));
    let (principal, _class, _posture) =
        map_verified_claims_to_principal(&verified("auth0|op", claims), &issuer).expect("maps");
    assert!(
        principal.operator,
        "a validated token with the configured operator permission yields operator=true"
    );

    // And even a minted operator never survives a wire round-trip (defence in depth).
    let json = serde_json::to_string(&principal).expect("serialize");
    let back: Principal = serde_json::from_str(&json).expect("deserialize");
    assert!(
        !back.operator,
        "the operator capability never persists across the wire"
    );
}

#[test]
fn a_token_without_the_operator_permission_is_not_an_operator() {
    let issuer = issuer();
    let mut claims = BTreeMap::new();
    claims.insert("permissions".to_string(), json!(["read:logs"]));
    let (principal, _class, _posture) =
        map_verified_claims_to_principal(&verified("auth0|x", claims), &issuer).expect("maps");
    assert!(!principal.operator);
}

// ---------------------------------------------------------------------------
// Blocking test (a): the host-asserted path always yields operator=false.
// ---------------------------------------------------------------------------

#[test]
fn the_host_asserted_principal_path_is_never_an_operator() {
    // The host-asserted path (`Principal::new`/`Principal::agent`, used by resolve_reader/
    // resolve_writer in principal.rs) does not run the mapper and thus never mints an operator.
    assert!(!Principal::new(Id::generate(), vec!["platform".into()]).operator);
    assert!(!Principal::agent(Id::generate()).operator);
}

// ---------------------------------------------------------------------------
// Fork#2: M2M-vs-SPA classification.
// ---------------------------------------------------------------------------

#[test]
fn an_m2m_token_classifies_distinctly_from_a_spa_token() {
    let issuer = issuer();

    let mut machine = BTreeMap::new();
    machine.insert("aud".to_string(), json!(["api://one", "api://two"]));
    let (_p, class, _posture) =
        map_verified_claims_to_principal(&verified("svc|robot", machine), &issuer).expect("maps");
    assert_eq!(class, TokenClass::Machine);

    let mut spa = BTreeMap::new();
    spa.insert("aud".to_string(), json!("https://api.aionforge.dev"));
    let (_p, class, _posture) =
        map_verified_claims_to_principal(&verified("auth0|human", spa), &issuer).expect("maps");
    assert_eq!(class, TokenClass::Spa);
}

// ---------------------------------------------------------------------------
// Fork#1: id stability + the collision-resistance guarantee.
// ---------------------------------------------------------------------------

#[test]
fn id_resolution_prefers_override_then_claim_then_read_only_hash() {
    // (1) Override wins outright.
    let anchor = Id::from_content_hash(b"the-durable-anchor");
    let mut writer = issuer();
    writer.allows_writes = true;
    writer.agent_id_overrides.insert("auth0|w".into(), anchor);
    let (p, _c, _posture) =
        map_verified_claims_to_principal(&verified("auth0|w", BTreeMap::new()), &writer)
            .expect("override anchors writer");
    assert_eq!(p.agent_id, anchor);

    // (2) With no override, a custom UUID claim anchors.
    let claimed = Id::generate();
    let mut claim_writer = issuer();
    claim_writer.allows_writes = true;
    claim_writer.agent_id_claim = Some("https://aionforge.dev/agent_id".into());
    let mut claims = BTreeMap::new();
    claims.insert(
        "https://aionforge.dev/agent_id".to_string(),
        json!(claimed.to_string()),
    );
    let (p, _c, _posture) =
        map_verified_claims_to_principal(&verified("auth0|w", claims), &claim_writer)
            .expect("claim anchors writer");
    assert_eq!(p.agent_id, claimed);

    // (3) Read-only with no anchor falls back to a deterministic, stable id.
    let read_only = issuer(); // allows_writes = false
    let (a, _, _) =
        map_verified_claims_to_principal(&verified("auth0|r", BTreeMap::new()), &read_only)
            .expect("read-only fallback");
    let (b, _, _) =
        map_verified_claims_to_principal(&verified("auth0|r", BTreeMap::new()), &read_only)
            .expect("read-only fallback");
    assert_eq!(a.agent_id, b.agent_id, "the fallback id is deterministic");
}

#[test]
fn distinct_subjects_and_issuers_never_collide_in_the_read_only_fallback() {
    let read_only = issuer();
    let (a, _, _) =
        map_verified_claims_to_principal(&verified("sub-one", BTreeMap::new()), &read_only)
            .expect("maps");
    let (b, _, _) =
        map_verified_claims_to_principal(&verified("sub-two", BTreeMap::new()), &read_only)
            .expect("maps");
    assert_ne!(a.agent_id, b.agent_id, "distinct subs ⇒ distinct ids");

    // A different issuer with the same sub is a different principal.
    let mut other_issuer = issuer();
    other_issuer.issuer = "https://other.example/".into();
    let mut other = verified("sub-one", BTreeMap::new());
    other.iss = "https://other.example/".into();
    let (c, _, _) = map_verified_claims_to_principal(&other, &other_issuer).expect("maps");
    assert_ne!(a.agent_id, c.agent_id, "distinct issuers ⇒ distinct ids");
}

// ---------------------------------------------------------------------------
// Blocking test (c): reject-on-absent / no silent fallback for a durable writer.
// ---------------------------------------------------------------------------

#[test]
fn a_writer_capable_issuer_without_an_anchor_is_refused_not_silently_hashed() {
    let mut writer = issuer();
    writer.allows_writes = true; // writer, but no override and no agent_id_claim
    let err = map_verified_claims_to_principal(&verified("auth0|w", BTreeMap::new()), &writer)
        .expect_err("an unanchored durable writer must be refused");
    assert_eq!(
        err,
        MapError::UnanchoredWriter,
        "the mapper refuses rather than minting an unstable id"
    );
}

#[test]
fn an_empty_subject_is_refused() {
    let read_only = issuer();
    let err = map_verified_claims_to_principal(&verified("", BTreeMap::new()), &read_only)
        .expect_err("no subject ⇒ no stable id");
    assert_eq!(err, MapError::MissingSubject);
}

// ---------------------------------------------------------------------------
// Fork#1 soundness seam: the write posture travels with the principal so the PR4
// write path can fail closed on a read-only/ephemeral identity (whose content-hash
// id is unstable across an issuer/sub migration). The mapper itself does not write,
// so PR3 pins the *posture*; the capture-path refusal is the documented PR4 contract.
// ---------------------------------------------------------------------------

#[test]
fn a_read_only_issuer_principal_carries_the_read_only_posture_for_the_pr4_write_guard() {
    // A read-only issuer with no anchor mints a content-hash id (sound ONLY because the principal
    // must never write). The returned WritePosture::ReadOnly is the enforceable marker the PR4
    // write path will consult to refuse a durable write under that unstable id.
    let read_only = issuer(); // allows_writes = false, no anchor
    let (principal, _class, posture) =
        map_verified_claims_to_principal(&verified("auth0|ephemeral", BTreeMap::new()), &read_only)
            .expect("read-only fallback maps");
    assert_eq!(
        posture,
        WritePosture::ReadOnly,
        "a read-only/ephemeral principal MUST be marked ReadOnly so the write path can refuse it"
    );
    // The principal looks like any other (no read-only marker on the Principal itself), which is
    // exactly why the posture must travel alongside: the write path cannot re-derive it.
    assert!(!principal.operator);

    // An anchored writer, by contrast, is Writer (its id is stable, so a durable write is sound).
    let mut writer = issuer();
    writer.allows_writes = true;
    writer
        .agent_id_overrides
        .insert("auth0|durable".into(), Id::from_content_hash(b"anchor"));
    let (_p, _class, writer_posture) =
        map_verified_claims_to_principal(&verified("auth0|durable", BTreeMap::new()), &writer)
            .expect("anchored writer maps");
    assert_eq!(writer_posture, WritePosture::Writer);
}
