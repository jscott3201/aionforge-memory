//! Namespace authorization: the principal, the authority seam, and the default policy (06 §1).
//!
//! Authorization is enforced at the façade on **every read and write** (06 §1). This module owns
//! the model: who is acting ([`Principal`]), the authority that rules on it ([`Authorizer`]), the
//! set of namespaces a principal may read ([`VisibleSet`]), and the deterministic default policy
//! ([`DefaultAuthorizer`]). The capture path consults it to confine writes; the retrieval path
//! consults it to scope reads.
//!
//! **Trust boundary.** Aionforge is an in-process multi-agent substrate (no cross-host federation
//! in v1.0): the *host* authenticates an agent and asserts which teams it belongs to, and the
//! substrate — a library in the host's process — trusts that assertion. The [`Principal`] is
//! therefore caller-asserted and supplied per request; team membership is a control-plane concern
//! the host already knows, not data the substrate re-derives from the graph (which would add a
//! lookup and a time-of-check/time-of-use window for no security gain). M4.T03 provenance signing
//! later binds a write to a key, catching an agent that misreports its own id.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::Id;
use crate::namespace::Namespace;

/// A caller-asserted security principal: the acting agent and the teams it belongs to (06 §1).
///
/// Supplied by the host per request (see the module trust-boundary note). `teams` are the team
/// ids the agent is a member of; empty ids are dropped at construction — a valid namespace never
/// has an empty id, so an empty entry could otherwise match a programmatically-built
/// `Namespace::Team("")` and is meaningless besides.
///
/// # The operator capability is server-set-only and never wire-trusted
///
/// `operator` is a coarse-grained, in-process system-level capability granted *only* by the
/// resource-server claims mapper to a principal minted from a validated token that carries the
/// configured operator permission (see [`Principal::with_operator`]). It is **deserialize-forced
/// to `false`**: the `#[serde(deserialize_with = "deserialize_operator_false")]` hook below
/// ignores whatever a JSON body supplies, so an untrusted request body can never set it, and a
/// `serialize` → `deserialize` round-trip of an operator principal always comes back
/// `operator == false`. Operator is a live capability, never a persisted or forgeable value — it
/// must be re-minted from a fresh validated token on every process, never read back from the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// The acting agent's id (the `<id>` of its `agent:<id>` private namespace).
    pub agent_id: Id,
    /// The team ids this agent is a member of (no empty entries; see [`Principal::new`]).
    #[serde(deserialize_with = "deserialize_non_empty_teams")]
    pub teams: Vec<String>,
    /// Whether this principal holds the in-process operator capability (system-level console
    /// visibility). **Server-set-only and deserialize-forced to `false`** — see the type docs:
    /// no JSON body can set it, and it never survives a wire round-trip. Set to `true` only by
    /// [`Principal::with_operator`], minted by the claims mapper from a validated operator token.
    #[serde(default, deserialize_with = "deserialize_operator_false")]
    pub operator: bool,
}

/// Filter empty team ids on the deserialization path too, so the no-empty-team invariant holds for
/// every construction route, not just [`Principal::new`].
fn deserialize_non_empty_teams<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let teams = Vec::<String>::deserialize(deserializer)?;
    Ok(teams.into_iter().filter(|team| !team.is_empty()).collect())
}

/// Force the `operator` bit to `false` on every deserialization path, regardless of the JSON body.
///
/// This is the security hinge of the operator capability: it is a server-set-only, in-process
/// capability, so it must never be derivable from untrusted wire input. The supplied value is
/// read (so the field is accepted, not rejected) and then **discarded** — deserializing
/// `{"operator":true}` and `{"operator":false}` both yield `false`, and a serialize→deserialize
/// round-trip of an operator principal returns a non-operator principal. The only route to
/// `operator == true` is the server-side [`Principal::with_operator`] constructor.
fn deserialize_operator_false<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Consume whatever is present — a bool, or any other JSON shape — and discard it. Using
    // `IgnoredAny` (rather than `bool`) makes the "ignore whatever the body supplies" contract
    // literal: `{"operator":true}`, `{"operator":"true"}`, `{"operator":1}`, `{"operator":null}`,
    // and an object/array are all accepted and all yield `false`, so a malformed `operator` field
    // can neither forge the bit nor reject the whole principal.
    let _ignored = serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(false)
}

impl Principal {
    /// A principal for an agent with an explicit team membership list. Empty team ids are dropped:
    /// a real team namespace never has an empty id, so an empty entry could only match a
    /// degenerate `Namespace::Team("")` — never a legitimate team. The operator bit is `false`:
    /// only [`Principal::with_operator`] mints an operator principal.
    #[must_use]
    pub fn new(agent_id: Id, teams: Vec<String>) -> Self {
        let teams = teams.into_iter().filter(|team| !team.is_empty()).collect();
        Self {
            agent_id,
            teams,
            operator: false,
        }
    }

    /// A principal for an agent that belongs to no team (the common single-agent case). The
    /// operator bit is `false`.
    #[must_use]
    pub fn agent(agent_id: Id) -> Self {
        Self {
            agent_id,
            teams: Vec::new(),
            operator: false,
        }
    }

    /// **Server-only.** Mint an operator principal — a principal that additionally holds the
    /// in-process operator capability (system-level console visibility). Empty team ids are
    /// dropped exactly as in [`Principal::new`].
    ///
    /// This is the *sole* route to `operator == true`. It exists for the resource-server claims
    /// mapper, which calls it only after a token has been cryptographically validated and proven
    /// to carry the deployment's configured operator permission. The host-asserted MCP principal
    /// path never calls it, so a host-asserted principal is always `operator == false`. The bit
    /// is deserialize-forced to `false` (see the type docs), so an operator principal minted here
    /// degrades to a non-operator principal the moment it crosses the wire.
    #[must_use]
    pub fn with_operator(agent_id: Id, teams: Vec<String>) -> Self {
        let teams = teams.into_iter().filter(|team| !team.is_empty()).collect();
        Self {
            agent_id,
            teams,
            operator: true,
        }
    }

    /// The agent's own private namespace (`agent:<id>`).
    #[must_use]
    pub fn private(&self) -> Namespace {
        Namespace::Agent(self.agent_id.to_string())
    }

    /// Whether the agent is a member of `team`.
    #[must_use]
    pub fn is_member_of(&self, team: &str) -> bool {
        self.teams.iter().any(|t| t == team)
    }
}

/// The set of namespaces a [`Principal`] may read, computed once per query so each candidate is an
/// O(1) check (06 §1). `global` is always readable and `system` never (unless an admin reveal
/// grants it, 07 §4); the principal's own private namespace and its team namespaces complete the
/// set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleSet {
    private: Namespace,
    teams: Vec<Namespace>,
    system: bool,
}

impl VisibleSet {
    /// Build the visible set from the principal's private namespace and its team namespaces.
    /// The system namespace is excluded — use [`VisibleSet::with_system`] for an admin reveal.
    #[must_use]
    pub fn new(private: Namespace, teams: Vec<Namespace>) -> Self {
        Self {
            private,
            teams,
            system: false,
        }
    }

    /// The same set, additionally admitting the `system` namespace — the namespace half of the
    /// admin reveal (07 §4, M6.T02). Only an authority that grants `may_surface_system` should
    /// build this; the default authority never does.
    #[must_use]
    pub fn with_system(mut self) -> Self {
        self.system = true;
        self
    }

    /// The concrete namespaces this set admits, as an explicit list — the enumerable
    /// inverse of [`VisibleSet::contains`]. A recall uses it to SCOPE candidate generation
    /// to the visible namespaces (so the per-signal fan-out is spent on memories the reader
    /// may actually see) instead of scanning every namespace and authorizing only
    /// afterward. Always includes `global` and the principal's own private namespace, plus
    /// its team namespaces, and `system` when this set was built for an admin reveal
    /// ([`VisibleSet::with_system`]). The order is stable (global, private, teams, system),
    /// and the membership is exactly what `contains` returns `true` for, so scoping to this
    /// list never drops a candidate the post-hoc filter would have admitted.
    #[must_use]
    pub fn namespaces(&self) -> Vec<Namespace> {
        let mut out = Vec::with_capacity(2 + self.teams.len() + usize::from(self.system));
        out.push(Namespace::Global);
        out.push(self.private.clone());
        out.extend(self.teams.iter().cloned());
        if self.system {
            out.push(Namespace::System);
        }
        out
    }

    /// Whether `candidate` is readable by this principal. `global` is always visible; `system` is
    /// never agent-visible (substrate-internal) unless this set was built for an admin reveal;
    /// otherwise the candidate must be the principal's own private namespace or one of its team
    /// namespaces.
    #[must_use]
    pub fn contains(&self, candidate: &Namespace) -> bool {
        match candidate {
            Namespace::Global => true,
            Namespace::System => self.system,
            other => *other == self.private || self.teams.iter().any(|team| team == other),
        }
    }
}

/// Why a write was denied — the structured reason recorded in the rejection audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The target is another agent's private namespace.
    NotOwnPrivate,
    /// The target is a team the principal is not a member of.
    NotTeamMember,
    /// The target (`global`/`system`) is never directly writable; it is reached only via promotion.
    NotDirectlyWritable,
}

impl DenyReason {
    /// A stable, human-readable reason string for the audit payload and the error message.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            DenyReason::NotOwnPrivate => "not the agent's own private namespace",
            DenyReason::NotTeamMember => "not a member of the team",
            DenyReason::NotDirectlyWritable => {
                "namespace is not directly writable (reach it via promotion)"
            }
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A write the authority refused: which agent, which target namespace, and why (06 §1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("agent {agent} may not write to namespace {target}: {reason}")]
pub struct AuthorizationError {
    /// The agent that attempted the write.
    pub agent: String,
    /// The canonical form of the namespace it tried to write.
    pub target: String,
    /// Why the write was refused.
    pub reason: DenyReason,
}

/// The namespace-authorization authority, consulted on every read and write (06 §1).
///
/// Synchronous and local, mirroring [`PrivacyFilter`](crate::contracts::PrivacyFilter): a write is
/// authorized or refused with a typed error; a read is scoped by a precomputed [`VisibleSet`]. The
/// production default is [`DefaultAuthorizer`]; later milestones can compose a stricter authority
/// (e.g. signature-gated writes in M4.T03) behind the same seam without touching the call sites.
pub trait Authorizer: Send + Sync + std::fmt::Debug {
    /// Authorize a write to `target` by `principal`. `Err` denies it; the façade rejects the write
    /// and records a `namespace_denied` audit.
    ///
    /// # Errors
    /// Returns [`AuthorizationError`] when the principal is not permitted to write `target`.
    fn authorize_write(
        &self,
        principal: &Principal,
        target: &Namespace,
    ) -> Result<(), AuthorizationError>;

    /// The set of namespaces `principal` may read, computed once per query.
    fn visible_namespaces(&self, principal: &Principal) -> VisibleSet;

    /// Whether `principal` holds the admin capability to surface system-role memories,
    /// which are excluded from default recall (07 §4, M6.T02). The default is **`false`**,
    /// so every authority is closed unless it deliberately overrides this — a recall only
    /// surfaces system content when the caller both requests it (`RecallOptions::
    /// include_system`) AND this grants it. The capability lives here, on the
    /// embedder-injected authority, rather than on the host-asserted [`Principal`] or a
    /// bare request flag, so an untrusted caller cannot forge it.
    fn may_surface_system(&self, principal: &Principal) -> bool {
        let _ = principal;
        false
    }
}

/// A shared authority is itself an authority, so one instance backs both the capture and retrieval
/// paths without being cloned (mirrors the [`Embedder`](crate::contracts::Embedder) forwarding).
impl<A: Authorizer + ?Sized> Authorizer for std::sync::Arc<A> {
    fn authorize_write(
        &self,
        principal: &Principal,
        target: &Namespace,
    ) -> Result<(), AuthorizationError> {
        (**self).authorize_write(principal, target)
    }

    fn visible_namespaces(&self, principal: &Principal) -> VisibleSet {
        (**self).visible_namespaces(principal)
    }

    fn may_surface_system(&self, principal: &Principal) -> bool {
        // Forward explicitly: a stricter authority injected behind Arc<dyn Authorizer>
        // (the retriever's actual shape) must be consulted, not silently shadowed by the
        // trait default.
        (**self).may_surface_system(principal)
    }
}

/// The deterministic default namespace policy (06 §1).
///
/// - **Write:** an agent may write only its own private namespace (`agent:<self>`) and the team
///   namespaces it is a member of; `global` and `system` are never directly writable (the only
///   path across the boundary is promotion, 06 §4). The capture path additionally forces every
///   *untrusted* write to the private namespace before this check runs, so untrusted content can
///   never reach a team even if the request asked for one.
/// - **Read:** `global` plus the principal's own private namespace and its team namespaces;
///   `system` is never agent-visible.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultAuthorizer;

impl Authorizer for DefaultAuthorizer {
    fn authorize_write(
        &self,
        principal: &Principal,
        target: &Namespace,
    ) -> Result<(), AuthorizationError> {
        let reason = match target {
            Namespace::Agent(id) if *id == principal.agent_id.to_string() => return Ok(()),
            Namespace::Team(team) if principal.is_member_of(team) => return Ok(()),
            Namespace::Agent(_) => DenyReason::NotOwnPrivate,
            Namespace::Team(_) => DenyReason::NotTeamMember,
            Namespace::Global | Namespace::System => DenyReason::NotDirectlyWritable,
        };
        Err(AuthorizationError {
            agent: principal.agent_id.to_string(),
            target: target.to_string(),
            reason,
        })
    }

    fn visible_namespaces(&self, principal: &Principal) -> VisibleSet {
        let teams = principal
            .teams
            .iter()
            .map(|team| Namespace::Team(team.clone()))
            .collect();
        VisibleSet::new(principal.private(), teams)
    }
}

// The operator-aware authority that adds the read-side operator capability lives in its own
// module (file-split to keep this file under the 700-LOC cap) and is re-exported below so the
// public path `aionforge_domain::authz::OperatorAwareAuthorizer` is unchanged.
mod operator;

pub use operator::OperatorAwareAuthorizer;

#[cfg(test)]
mod tests {
    use super::*;

    // Agent ids are UUIDs, so derive them from content hashes and build namespaces from the id
    // string (an agent's private namespace is `agent:<that id>`).
    fn agent_id(seed: &[u8]) -> Id {
        Id::from_content_hash(seed)
    }

    fn private_of(seed: &[u8]) -> Namespace {
        Namespace::Agent(agent_id(seed).to_string())
    }

    fn alice() -> Principal {
        Principal::new(agent_id(b"alice"), vec!["squad".to_string()])
    }

    #[test]
    fn an_agent_may_write_its_own_private_namespace() {
        let alice = alice();
        assert!(
            DefaultAuthorizer
                .authorize_write(&alice, &alice.private())
                .is_ok()
        );
    }

    #[test]
    fn an_agent_may_not_write_another_agents_private_namespace() {
        let err = DefaultAuthorizer
            .authorize_write(&alice(), &private_of(b"bob"))
            .expect_err("denied");
        assert_eq!(err.reason, DenyReason::NotOwnPrivate);
    }

    #[test]
    fn an_agent_may_write_a_team_it_belongs_to_but_not_others() {
        let authz = DefaultAuthorizer;
        assert!(
            authz
                .authorize_write(&alice(), &Namespace::Team("squad".to_string()))
                .is_ok(),
            "member team is writable"
        );
        let err = authz
            .authorize_write(&alice(), &Namespace::Team("other".to_string()))
            .expect_err("non-member team denied");
        assert_eq!(err.reason, DenyReason::NotTeamMember);
    }

    #[test]
    fn global_and_system_are_never_directly_writable() {
        let authz = DefaultAuthorizer;
        for target in [Namespace::Global, Namespace::System] {
            let err = authz
                .authorize_write(&alice(), &target)
                .expect_err("not directly writable");
            assert_eq!(err.reason, DenyReason::NotDirectlyWritable);
        }
    }

    #[test]
    fn the_visible_set_covers_global_self_and_member_teams_only() {
        let alice = alice();
        let visible = DefaultAuthorizer.visible_namespaces(&alice);
        assert!(
            visible.contains(&Namespace::Global),
            "global always visible"
        );
        assert!(visible.contains(&alice.private()), "own private");
        assert!(
            visible.contains(&Namespace::Team("squad".to_string())),
            "member team"
        );
        assert!(
            !visible.contains(&Namespace::Team("other".to_string())),
            "non-member team hidden"
        );
        assert!(
            !visible.contains(&private_of(b"bob")),
            "other private hidden"
        );
        assert!(
            !visible.contains(&Namespace::System),
            "system never agent-visible"
        );
    }

    #[test]
    fn namespaces_lists_exactly_what_contains_admits() {
        let alice = alice();
        let visible = DefaultAuthorizer.visible_namespaces(&alice);
        let listed = visible.namespaces();
        // The list covers global, own private, and member teams — and nothing else.
        assert!(listed.contains(&Namespace::Global), "global listed");
        assert!(listed.contains(&alice.private()), "own private listed");
        assert!(
            listed.contains(&Namespace::Team("squad".to_string())),
            "member team listed"
        );
        assert!(
            !listed.contains(&Namespace::System),
            "system not listed without an admin reveal"
        );
        // Parity: every listed namespace is admitted by `contains`, and the list is the
        // full enumerable visible set (scoping to it cannot drop an admitted candidate).
        for ns in &listed {
            assert!(
                visible.contains(ns),
                "listed namespace {ns} must be contained"
            );
        }
        // The admin-reveal set additionally lists system.
        let revealed = visible.with_system().namespaces();
        assert!(
            revealed.contains(&Namespace::System),
            "an admin-reveal set lists the system namespace"
        );
    }

    #[test]
    fn an_agent_with_no_teams_sees_only_global_and_itself() {
        let solo = Principal::agent(agent_id(b"solo"));
        let visible = DefaultAuthorizer.visible_namespaces(&solo);
        assert!(visible.contains(&Namespace::Global));
        assert!(visible.contains(&solo.private()));
        assert!(!visible.contains(&Namespace::Team("squad".to_string())));
    }

    #[test]
    fn empty_team_ids_are_dropped_on_every_construction_path() {
        // Direct construction filters empties.
        let p = Principal::new(agent_id(b"alice"), vec![String::new(), "squad".to_string()]);
        assert_eq!(p.teams, vec!["squad".to_string()]);
        assert!(!p.is_member_of(""), "an empty team is never a member");
        assert_eq!(
            DefaultAuthorizer
                .authorize_write(&p, &Namespace::Team(String::new()))
                .expect_err("denied")
                .reason,
            DenyReason::NotTeamMember,
            "a degenerate team:\"\" write is refused"
        );
        assert!(
            !DefaultAuthorizer
                .visible_namespaces(&p)
                .contains(&Namespace::Team(String::new())),
            "team:\"\" is never visible"
        );

        // The deserialization path filters too (it does not route through `new`).
        let json = format!(
            r#"{{"agent_id":"{}","teams":["","squad"]}}"#,
            agent_id(b"alice")
        );
        let de: Principal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(de.teams, vec!["squad".to_string()]);
    }

    #[test]
    fn the_arc_forwarding_delegates() {
        let authz: std::sync::Arc<dyn Authorizer> = std::sync::Arc::new(DefaultAuthorizer);
        let alice = alice();
        assert!(authz.authorize_write(&alice, &alice.private()).is_ok());
        assert!(
            authz
                .visible_namespaces(&alice)
                .contains(&Namespace::Global)
        );
    }

    #[test]
    fn the_operator_bit_is_false_on_every_normal_construction_path() {
        // The two non-operator constructors never set the bit.
        assert!(!Principal::new(agent_id(b"alice"), vec!["squad".into()]).operator);
        assert!(!Principal::agent(agent_id(b"solo")).operator);
        // Only `with_operator` mints it.
        assert!(Principal::with_operator(agent_id(b"op"), Vec::new()).operator);
    }

    #[test]
    fn the_operator_bit_cannot_be_set_from_a_json_body() {
        // An untrusted body asserting `"operator": true` is forced to `false` by the
        // deserialize hook: the bit is a server-set-only in-process capability.
        let json_true = format!(
            r#"{{"agent_id":"{}","teams":["squad"],"operator":true}}"#,
            agent_id(b"alice")
        );
        let from_true: Principal = serde_json::from_str(&json_true).expect("deserialize");
        assert!(
            !from_true.operator,
            "a body-asserted operator:true must deserialize to operator=false"
        );

        // An explicit `false` and an omitted field also yield `false`.
        let json_false = format!(
            r#"{{"agent_id":"{}","teams":["squad"],"operator":false}}"#,
            agent_id(b"alice")
        );
        assert!(
            !serde_json::from_str::<Principal>(&json_false)
                .expect("deserialize")
                .operator
        );
        let json_absent = format!(
            r#"{{"agent_id":"{}","teams":["squad"]}}"#,
            agent_id(b"alice")
        );
        assert!(
            !serde_json::from_str::<Principal>(&json_absent)
                .expect("deserialize")
                .operator
        );
    }

    #[test]
    fn an_operator_principal_never_survives_a_wire_round_trip() {
        // An operator is a live capability, never a persisted/forgeable value: serialize an
        // operator principal and it comes back a non-operator principal.
        let op = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);
        assert!(op.operator, "minted operator");
        let json = serde_json::to_string(&op).expect("serialize");
        let back: Principal = serde_json::from_str(&json).expect("deserialize");
        assert!(
            !back.operator,
            "an operator principal must round-trip back to operator=false"
        );
        // Everything else round-trips unchanged.
        assert_eq!(back.agent_id, op.agent_id);
        assert_eq!(back.teams, op.teams);
    }

    #[test]
    fn the_operator_authorizer_grants_the_capability_only_to_operators() {
        let authz = OperatorAwareAuthorizer::new(DefaultAuthorizer);
        let regular = alice();
        let operator = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);

        // A regular principal: the inner authority is closed, so no system reveal capability.
        assert!(!authz.may_surface_system(&regular));
        // An operator: the capability is granted.
        assert!(authz.may_surface_system(&operator));
    }

    #[test]
    fn the_operator_authorizer_never_pre_widens_the_visible_set() {
        // The authority must NOT widen `visible_namespaces` with the system namespace — not even
        // for an operator. Widening here is unconditional (no `include_system` in scope) and would
        // defeat the AND gate at the read sites. The widening is the call site's job, in lockstep
        // with `include_system`.
        let authz = OperatorAwareAuthorizer::new(DefaultAuthorizer);
        let operator = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);

        let visible = authz.visible_namespaces(&operator);
        assert!(
            !visible.contains(&Namespace::System),
            "the authority must NOT pre-widen an operator's set with the system namespace"
        );
        // The ordinary namespaces are present (delegation is verbatim).
        assert!(visible.contains(&Namespace::Global));
        assert!(visible.contains(&operator.private()));
        assert!(visible.contains(&Namespace::Team("squad".to_string())));
        // A non-operator's set is identical — verbatim delegation either way.
        let regular = alice();
        let regular_visible = authz.visible_namespaces(&regular);
        assert!(!regular_visible.contains(&Namespace::System));
        assert_eq!(
            regular_visible,
            DefaultAuthorizer.visible_namespaces(&regular),
            "a non-operator's visible set is the inner set, unchanged"
        );
    }

    /// Reproduce the read-site composition every call site uses:
    /// `let mut visible = authorizer.visible_namespaces(p);`
    /// `let surface_system = include_system && authorizer.may_surface_system(p);`
    /// `if surface_system { visible = visible.with_system(); }`
    /// and return the resulting set so a test can assert exactly what a recall would gate on.
    fn composed_visible_at_an_and_site(
        authz: &impl Authorizer,
        principal: &Principal,
        include_system: bool,
    ) -> VisibleSet {
        let mut visible = authz.visible_namespaces(principal);
        let surface_system = include_system && authz.may_surface_system(principal);
        if surface_system {
            visible = visible.with_system();
        }
        visible
    }

    #[test]
    fn an_operator_surfaces_system_at_an_and_site_only_when_include_system_is_set() {
        // This is the test that would have caught the over-widening blocker: it composes the
        // operator authorizer exactly as the AND-gated read sites do, for both values of
        // `include_system`, and asserts the system namespace gate lifts in lockstep — not before.
        let authz = OperatorAwareAuthorizer::new(DefaultAuthorizer);
        let operator = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);

        // Ordinary recall (include_system = false): an operator does NOT see the system namespace,
        // so no system-namespace episode and no system-namespace core block can surface.
        let ordinary = composed_visible_at_an_and_site(&authz, &operator, false);
        assert!(
            !ordinary.contains(&Namespace::System),
            "an operator on an include_system=false recall must NOT surface the system namespace"
        );
        // ...but the operator still sees everything it ordinarily would.
        assert!(ordinary.contains(&Namespace::Global));
        assert!(ordinary.contains(&operator.private()));
        assert!(ordinary.contains(&Namespace::Team("squad".to_string())));

        // Opt-in recall (include_system = true): now the system namespace is admitted.
        let opted_in = composed_visible_at_an_and_site(&authz, &operator, true);
        assert!(
            opted_in.contains(&Namespace::System),
            "an operator on an include_system=true recall surfaces the system namespace"
        );

        // A non-operator never surfaces system, even asking for it.
        let regular = alice();
        assert!(
            !composed_visible_at_an_and_site(&authz, &regular, true).contains(&Namespace::System),
            "a non-operator never surfaces the system namespace, opt-in or not"
        );
    }

    #[test]
    fn the_operator_authorizer_never_widens_write_authority() {
        // The operator capability is read-side only: an operator still writes only its own
        // private namespace and member teams, exactly like a non-operator.
        let authz = OperatorAwareAuthorizer::new(DefaultAuthorizer);
        let operator = Principal::with_operator(agent_id(b"op"), vec!["squad".into()]);
        assert!(
            authz
                .authorize_write(&operator, &operator.private())
                .is_ok(),
            "operator writes its own private namespace"
        );
        assert!(
            authz
                .authorize_write(&operator, &Namespace::Team("squad".to_string()))
                .is_ok(),
            "operator writes a member team"
        );
        for target in [Namespace::Global, Namespace::System] {
            assert_eq!(
                authz
                    .authorize_write(&operator, &target)
                    .expect_err("operator may not directly write global/system")
                    .reason,
                DenyReason::NotDirectlyWritable,
                "the operator bit does not unlock global/system writes"
            );
        }
        assert_eq!(
            authz
                .authorize_write(&operator, &private_of(b"bob"))
                .expect_err("operator may not write another agent's private namespace")
                .reason,
            DenyReason::NotOwnPrivate,
        );
    }

    #[test]
    fn the_operator_authorizer_delegates_through_an_arc() {
        // Wrapped behind the retriever's actual `Arc<dyn Authorizer>` shape, the operator
        // capability and the inner delegation both survive.
        let authz: std::sync::Arc<dyn Authorizer> =
            std::sync::Arc::new(OperatorAwareAuthorizer::new(DefaultAuthorizer));
        let operator = Principal::with_operator(agent_id(b"op"), Vec::new());
        let regular = Principal::agent(agent_id(b"solo"));
        assert!(
            authz.may_surface_system(&operator),
            "Arc forwards the grant"
        );
        assert!(
            !authz.may_surface_system(&regular),
            "Arc forwards the inner denial"
        );
        assert!(
            authz.authorize_write(&regular, &regular.private()).is_ok(),
            "Arc forwards the inner write check"
        );
    }
}
