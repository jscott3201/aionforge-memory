//! Blocking-precondition acceptance for the PR3 operator capability at the **domain layer**.
//!
//! PR3 adds a server-set-only `operator` bit to [`Principal`] and an
//! [`OperatorAwareAuthorizer`] that grants system visibility to an operator. This file pins the
//! domain-layer half of the three blocking-precondition tests; the other two layers (config
//! validation and the claims→Principal mapper) are pinned where they actually live, because the
//! domain crate must not depend on `aionforge-config` or `aionforge-mcp`:
//!
//! * **(a) operator-bit-cannot-be-body-set** — pinned *here* in full: no JSON body can set the
//!   operator bit, an operator principal never survives a wire round-trip, and the operator
//!   authority grants the system-surface *capability* only to an operator — while leaving the
//!   namespace gate to lift in lockstep with the caller's `include_system` opt-in at each read
//!   site (it never pre-widens the visible set, which would leak system content on an ordinary
//!   recall). (The host-asserted MCP path always yielding `operator == false` is additionally
//!   pinned at the MCP layer, where that path lives.)
//! * **(b) teams-allow-list fail-closed + reserved-name-spoof-grants-nothing** — the
//!   `Config::validate` half lives in `aionforge-config` (the allow-list is a config concern); the
//!   JWT-asserts-`team:"system"`/`"global"`/non-allow-listed spoof half lives in
//!   `aionforge-mcp/tests/mapper.rs`, where the mapper that enforces the allow-list lives. This
//!   file pins the domain invariant the spoof relies on: a `system`/`global` name in a
//!   [`Principal`]'s team list grants **no** access through the default or operator authority.
//! * **(c) reject-on-absent-Principal** — documented here as deferred to PR4. PR3 introduces no
//!   HTTP handler, so there is no request path to reject at; the rejection invariant is encoded as
//!   the mapper returning an error rather than a fallback identity (pinned at the MCP layer) and
//!   the standing absence of any wire route to `operator == true` (pinned here). Full
//!   handler-level rejection (a request with auth enabled and no `ValidatedPrincipal` in the
//!   request extensions is refused, never falling back to a body-asserted identity) is PR4 scope.

use aionforge_domain::authz::{Authorizer, DefaultAuthorizer, OperatorAwareAuthorizer, Principal};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;

fn agent_id(seed: &[u8]) -> Id {
    Id::from_content_hash(seed)
}

// ---------------------------------------------------------------------------
// Blocking test (a): the operator bit can never be set from an untrusted body.
// ---------------------------------------------------------------------------

#[test]
fn operator_bit_cannot_be_body_set() {
    // A request body asserting `"operator": true` is forced to `false` by the deserialize hook.
    let body_true = format!(
        r#"{{"agent_id":"{}","teams":["squad"],"operator":true}}"#,
        agent_id(b"victim")
    );
    let from_body: Principal = serde_json::from_str(&body_true).expect("deserialize");
    assert!(
        !from_body.operator,
        "a body-asserted operator:true MUST deserialize to operator=false"
    );

    // Explicit false and an omitted field also yield false.
    for json in [
        format!(
            r#"{{"agent_id":"{}","teams":[],"operator":false}}"#,
            agent_id(b"a")
        ),
        format!(r#"{{"agent_id":"{}","teams":[]}}"#, agent_id(b"a")),
    ] {
        assert!(
            !serde_json::from_str::<Principal>(&json)
                .expect("deserialize")
                .operator
        );
    }

    // Any non-bool shape in the operator field is also accepted and ignored — it must neither
    // forge the bit nor reject the whole principal (the `IgnoredAny` deserialize hook discards it).
    for raw in ["\"true\"", "1", "null", "{}", "[]"] {
        let json = format!(
            r#"{{"agent_id":"{}","teams":[],"operator":{raw}}}"#,
            agent_id(b"a")
        );
        let principal: Principal = serde_json::from_str(&json)
            .expect("a non-bool operator field must be ignored, not error the deserialize");
        assert!(
            !principal.operator,
            "a non-bool operator body value ({raw}) MUST yield operator=false"
        );
    }
}

#[test]
fn an_operator_principal_never_survives_serialization() {
    // The only route to operator=true is the server-side constructor; it never survives the wire.
    let op = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);
    assert!(op.operator);
    let json = serde_json::to_string(&op).expect("serialize");
    let back: Principal = serde_json::from_str(&json).expect("deserialize");
    assert!(
        !back.operator,
        "operator is a live capability, never a persisted/forgeable value"
    );
}

#[test]
fn the_operator_authorizer_grants_the_capability_only_to_operators() {
    // The capability (`may_surface_system`) is the ONLY thing the operator authority grants.
    // It is granted to an operator and withheld from everyone else.
    let authz = OperatorAwareAuthorizer::new(DefaultAuthorizer);
    let regular = Principal::new(agent_id(b"reg"), vec!["squad".into()]);
    let operator = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);

    assert!(!authz.may_surface_system(&regular));
    assert!(authz.may_surface_system(&operator));
}

#[test]
fn the_operator_authorizer_does_not_pre_widen_the_visible_set() {
    // The authority must NOT widen `visible_namespaces` with the system namespace, not even for
    // an operator: there is no `include_system` flag in scope here, so any widening would be
    // unconditional and would leak system-namespace content (episodes and core blocks) on every
    // operator recall regardless of the caller's opt-in. The widening is the call site's job.
    let authz = OperatorAwareAuthorizer::new(DefaultAuthorizer);
    let regular = Principal::new(agent_id(b"reg"), vec!["squad".into()]);
    let operator = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);

    assert!(
        !authz
            .visible_namespaces(&regular)
            .contains(&Namespace::System),
        "a non-operator never sees the system namespace"
    );
    assert!(
        !authz
            .visible_namespaces(&operator)
            .contains(&Namespace::System),
        "the authority must NOT pre-widen an operator's set with the system namespace; the call \
         site widens in lockstep with include_system"
    );
}

#[test]
fn an_operator_surfaces_system_only_when_the_caller_opts_in_at_an_and_gated_site() {
    // The blocking-precondition this PR cares about, exercised through the *call-site composition*
    // every AND-gated read site (engine recall, read_memory, retriever) uses, rather than the
    // authority in isolation. This is the test that catches the over-widening leak: an operator on
    // an ordinary (include_system=false) recall must NOT surface the system namespace; only an
    // explicit include_system=true opt-in lifts the namespace gate. A non-operator never surfaces
    // it, opt-in or not.
    let authz = OperatorAwareAuthorizer::new(DefaultAuthorizer);
    let operator = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);
    let regular = Principal::new(agent_id(b"reg"), vec!["squad".into()]);

    // Reproduce the exact site composition: visible = visible_namespaces(p); surface_system =
    // include_system && may_surface_system(p); if surface_system { visible.with_system() }.
    let compose = |principal: &Principal, include_system: bool| {
        let mut visible = authz.visible_namespaces(principal);
        let surface_system = include_system && authz.may_surface_system(principal);
        if surface_system {
            visible = visible.with_system();
        }
        visible
    };

    assert!(
        !compose(&operator, false).contains(&Namespace::System),
        "an operator on an include_system=false recall must NOT surface the system namespace"
    );
    assert!(
        compose(&operator, true).contains(&Namespace::System),
        "an operator opting in with include_system=true surfaces the system namespace"
    );
    assert!(
        !compose(&regular, true).contains(&Namespace::System),
        "a non-operator never surfaces the system namespace, opt-in or not"
    );
}

// ---------------------------------------------------------------------------
// Blocking test (b), domain half: a reserved name in a principal's team list
// grants nothing. The mapper (MCP layer) is what *drops* such names before they
// ever reach a Principal; this pins that even if one leaked in, neither the
// default nor the operator authority would honour it as system access.
// ---------------------------------------------------------------------------

#[test]
fn a_reserved_team_name_in_a_principal_grants_no_system_access() {
    // Even a (defensively) constructed principal whose team list literally contains "system" or
    // "global" gets no system-namespace visibility from the default authority: team membership is
    // matched as `Namespace::Team(name)`, never as the reserved `Namespace::System`/`Global`.
    for reserved in ["system", "global"] {
        let principal = Principal::new(agent_id(b"spoofer"), vec![reserved.to_string()]);
        let visible = DefaultAuthorizer.visible_namespaces(&principal);
        assert!(
            !visible.contains(&Namespace::System),
            "team:{reserved:?} must not confer system visibility"
        );
        // It also does not unlock a direct write to the reserved namespaces.
        for target in [Namespace::Global, Namespace::System] {
            assert!(
                DefaultAuthorizer
                    .authorize_write(&principal, &target)
                    .is_err(),
                "team:{reserved:?} must not unlock a write to {target}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Blocking test (c), domain half: there is NO wire route to operator=true, so a
// request that fails to carry a validated operator capability can never forge
// one by supplying a body. Full handler-level rejection is PR4 (see the module
// docs); PR3 pins the substrate invariant that the rejection guarantee rests on.
// ---------------------------------------------------------------------------

#[test]
fn there_is_no_wire_route_to_an_operator_principal() {
    // No combination of JSON fields produces operator=true. (This is the substrate guarantee a
    // PR4 handler relies on: even a fully body-controlled principal is never an operator, so a
    // missing/forged credential cannot escalate.)
    for body in [
        r#"{"agent_id":"00000000-0000-7000-8000-000000000000","teams":["system"],"operator":true}"#,
        r#"{"agent_id":"00000000-0000-7000-8000-000000000000","teams":["global","admin"],"operator":true}"#,
    ] {
        let principal: Principal = serde_json::from_str(body).expect("deserialize");
        assert!(
            !principal.operator,
            "no request body may mint an operator principal"
        );
    }
}
