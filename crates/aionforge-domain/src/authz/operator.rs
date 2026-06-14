//! The operator-aware authority: the read-side capability seam for the OAuth operator bit.
//!
//! [`OperatorAwareAuthorizer`] wraps any inner [`Authorizer`] and adds the *single* override the
//! operator capability needs — `may_surface_system` returns `true` for a
//! [`Principal::operator`](crate::authz::Principal) principal. It deliberately does **not** widen
//! `visible_namespaces`; the namespace gate is lifted by each read site in lockstep with
//! `include_system`. It lives in its own module (the file split keeps `authz.rs` under the
//! 700-LOC cap) but is re-exported from [`crate::authz`], so the public path is unchanged.

use crate::authz::{AuthorizationError, Authorizer, Principal, VisibleSet};
use crate::namespace::Namespace;

/// An authority that grants the operator capability on top of an inner authority (07 §4, PR3).
///
/// `OperatorAwareAuthorizer` *wraps* an inner [`Authorizer`] (the production default is
/// [`DefaultAuthorizer`](crate::authz::DefaultAuthorizer), but any authority composes) and adds
/// exactly one thing: an [`Principal::operator`](crate::authz::Principal) principal may surface
/// system-role memories and read the `system` namespace. It mirrors the retriever's authority
/// shape (`Arc<dyn Authorizer>`), so it can be dropped in behind the same seam without touching
/// any call site.
///
/// # What it overrides, and why
///
/// * [`Authorizer::may_surface_system`] — `true` for an operator, else delegates to the inner
///   authority. This is the capability half of the admin reveal (see the four-site notes below)
///   and the **only** override needed: every call site already lifts the namespace gate from this
///   capability (`if surface_system { visible.with_system() }`), so granting the capability is
///   sufficient to surface system content *when the caller opts in*.
/// * [`Authorizer::visible_namespaces`] — **delegated verbatim** to the inner authority; this
///   authority does **not** pre-widen the set with `system`. Widening here would be unconditional
///   (there is no `include_system` in scope) and would therefore defeat the AND gate: every call
///   site composes `let mut visible = visible_namespaces(p); if surface_system { visible =
///   visible.with_system(); }` where `surface_system = include_system && may_surface_system(p)`.
///   If this authority already returned a system-bearing set, an operator running an *ordinary*
///   recall (`include_system == false ⇒ surface_system == false`) would skip the `if` branch yet
///   still carry `system == true`, leaking every non-system-role episode and every core block in
///   the `system` namespace with no caller opt-in. Leaving the widening to each call site lifts the
///   namespace gate in lockstep with `include_system`, exactly as the design requires.
/// * [`Authorizer::authorize_write`] — **not touched**: delegates verbatim to the inner authority.
///   The operator capability is read-side only; it never widens write authority. An operator still
///   writes only its own private namespace and member teams, exactly like any other principal.
///
/// # Four-site behaviour (each reviewed and documented)
///
/// All four sites build their visible set as `visible_namespaces(p)` (now the *un-widened* inner
/// set for everyone, operator or not) and then conditionally widen it with `with_system()`. Because
/// this authority widens at *no* site itself, the operator's namespace gate lifts exactly where the
/// site's own `surface_system` flag lifts:
///
/// 1. `engine::Memory::audit_explicit_namespace_denials` (AND path) — system surfaces only with
///    `include_system` AND `may_surface_system`; an operator satisfies the authority half, so the
///    audit short-circuit fires only when the operator also opts in (no audit-row suppression on an
///    ordinary `include_system == false` recall).
/// 2. `read_memory` tool (AND path) — same: requires both the `include_system` flag and the
///    operator capability.
/// 3. `session_manifest` tool (**NON-AND** path) — `surface_system = may_surface_system(principal)`
///    *alone*, with no caller opt-in. An operator therefore surfaces system-role episodes in the
///    manifest **unconditionally**. This is **intended**: an operator has unconditional
///    manifest-level system visibility; it is a feature of the operator capability, not a bug. Here
///    the site's own `surface_system` is already `true` for an operator, so its `with_system()`
///    branch runs — the manifest is the one place the namespace gate lifts without a caller flag.
/// 4. `retriever::recall` (AND path) — system surfaces only with `include_system` AND
///    `may_surface_system`; an operator satisfies the authority half. The identity pre-pass
///    (`core_block_entries`) reads the *same* un-widened set unless the site widens it, so a
///    `system`-namespace core block surfaces to an operator only on an `include_system` recall.
#[derive(Debug, Clone, Copy)]
pub struct OperatorAwareAuthorizer<A> {
    inner: A,
}

impl<A: Authorizer> OperatorAwareAuthorizer<A> {
    /// Wrap an inner authority, adding the operator system-visibility capability on top.
    #[must_use]
    pub fn new(inner: A) -> Self {
        Self { inner }
    }

    /// Borrow the wrapped inner authority (e.g. to assert delegation in a test).
    #[must_use]
    pub fn inner(&self) -> &A {
        &self.inner
    }
}

impl<A: Authorizer> Authorizer for OperatorAwareAuthorizer<A> {
    fn authorize_write(
        &self,
        principal: &Principal,
        target: &Namespace,
    ) -> Result<(), AuthorizationError> {
        // The operator capability never widens write authority: delegate verbatim.
        self.inner.authorize_write(principal, target)
    }

    fn visible_namespaces(&self, principal: &Principal) -> VisibleSet {
        // Delegate verbatim — do NOT pre-widen with the system namespace, not even for an
        // operator. There is no `include_system` flag in scope here, so any widening would be
        // unconditional and would defeat the AND gate at the read sites: each site composes
        // `visible_namespaces(p)` with `if surface_system { with_system() }`, and an operator's
        // ordinary recall (`include_system == false`) skips that branch. Pre-widening here would
        // leak every `system`-namespace candidate (episodes and core blocks) on every operator
        // recall with no caller opt-in. The operator's namespace gate is lifted instead by the
        // site's own `with_system()` call, in lockstep with `include_system`, because
        // `may_surface_system` (below) already returns `true` for an operator.
        self.inner.visible_namespaces(principal)
    }

    fn may_surface_system(&self, principal: &Principal) -> bool {
        // An operator may surface system content; otherwise defer to the inner authority (which,
        // for the default, is closed).
        principal.operator || self.inner.may_surface_system(principal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::DefaultAuthorizer;
    use crate::ids::Id;

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
                crate::authz::DenyReason::NotDirectlyWritable,
                "the operator bit does not unlock global/system writes"
            );
        }
        assert_eq!(
            authz
                .authorize_write(&operator, &private_of(b"bob"))
                .expect_err("operator may not write another agent's private namespace")
                .reason,
            crate::authz::DenyReason::NotOwnPrivate,
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
