//! The validated-principal request extension (PR4 of the OAuth workstream) and its lookup.
//!
//! The HTTP auth producer maps a verified token to a [`Principal`] (via
//! [`map_verified_claims_to_principal`](crate::map_verified_claims_to_principal)) and inserts the
//! resolved identity into request extensions as a [`ValidatedPrincipal`]. This module defines that
//! extension type and the helper that reads it back out of an rmcp request context. When HTTP auth
//! is disabled no producer inserts the extension, so the lookup returns `None` and the identity
//! resolvers use the legacy explicit body fields.
//!
//! # Why a newtype, and why it carries the posture
//!
//! A [`Principal`] alone is not enough for the write path. The mapper returns a
//! [`WritePosture`] *alongside* the principal (a read-only/ephemeral issuer may mint an unstable
//! content-hash id that must never write durable memory — see [`WritePosture`]), and the
//! [`Principal`] deliberately carries no read-only marker. So the extension bundles both: the
//! resolved [`Principal`] (operator bit included) and the [`WritePosture`], plus the
//! [`TokenClass`] for audit provenance. This is the **only** value PR4 inserts into / reads from
//! the request extensions.

use rmcp::model::Extensions;

use aionforge_engine::Principal;

use crate::{TokenClass, WritePosture};

/// The validated principal extracted from a request extension.
///
/// Carries the full resolved [`Principal`] (including the server-only operator bit) and the
/// [`WritePosture`], so the write path can fail closed on a [`ReadOnly`](WritePosture::ReadOnly)
/// identity whose content-hash id may be unstable. [`TokenClass`] is included for audit
/// provenance. This is the **only** type PR4 inserts into / reads from the request extensions;
/// it travels alongside the [`Principal`] so handlers never re-derive the posture.
#[derive(Debug, Clone)]
pub struct ValidatedPrincipal {
    /// The in-process principal (agent id, teams, operator bit) resolved from a validated token.
    pub principal: Principal,
    /// Whether the issuer permits durable writes ([`Writer`](WritePosture::Writer)) or is
    /// read-only/ephemeral ([`ReadOnly`](WritePosture::ReadOnly)). The write path consults this.
    pub write_posture: WritePosture,
    /// M2M-vs-SPA token classification ([`Machine`](TokenClass::Machine) /
    /// [`Spa`](TokenClass::Spa)) recorded for audit provenance.
    pub token_class: TokenClass,
}

impl ValidatedPrincipal {
    /// Bundle a resolved [`Principal`] with its [`WritePosture`] and [`TokenClass`].
    ///
    /// PR5's validator layer calls this with the triple returned by
    /// [`map_verified_claims_to_principal`](crate::map_verified_claims_to_principal), then inserts
    /// the result into the request extensions.
    #[must_use]
    pub fn new(principal: Principal, write_posture: WritePosture, token_class: TokenClass) -> Self {
        Self {
            principal,
            write_posture,
            token_class,
        }
    }
}

/// Extract the [`ValidatedPrincipal`] a validator layer inserted into a request's extensions.
///
/// # The extension is nested two levels deep (rmcp 1.6 shape)
///
/// rmcp's [`Extensions`] is a **bespoke, vendored type-map** ([`rmcp::model::Extensions`]) — it is
/// *not* `http::Extensions` re-exported. A PR5 HTTP producer can only write into the HTTP
/// request's `http::request::Parts.extensions` (an [`http::Extensions`]) before forwarding to
/// rmcp. The rmcp streamable-http transport then carries that whole `Parts` value into the rmcp
/// [`Extensions`] bag as a *single* `http::request::Parts` entry (rmcp 1.6.0
/// `transport/streamable_http_server/tower.rs`), rather than merging the two maps. So a
/// [`ValidatedPrincipal`] a producer inserts lives one level below the rmcp bag, exactly mirrored
/// by rmcp's own documented two-level read pattern
/// (`parts = ctx.extensions.get::<http::request::Parts>()`, then
/// `parts.extensions.get::<State>()`).
///
/// This helper therefore reads two levels: first the [`http::request::Parts`] out of the rmcp
/// [`Extensions`], then the [`ValidatedPrincipal`] out of `parts.extensions`. It returns `None` if
/// *either* level is absent — which is **always** the case in PR4 (dark mode, no producer wired),
/// so today every call site falls through to the auth-disabled legacy path. A handler receiving
/// `RequestContext<RoleServer>` passes `&context.extensions` here. PR5 inserts the extension via the
/// same two-level nesting and lights this up.
#[must_use]
pub fn validated_principal_from_extensions(extensions: &Extensions) -> Option<ValidatedPrincipal> {
    let parts = extensions.get::<http::request::Parts>()?;
    parts.extensions.get::<ValidatedPrincipal>().cloned()
}

#[cfg(test)]
mod tests {
    use aionforge_domain::ids::Id;
    use rmcp::model::Extensions;

    use super::{ValidatedPrincipal, validated_principal_from_extensions};
    use crate::{TokenClass, WritePosture};
    use aionforge_engine::Principal;

    /// Build the rmcp [`Extensions`] bag exactly as the streamable-http transport does once a PR5
    /// HTTP producer has run: the producer inserts a [`ValidatedPrincipal`] into the HTTP request's
    /// `http::request::Parts.extensions`, then the transport carries that whole `Parts` value into
    /// the rmcp bag as a single entry (rmcp 1.6.0). This is the *only* faithful way to exercise the
    /// two-level lookup — a bare single-level insert would pass even a broken helper.
    fn rmcp_extensions_with_validated_principal(validated: ValidatedPrincipal) -> Extensions {
        // The HTTP request `Parts` the producer mutates (`http::Extensions`, level 1).
        let (mut parts, ()) = http::Request::builder()
            .body(())
            .expect("a trivial request builds")
            .into_parts();
        parts.extensions.insert(validated);

        // The rmcp `model::Extensions` bag the transport builds, carrying the whole `Parts` (level
        // 0) — distinct from the `http::Extensions` above, which is exactly why the helper hops.
        let mut extensions = Extensions::new();
        extensions.insert(parts);
        extensions
    }

    #[test]
    fn an_empty_extension_bag_yields_no_validated_principal() {
        // PR4 dark-mode reality: no producer, so the lookup is always None at runtime.
        let extensions = Extensions::new();
        assert!(validated_principal_from_extensions(&extensions).is_none());
    }

    #[test]
    fn a_parts_value_without_a_validated_principal_yields_none() {
        // The outer Parts hop resolves, but the inner http::Extensions has no ValidatedPrincipal:
        // the helper must still return None (it never confuses "Parts present" with "principal
        // present"). This is the auth-on/no-validator-state shape.
        let (parts, ()) = http::Request::builder()
            .body(())
            .expect("a trivial request builds")
            .into_parts();
        let mut extensions = Extensions::new();
        extensions.insert(parts);
        assert!(validated_principal_from_extensions(&extensions).is_none());
    }

    #[test]
    fn the_lookup_reads_back_the_principal_with_operator_bit_and_posture_intact() {
        // Mirror PR5's insert side through the REAL two-level nesting: an operator, read-only
        // principal classified SPA, inserted into Parts.extensions, then Parts into the rmcp bag.
        let agent = Id::generate();
        let inserted = ValidatedPrincipal::new(
            Principal::with_operator(agent, vec!["platform".to_string()]),
            WritePosture::ReadOnly,
            TokenClass::Spa,
        );
        let extensions = rmcp_extensions_with_validated_principal(inserted);

        let read_back = validated_principal_from_extensions(&extensions)
            .expect("the inserted ValidatedPrincipal is read back through the two-level nesting");
        assert_eq!(read_back.principal.agent_id, agent);
        assert!(
            read_back.principal.operator,
            "the operator bit survives the extension round-trip"
        );
        assert_eq!(read_back.principal.teams, vec!["platform".to_string()]);
        assert_eq!(
            read_back.write_posture,
            WritePosture::ReadOnly,
            "the write posture survives so the write path can fail closed"
        );
        assert_eq!(read_back.token_class, TokenClass::Spa);
    }

    #[test]
    fn a_writer_machine_principal_round_trips_with_its_posture() {
        let agent = Id::generate();
        let inserted = ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::Writer,
            TokenClass::Machine,
        );
        let extensions = rmcp_extensions_with_validated_principal(inserted);

        let read_back =
            validated_principal_from_extensions(&extensions).expect("validated principal present");
        assert!(!read_back.principal.operator);
        assert_eq!(read_back.write_posture, WritePosture::Writer);
        assert_eq!(read_back.token_class, TokenClass::Machine);
    }

    #[test]
    fn a_bare_single_level_insert_is_not_found_so_the_hop_is_load_bearing() {
        // Regression guard for the original blocker: inserting a bare ValidatedPrincipal directly
        // into the rmcp bag (the WRONG shape a Tower layer can never produce) must NOT be read back
        // — proving the helper truly hops through Parts and is not silently single-level.
        let agent = Id::generate();
        let mut extensions = Extensions::new();
        extensions.insert(ValidatedPrincipal::new(
            Principal::agent(agent),
            WritePosture::Writer,
            TokenClass::Machine,
        ));
        assert!(
            validated_principal_from_extensions(&extensions).is_none(),
            "a single-level insert is the wrong shape and must not resolve"
        );
    }
}
