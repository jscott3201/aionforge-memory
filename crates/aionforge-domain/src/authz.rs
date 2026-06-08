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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// The acting agent's id (the `<id>` of its `agent:<id>` private namespace).
    pub agent_id: Id,
    /// The team ids this agent is a member of (no empty entries; see [`Principal::new`]).
    #[serde(deserialize_with = "deserialize_non_empty_teams")]
    pub teams: Vec<String>,
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

impl Principal {
    /// A principal for an agent with an explicit team membership list. Empty team ids are dropped:
    /// a real team namespace never has an empty id, so an empty entry could only match a
    /// degenerate `Namespace::Team("")` — never a legitimate team.
    #[must_use]
    pub fn new(agent_id: Id, teams: Vec<String>) -> Self {
        let teams = teams.into_iter().filter(|team| !team.is_empty()).collect();
        Self { agent_id, teams }
    }

    /// A principal for an agent that belongs to no team (the common single-agent case).
    #[must_use]
    pub fn agent(agent_id: Id) -> Self {
        Self {
            agent_id,
            teams: Vec::new(),
        }
    }

    /// The agent's own private namespace (`agent:<id>`).
    #[must_use]
    pub fn private(&self) -> Namespace {
        Namespace::Agent(self.agent_id.as_str().to_string())
    }

    /// Whether the agent is a member of `team`.
    #[must_use]
    pub fn is_member_of(&self, team: &str) -> bool {
        self.teams.iter().any(|t| t == team)
    }
}

/// The set of namespaces a [`Principal`] may read, computed once per query so each candidate is an
/// O(1) check (06 §1). `global` is always readable and `system` never; the principal's own private
/// namespace and its team namespaces complete the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleSet {
    private: Namespace,
    teams: Vec<Namespace>,
}

impl VisibleSet {
    /// Build the visible set from the principal's private namespace and its team namespaces.
    #[must_use]
    pub fn new(private: Namespace, teams: Vec<Namespace>) -> Self {
        Self { private, teams }
    }

    /// Whether `candidate` is readable by this principal. `global` is always visible; `system` is
    /// never agent-visible (substrate-internal); otherwise the candidate must be the principal's
    /// own private namespace or one of its team namespaces.
    #[must_use]
    pub fn contains(&self, candidate: &Namespace) -> bool {
        match candidate {
            Namespace::Global => true,
            Namespace::System => false,
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
pub trait Authorizer: Send + Sync {
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
            Namespace::Agent(id) if id.as_str() == principal.agent_id.as_str() => return Ok(()),
            Namespace::Team(team) if principal.is_member_of(team) => return Ok(()),
            Namespace::Agent(_) => DenyReason::NotOwnPrivate,
            Namespace::Team(_) => DenyReason::NotTeamMember,
            Namespace::Global | Namespace::System => DenyReason::NotDirectlyWritable,
        };
        Err(AuthorizationError {
            agent: principal.agent_id.as_str().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Agent ids are ULIDs, so derive them from content hashes and build namespaces from the id
    // string (an agent's private namespace is `agent:<that id>`).
    fn agent_id(seed: &[u8]) -> Id {
        Id::from_content_hash(seed)
    }

    fn private_of(seed: &[u8]) -> Namespace {
        Namespace::Agent(agent_id(seed).as_str().to_string())
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
            agent_id(b"alice").as_str()
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
}
