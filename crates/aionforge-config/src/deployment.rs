//! Named per-deployment auth+server blocks and the runtime selector (PR6b).
//!
//! Its own module because a deployment is a coherent unit: one named bundle of the
//! [`AuthConfig`] resource-server posture and the [`ServerHttpConfig`] transport posture,
//! so an operator can carry production, staging, and local profiles in a single
//! `config.toml` and switch a server between them at startup.
//!
//! **DEFAULT-OFF is the keystone.** A config that declares no `[deployments.*]` blocks and
//! names no selector is byte-for-byte today's config: [`Config::activate_deployment`]
//! returns immediately on `(None, empty)` *before* any mutation or re-validation, so the
//! top-level `[auth]`/`[server]` blocks are the live posture exactly as before this PR.
//!
//! When deployments *are* declared the selector is fail-closed: declaring deployments but
//! naming none is an error (you must choose), and naming a deployment that does not exist is
//! an error. Every *declared* deployment — active or not — is validated at load by
//! [`Config::validate`], so a broken inactive profile is caught before it can be selected.
//! Selector errors name only the **key** (`active_deployment`), never the requested
//! deployment value, keeping the error space secret-free.

use serde::{Deserialize, Serialize};

use crate::auth::AuthConfig;
use crate::config::Config;
use crate::error::ConfigError;
use crate::server::ServerHttpConfig;

/// One named deployment profile: a bundle of an [`AuthConfig`] resource-server posture and a
/// [`ServerHttpConfig`] transport posture, selected by name to become the live `[auth]` and
/// `[server]` blocks.
///
/// Both fields default, so an empty `[deployments.<name>]` table yields the same all-default
/// posture as omitting the top-level blocks. Activating a deployment splices these two blocks
/// over the top-level [`Config::auth`] and [`Config::server`] fields and re-validates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeploymentConfig {
    /// The OAuth resource-server posture this deployment runs (master switch + issuers).
    /// Default-off, exactly like the top-level [`Config::auth`] block.
    pub auth: AuthConfig,
    /// The Streamable HTTP transport posture this deployment runs (bind address, allow-lists,
    /// session statefulness). All-default reproduces today's loopback behavior, exactly like
    /// the top-level [`Config::server`] block.
    pub server: ServerHttpConfig,
}

impl Config {
    /// Activate a named deployment, splicing its `[auth]` and `[server]` blocks over the
    /// top-level [`Config::auth`] and [`Config::server`] fields and re-validating the result.
    ///
    /// The selector is resolved by the caller (a CLI flag overriding
    /// [`Config::active_deployment`]) and passed here as `selected`. The behavior is keyed on
    /// `(selected, whether any deployment is declared)`:
    ///
    /// - **`(None, no deployments)`** — DEFAULT-OFF. Returns `Ok(())` immediately, *before any
    ///   mutation or re-validation*. This is the backward-compatibility keystone: a config with
    ///   no deployments and no selector is left byte-for-byte unchanged.
    /// - **`(None, deployments declared)`** — fail-closed. Returns
    ///   [`ConfigError::missing`]`("active_deployment")`: declaring deployments forces an
    ///   explicit choice rather than silently running the top-level blocks.
    /// - **`(Some(name), _)`** — looks `name` up under `[deployments]`. An unknown name is
    ///   [`ConfigError::invalid`]`("active_deployment", …)`; a known one has its `auth` and
    ///   `server` blocks cloned over the top-level fields, then the whole [`Config`] is
    ///   re-validated.
    ///
    /// Selector errors name only the `active_deployment` key and never echo the requested
    /// deployment value, keeping the error space secret-free.
    ///
    /// # Errors
    /// Returns [`ConfigError::missing`] when deployments are declared but none is selected,
    /// [`ConfigError::invalid`] when the selected name is not declared, or any validation
    /// error surfaced by re-running [`Config::validate`] after the splice.
    pub fn activate_deployment(&mut self, selected: Option<&str>) -> Result<(), ConfigError> {
        match (selected, self.deployments.is_empty()) {
            // DEFAULT-OFF: no deployments and no selector — return before any mutation or
            // re-validation so the config is left byte-for-byte unchanged.
            (None, true) => Ok(()),
            // Deployments are declared but none was selected: fail closed, naming only the key.
            (None, false) => Err(ConfigError::missing("active_deployment")),
            // A name was selected: it must be declared, then its blocks become the live posture.
            (Some(name), _) => {
                let Some(deployment) = self.deployments.get(name) else {
                    return Err(ConfigError::invalid(
                        "active_deployment",
                        "names a deployment not declared under [deployments]",
                    ));
                };
                self.auth = deployment.auth.clone();
                self.server = deployment.server.clone();
                self.validate()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IssuerConfig;

    /// A deployment whose enabled auth block validates: one anchored issuer with an
    /// https origin and a non-empty audience.
    fn enabled_deployment() -> DeploymentConfig {
        DeploymentConfig {
            auth: AuthConfig {
                enabled: true,
                issuers: vec![IssuerConfig {
                    issuer: "https://issuer.example/".into(),
                    audience: "https://api.aionforge.dev".into(),
                    agent_id_claim: Some("https://aionforge.dev/agent_id".into()),
                    ..IssuerConfig::default()
                }],
            },
            server: ServerHttpConfig {
                listen: "0.0.0.0:8080".parse().expect("addr"),
                ..ServerHttpConfig::default()
            },
        }
    }

    #[test]
    fn default_off_no_deployments_no_selector_is_a_no_op() {
        // The keystone: an empty-deployments config with no selector is left byte-for-byte
        // unchanged, and the call succeeds without touching auth/server.
        let before = Config::default();
        let mut after = before.clone();
        after
            .activate_deployment(None)
            .expect("the default-off path is Ok");
        assert_eq!(after, before, "default-off mutates nothing");
    }

    #[test]
    fn declared_deployments_with_no_selector_fail_closed() {
        let mut config = Config::default();
        config
            .deployments
            .insert("prod".into(), enabled_deployment());
        let err = config
            .activate_deployment(None)
            .expect_err("declaring deployments forces a choice");
        assert!(
            matches!(err, ConfigError::Missing(ref key) if key == "active_deployment"),
            "the error names only the active_deployment key: {err}"
        );
    }

    #[test]
    fn an_unknown_deployment_name_is_rejected_without_echoing_the_value() {
        let mut config = Config::default();
        config
            .deployments
            .insert("prod".into(), enabled_deployment());
        let err = config
            .activate_deployment(Some("staging-secret-name"))
            .expect_err("an unknown name is rejected");
        match &err {
            ConfigError::Invalid { key, reason } => {
                assert_eq!(key, "active_deployment", "names only the key");
                assert!(
                    !reason.contains("staging-secret-name"),
                    "the reason never echoes the requested value: {reason}"
                );
            }
            other => panic!("expected an invalid-key error, got {other}"),
        }
        assert!(
            !err.to_string().contains("staging-secret-name"),
            "the rendered error never echoes the requested value: {err}"
        );
    }

    #[test]
    fn selecting_a_deployment_splices_its_auth_and_server_blocks() {
        let deployment = enabled_deployment();
        let mut config = Config::default();
        assert_ne!(
            config.auth, deployment.auth,
            "precondition: the top-level auth differs from the deployment's"
        );
        assert_ne!(
            config.server, deployment.server,
            "precondition: the top-level server differs from the deployment's"
        );
        config.deployments.insert("prod".into(), deployment.clone());

        config
            .activate_deployment(Some("prod"))
            .expect("a declared, valid deployment activates");

        assert_eq!(config.auth, deployment.auth, "auth is spliced from prod");
        assert_eq!(
            config.server, deployment.server,
            "server is spliced from prod"
        );
    }

    #[test]
    fn a_broken_inactive_deployment_is_caught_at_load_naming_the_deployment() {
        // An enabled auth block with a cleartext non-loopback issuer is rejected by
        // AuthConfig::validate. As an *inactive* deployment it must still be caught by the
        // top-level Config::validate loop, and the surfaced key is located under the
        // deployment so an operator can find the broken profile.
        let mut config = Config::default();
        config.deployments.insert(
            "broken".into(),
            DeploymentConfig {
                auth: AuthConfig {
                    enabled: true,
                    issuers: vec![IssuerConfig {
                        issuer: "http://evil.example".into(),
                        audience: "https://api.aionforge.dev".into(),
                        ..IssuerConfig::default()
                    }],
                },
                server: ServerHttpConfig::default(),
            },
        );
        let err = config
            .validate()
            .expect_err("a broken inactive deployment fails validation at load");
        assert!(
            err.to_string().contains("deployments.broken."),
            "the error locates the broken deployment by name: {err}"
        );
    }

    #[test]
    fn the_deployment_block_round_trips_through_json() {
        let deployment = enabled_deployment();
        let json = serde_json::to_string(&deployment).expect("serialize");
        let back: DeploymentConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, deployment);
    }
}
