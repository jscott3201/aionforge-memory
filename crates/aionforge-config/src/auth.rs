//! OAuth resource-server authentication configuration (DEFAULT-OFF).
//!
//! Its own module because the resource-server posture is a coherent unit: the master
//! switch, the set of trusted token issuers, and per-issuer claim mapping. This is the
//! PR1 surface — **config only**. No JWT validation, JWKS discovery, or principal
//! mapping runs here; those are PR2/PR3 and read these fields. When the master switch
//! [`AuthConfig::enabled`] is `false` (the default) the server derives **no identity**
//! from a connection, exactly today's behavior.
//!
//! [`AuthConfig::validate`] is **fail-closed but scoped**: a disabled block imposes
//! nothing (returns `Ok` immediately), while an *enabled* block must name at least one
//! sound issuer with an `https`/loopback origin, a non-empty audience, and only the
//! permitted RSA algorithms. [`AuthConfig::startup_warnings`] carries soft advisories the
//! host logs at startup in PR5 — they never fail [`AuthConfig::validate`].

use std::collections::{BTreeMap, BTreeSet};

use aionforge_domain::ids::Id;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Leeway above which a configured clock-skew window is flagged as unusually large by
/// [`AuthConfig::startup_warnings`] (two minutes). Not a hard limit — large leeway is a
/// posture choice, not a configuration error.
const LARGE_LEEWAY_SECS: u64 = 120;

/// OAuth resource-server posture: the master switch and the trusted token issuers.
///
/// DEFAULT-OFF. With [`AuthConfig::enabled`] `false` the server derives no identity from
/// a connection (today's behavior); only an enabled block is validated against the
/// issuer rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// The master switch. When `false` (the default) the server derives no identity from
    /// a connection and imposes no issuer requirements. When `true` the block must name
    /// at least one sound issuer (see [`AuthConfig::validate`]).
    pub enabled: bool,
    /// The token issuers this deployment trusts, each with its own audience and claim
    /// mapping. Empty by default; required to be non-empty once auth is enabled.
    pub issuers: Vec<IssuerConfig>,
}

/// One trusted token issuer and the claim mapping a principal from it is built with.
///
/// Fields beyond the transport/algorithm gate are consumed in later PRs (JWKS discovery
/// in PR2, principal mapping and the team/operator authorization in PR3); they are stored
/// and documented here so the wire shape is stable from PR1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IssuerConfig {
    /// The token `iss` this entry trusts, compared **byte-for-byte** against the claim.
    /// Issuer URLs are not normalized: Auth0 ends in a trailing `/`, Entra v2 does not,
    /// so the exact string matters.
    pub issuer: String,
    /// The JWKS endpoint for this issuer's signing keys. `None` (the default) means PR2
    /// discovers it from the issuer's `/.well-known/openid-configuration` document.
    pub jwks_uri: Option<String>,
    /// The required `aud` (the API identifier / resource indicator, RFC 8707). A token
    /// whose audience does not match is rejected in PR2.
    pub audience: String,
    /// The signature algorithms accepted for this issuer. Defaults to `["RS256"]`; only
    /// `RS256`/`RS384`/`RS512` are permitted (case-sensitive) — `none` and the symmetric
    /// `HS*` families are refused.
    pub allowed_algs: Vec<String>,
    /// The claim name carrying the principal's team memberships. Defaults to the
    /// URI-namespaced `https://aionforge.dev/teams`, because Auth0 strips custom claims
    /// that are not URI-namespaced.
    pub teams_claim: String,
    /// The mandatory fail-closed team allow-list. A team named in a token but absent from
    /// this set is **dropped**, not granted (enforced in PR3). An empty set therefore
    /// grants no teams from this issuer.
    pub teams_allowlist: BTreeSet<String>,
    /// The Auth0 RBAC permission string that marks a console operator, if any. Consumed
    /// in PR3 to set the operator bit; `None` means this issuer mints no operators.
    pub operator_permission: Option<String>,
    /// Clock-skew leeway in seconds applied to `exp`/`nbf`/`iat` during validation.
    /// Defaults to 60.
    pub leeway_secs: u64,
    /// Fork#1: a map from a token `sub` to a fixed agent UUID, so a durable-writer keeps
    /// its namespace across an issuer/`sub` migration. Empty by default.
    pub agent_id_overrides: BTreeMap<String, Id>,
    /// Fork#1 alternative: the name of an immutable custom claim carrying a stable agent
    /// UUID, preferred over hashing `sub`. `None` by default.
    pub agent_id_claim: Option<String>,
    /// Whether principals from this issuer may write durable memory. Defaults to `true`
    /// (M2M agents write); a read-only or operator-only issuer sets `false`.
    pub allows_writes: bool,
}

impl Default for IssuerConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            jwks_uri: None,
            audience: String::new(),
            allowed_algs: vec!["RS256".into()],
            teams_claim: "https://aionforge.dev/teams".into(),
            teams_allowlist: BTreeSet::new(),
            operator_permission: None,
            leeway_secs: 60,
            agent_id_overrides: BTreeMap::new(),
            agent_id_claim: None,
            allows_writes: true,
        }
    }
}

/// The signature algorithms an issuer may accept. Only the RSA-PKCS#1 family is
/// permitted: `none` defeats verification and the symmetric `HS*` family shares the
/// signing secret with verifiers, neither of which is sound for a resource server.
const PERMITTED_ALGS: [&str; 3] = ["RS256", "RS384", "RS512"];

/// Whether `issuer` is an acceptable issuer origin: an `https` URL with a non-empty host,
/// or an `http` loopback origin (cleartext is only tolerated to localhost). The host is
/// matched by **equality** against the standard loopback literals (`localhost`,
/// `127.0.0.1`, `[::1]`), never by prefix: an attacker domain such as
/// `http://localhost.evil.example/` or `http://127.0.0.1@evil.example/` (userinfo spoof)
/// must NOT be admitted as cleartext, and a structurally-empty `https://` is rejected.
fn issuer_origin_is_allowed(issuer: &str) -> bool {
    if let Some(rest) = issuer.strip_prefix("https://") {
        // A real issuer always has a host; reject a structurally-empty `https://`. Any
        // userinfo (`user@host`) is stripped so it is the bare host that must be non-empty.
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = authority.rsplit('@').next().unwrap_or_default();
        return !host.is_empty();
    }
    if let Some(rest) = issuer.strip_prefix("http://") {
        // The authority is everything up to the first path/query/fragment delimiter.
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        // Any userinfo (`user@host`) means the real host is whatever follows the `@`,
        // so a literal in the userinfo position is a spoof, not a loopback host.
        if authority.contains('@') {
            return false;
        }
        // Compare the bare host (port stripped) for EQUALITY, keeping IPv6 brackets
        // intact so `[::1]` is matched as a whole and `[::1].evil` is not.
        let host = if let Some(after_bracket) = authority.strip_prefix('[') {
            // A bracketed host must be exactly `[..]` optionally followed by `:port`;
            // anything between the closing `]` and a `:`/end (e.g. `.evil`) is a spoof.
            match after_bracket.split_once(']') {
                Some((inner, tail)) if tail.is_empty() || tail.starts_with(':') => {
                    return inner == "::1";
                }
                _ => return false,
            }
        } else {
            authority.split(':').next().unwrap_or_default()
        };
        return matches!(host, "localhost" | "127.0.0.1");
    }
    false
}

impl AuthConfig {
    /// Validate the resource-server posture, fail-closed but scoped to an enabled block.
    ///
    /// A disabled block imposes nothing and returns `Ok` immediately. An enabled block
    /// must name at least one issuer, with no duplicate `issuer` strings; each issuer
    /// must have an `https`/loopback origin, a non-empty audience, and a non-empty list
    /// of permitted RSA algorithms.
    ///
    /// # Errors
    /// Returns [`ConfigError`] naming the offending key (never quoting a value).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        if self.issuers.is_empty() {
            return Err(ConfigError::invalid(
                "auth.issuers",
                "must list at least one issuer when auth is enabled",
            ));
        }
        let mut seen = BTreeSet::new();
        for issuer in &self.issuers {
            if !seen.insert(issuer.issuer.as_str()) {
                return Err(ConfigError::invalid(
                    "auth.issuers",
                    "two entries declare the same issuer",
                ));
            }
        }
        for (i, issuer) in self.issuers.iter().enumerate() {
            if !issuer_origin_is_allowed(&issuer.issuer) {
                return Err(ConfigError::invalid(
                    format!("auth.issuers[{i}].issuer"),
                    "must be an https URL or an http loopback origin (cleartext issuers are \
                     refused)",
                ));
            }
            if issuer.audience.trim().is_empty() {
                return Err(ConfigError::missing(format!("auth.issuers[{i}].audience")));
            }
            if issuer.allowed_algs.is_empty() {
                return Err(ConfigError::invalid(
                    format!("auth.issuers[{i}].allowed_algs"),
                    "must list at least one algorithm",
                ));
            }
            for alg in &issuer.allowed_algs {
                if !PERMITTED_ALGS.contains(&alg.as_str()) {
                    return Err(ConfigError::invalid(
                        format!("auth.issuers[{i}].allowed_algs"),
                        "only RS256/RS384/RS512 are accepted; \"none\" and symmetric (HS*) \
                         algorithms are refused",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Soft startup advisories the host logs at startup (PR5). These are **not**
    /// validation errors: [`AuthConfig::validate`] returns `Ok` when only warnings apply.
    ///
    /// A disabled block produces no warnings. For an enabled block this flags
    /// writer-capable issuers with no durable-writer anchor (fork#1) and any unusually
    /// large clock-skew leeway, naming the issuer by index, never by value.
    pub fn startup_warnings(&self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut warnings = Vec::new();
        for (i, issuer) in self.issuers.iter().enumerate() {
            if issuer.allows_writes
                && issuer.agent_id_overrides.is_empty()
                && issuer.agent_id_claim.is_none()
            {
                warnings.push(format!(
                    "auth.issuers[{i}] permits durable writes but has no agent-id anchor: \
                     agent_id will be derived by hashing the token sub and can be orphaned by \
                     an issuer/sub migration; set agent_id_overrides or agent_id_claim"
                ));
            }
            if issuer.leeway_secs > LARGE_LEEWAY_SECS {
                warnings.push(format!(
                    "auth.issuers[{i}].leeway_secs is unusually large (over two minutes), which \
                     widens the window for replaying an expired or not-yet-valid token"
                ));
            }
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer-capable issuer anchored by an explicit agent-id claim, so it draws no
    /// fork#1 warning and clears validation when enabled.
    fn anchored_issuer() -> IssuerConfig {
        IssuerConfig {
            issuer: "https://issuer.example/".into(),
            audience: "https://api.aionforge.dev".into(),
            agent_id_claim: Some("https://aionforge.dev/agent_id".into()),
            ..IssuerConfig::default()
        }
    }

    #[test]
    fn the_default_block_is_off_validates_and_warns_nothing() {
        let config = AuthConfig::default();
        assert!(!config.enabled, "the master switch is off by default");
        assert!(config.issuers.is_empty());
        assert!(config.validate().is_ok());
        assert!(config.startup_warnings().is_empty());
    }

    #[test]
    fn enabled_with_no_issuers_fails_and_names_auth_issuers() {
        let config = AuthConfig {
            enabled: true,
            issuers: Vec::new(),
        };
        let err = config
            .validate()
            .expect_err("an enabled block needs an issuer");
        assert!(
            err.to_string().contains("auth.issuers"),
            "the error names the offending key: {err}"
        );
    }

    #[test]
    fn a_cleartext_issuer_is_rejected_while_https_and_loopback_are_accepted() {
        // Cleartext, non-loopback origins are refused. The spoof vectors below all begin
        // with a loopback literal but resolve to an attacker-controlled host, so a prefix
        // match would fail open; host-equality must reject every one of them.
        for bad_issuer in [
            "http://evil.example",
            "http://localhost.evil.example/",
            "http://127.0.0.1.evil.com/",
            "http://localhost.attacker.com",
            "http://localhostx",
            "http://localhostfoo/",
            "http://127.0.0.1@evil.example/",
            "http://[::1].evil/",
            // A structurally-empty https issuer has no host: meaningless config the gate
            // rejects rather than silently storing for PR2 to fetch.
            "https://",
            "https:///path",
        ] {
            let config = AuthConfig {
                enabled: true,
                issuers: vec![IssuerConfig {
                    issuer: bad_issuer.into(),
                    ..anchored_issuer()
                }],
            };
            let err = config
                .validate()
                .expect_err("cleartext non-loopback is refused");
            assert!(
                err.to_string().contains("auth.issuers[0].issuer"),
                "the error names the offending issuer for {bad_issuer}: {err}"
            );
        }

        for ok_issuer in [
            "https://issuer.example/",
            "http://localhost:8080/",
            "http://127.0.0.1:8080/",
            "http://[::1]:8080/",
            "http://127.0.0.1/",
        ] {
            let config = AuthConfig {
                enabled: true,
                issuers: vec![IssuerConfig {
                    issuer: ok_issuer.into(),
                    ..anchored_issuer()
                }],
            };
            assert!(
                config.validate().is_ok(),
                "{ok_issuer} is an acceptable origin"
            );
        }
    }

    #[test]
    fn an_empty_audience_fails_and_names_the_audience_key() {
        let config = AuthConfig {
            enabled: true,
            issuers: vec![IssuerConfig {
                audience: "   ".into(),
                ..anchored_issuer()
            }],
        };
        let err = config.validate().expect_err("an audience is required");
        assert!(
            err.to_string().contains("auth.issuers[0].audience"),
            "the error names the offending key: {err}"
        );
    }

    #[test]
    fn only_rsa_algorithms_are_accepted_and_the_message_hides_the_offender() {
        // The rejection message is a fixed string that never interpolates the offending
        // value: it reads identically for `none` and for a symmetric alg, so nothing the
        // token supplied leaks into the error. (The static text names `"none"` and `HS*`
        // as the refused *classes*; the assertion below pins that the supplied value
        // itself is never echoed by using an offender the fixed text does not name.)
        let mut messages = Vec::new();
        for bad in ["none", "HS256", "ES256", "PS256"] {
            let config = AuthConfig {
                enabled: true,
                issuers: vec![IssuerConfig {
                    allowed_algs: vec![bad.into()],
                    ..anchored_issuer()
                }],
            };
            let err = config
                .validate()
                .expect_err("non-RSA algorithms are refused");
            assert!(
                err.to_string().contains("auth.issuers[0].allowed_algs"),
                "the error names the offending key: {err}"
            );
            messages.push(err.to_string());
        }
        assert!(
            messages.windows(2).all(|pair| pair[0] == pair[1]),
            "the message is constant, so the offending value is never interpolated: {messages:?}"
        );
        // The two offenders the spec names directly. `HS256` is provably never echoed:
        // the fixed text names the `HS*` *class*, not the concrete `HS256` value.
        assert!(
            !messages[1].contains("HS256"),
            "the rejection must not quote the offending algorithm: {}",
            messages[1]
        );
        // `none` cannot be checked via `!contains`: the fixed text legitimately names the
        // `"none"` class (auth.rs static reason), so its literal collides with the value.
        // The message-constancy check above is what pins that `none` is not interpolated.
        //
        // `ES256` is a synthetic offender the fixed text never names, so it too is proven
        // un-echoed directly.
        assert!(
            !messages[2].contains("ES256"),
            "the rejection must not quote the offending algorithm: {}",
            messages[2]
        );
        // `PS256` pins the PS* (RSA-PSS) class as refused, and like ES256 the fixed text
        // never names it, so it is also proven un-echoed directly.
        assert!(
            !messages[3].contains("PS256"),
            "the rejection must not quote the offending algorithm: {}",
            messages[3]
        );

        let good = AuthConfig {
            enabled: true,
            issuers: vec![IssuerConfig {
                allowed_algs: vec!["RS256".into()],
                ..anchored_issuer()
            }],
        };
        assert!(good.validate().is_ok(), "RS256 is accepted");
    }

    #[test]
    fn duplicate_issuers_are_rejected() {
        let config = AuthConfig {
            enabled: true,
            issuers: vec![anchored_issuer(), anchored_issuer()],
        };
        let err = config.validate().expect_err("two entries share an issuer");
        assert!(
            err.to_string().contains("auth.issuers"),
            "the error names the offending key: {err}"
        );
        assert!(
            err.to_string().contains("same issuer"),
            "the error pins the duplicate-issuer path specifically: {err}"
        );
    }

    #[test]
    fn an_empty_algorithm_list_is_rejected_and_names_the_key() {
        // The empty-list branch is distinct from the per-algorithm permitted-set check:
        // an issuer that accepts zero algorithms must fail closed, never silently validate.
        let config = AuthConfig {
            enabled: true,
            issuers: vec![IssuerConfig {
                allowed_algs: Vec::new(),
                ..anchored_issuer()
            }],
        };
        let err = config
            .validate()
            .expect_err("an empty algorithm list is rejected");
        assert!(
            err.to_string().contains("auth.issuers[0].allowed_algs"),
            "the error names the offending key: {err}"
        );
        assert!(
            err.to_string().contains("at least one algorithm"),
            "the empty-list reason is distinct from the permitted-set rejection: {err}"
        );
    }

    #[test]
    fn an_unusually_large_leeway_warns_only_above_the_two_minute_threshold() {
        // The fork#1 anchor is set so the ONLY possible advisory is the leeway one, which
        // isolates the `> LARGE_LEEWAY_SECS` threshold and its strict-`>` boundary.
        let warned = AuthConfig {
            enabled: true,
            issuers: vec![IssuerConfig {
                leeway_secs: LARGE_LEEWAY_SECS + 1,
                ..anchored_issuer()
            }],
        };
        let warnings = warned.startup_warnings();
        assert_eq!(
            warnings.len(),
            1,
            "exactly the leeway warning: {warnings:?}"
        );
        assert!(
            warnings[0].contains("auth.issuers[0].leeway_secs"),
            "the warning names the offending key: {warnings:?}"
        );

        // Exactly at the threshold draws no warning (the boundary is strict `>`).
        let boundary = AuthConfig {
            enabled: true,
            issuers: vec![IssuerConfig {
                leeway_secs: LARGE_LEEWAY_SECS,
                ..anchored_issuer()
            }],
        };
        assert!(
            boundary.startup_warnings().is_empty(),
            "a leeway exactly at the threshold is not flagged"
        );
    }

    #[test]
    fn an_unanchored_writer_issuer_warns_but_an_anchor_or_read_only_does_not() {
        // A writer with no anchor draws the fork#1 orphaned-namespace warning.
        let unanchored = AuthConfig {
            enabled: true,
            issuers: vec![IssuerConfig {
                issuer: "https://issuer.example/".into(),
                audience: "https://api.aionforge.dev".into(),
                ..IssuerConfig::default()
            }],
        };
        assert!(unanchored.validate().is_ok(), "warnings are not errors");
        let warnings = unanchored.startup_warnings();
        assert_eq!(
            warnings.len(),
            1,
            "exactly the fork#1 warning: {warnings:?}"
        );
        assert!(
            warnings[0].contains("auth.issuers[0]"),
            "the warning names the issuer index: {warnings:?}"
        );

        // The same issuer with an agent-id claim is anchored, so no warning.
        let anchored = AuthConfig {
            enabled: true,
            issuers: vec![anchored_issuer()],
        };
        assert!(
            anchored.startup_warnings().is_empty(),
            "an anchored writer draws no warning"
        );

        // A read-only issuer needs no anchor, so no warning even without one.
        let read_only = AuthConfig {
            enabled: true,
            issuers: vec![IssuerConfig {
                issuer: "https://issuer.example/".into(),
                audience: "https://api.aionforge.dev".into(),
                allows_writes: false,
                ..IssuerConfig::default()
            }],
        };
        assert!(
            read_only.startup_warnings().is_empty(),
            "a read-only issuer needs no namespace anchor"
        );
    }

    #[test]
    fn the_teams_allow_list_is_fail_closed_an_empty_set_validates_and_grants_nothing() {
        // PR3 blocking precondition (b), config half: the mandatory teams allow-list is
        // fail-closed. An *empty* allow-list is a valid configuration — it simply grants no teams
        // from this issuer (the PR3 mapper drops every team name, including a spoofed reserved
        // `system`/`global`). Validation must accept it, never require a non-empty list.
        let empty = AuthConfig {
            enabled: true,
            issuers: vec![anchored_issuer()], // default teams_allowlist is empty
        };
        assert!(
            empty.issuers[0].teams_allowlist.is_empty(),
            "the default allow-list is empty"
        );
        assert!(
            empty.validate().is_ok(),
            "an empty allow-list is a valid, fail-closed posture (grants no teams)"
        );

        // A populated allow-list of plain name KEYS also validates.
        let mut issuer = anchored_issuer();
        issuer.teams_allowlist.insert("platform".into());
        issuer.teams_allowlist.insert("payments".into());
        let populated = AuthConfig {
            enabled: true,
            issuers: vec![issuer],
        };
        assert!(populated.validate().is_ok());
    }

    #[test]
    fn the_posture_round_trips_through_json() {
        let mut issuer = anchored_issuer();
        issuer.teams_allowlist.insert("platform".into());
        issuer.operator_permission = Some("console:operate".into());
        issuer
            .agent_id_overrides
            .insert("sub-123".into(), Id::from_content_hash(b"durable-writer"));
        let config = AuthConfig {
            enabled: true,
            issuers: vec![issuer],
        };

        let json = serde_json::to_string(&config).expect("serialize");
        let back: AuthConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, config);
    }
}
