//! Acceptance tests for the named-deployment switcher (PR6b).
//!
//! Pins the deployment contract end to end through a full figment load: named
//! `[deployments.<name>]` `[auth]`+`[server]` blocks parse and validate (active or not);
//! `Config::activate_deployment` splices the selected block over the top-level posture; the
//! `active_deployment` key maps from `AIONFORGE_ACTIVE_DEPLOYMENT` for free; and the
//! DEFAULT-OFF path (no deployments, no selector) leaves the config byte-for-byte unchanged.
//! Fail-closed and secret-free discipline are asserted on every error path.

// figment::Jail's closure returns Result<(), figment::Error>, whose Err variant is large;
// that is the harness's type, not ours, so allow it in this test file.
#![allow(clippy::result_large_err)]

use std::path::Path;

use aionforge_config::{Config, ConfigError};

use figment::providers::{Format, Serialized, Toml};
use figment::{Figment, Jail};

/// Enter a config-loading jail with no inherited `AIONFORGE_*` variables.
///
/// The layered loader reads `Env::prefixed("AIONFORGE_")` from the live process environment,
/// and figment's [`Jail`] restores env *writes* on drop but does not clear inherited variables
/// at entry. `set_env` cannot neutralise a variable a test does not name, and `remove_var` is
/// forbidden workspace-wide, so we clear everything and immediately restore `HOME` (read by
/// `Config::default` → `default_data_dir`). [`Jail`] serializes jails under a global lock and
/// restores the full environment on drop, so this is race-free and self-undoing.
fn clear_inherited_env(jail: &mut Jail) {
    let home = std::env::var_os("HOME");
    jail.clear_env();
    if let Some(home) = home {
        jail.set_env("HOME", home.to_string_lossy());
    }
}

/// A `config.toml` declaring two named deployments — a default-off `local` and an enabled,
/// stateless `prod` — for the deployment-switcher acceptance tests. Both deployments validate
/// in isolation; selection is what splices one over the top-level blocks.
const DEPLOYMENT_TOML: &str = r#"
    [deployments.local.server]
    listen = "127.0.0.1:3918"

    [deployments.prod.server]
    listen = "0.0.0.0:8443"
    stateful = false

    [deployments.prod.auth]
    enabled = true

    [[deployments.prod.auth.issuers]]
    issuer = "https://issuer.example/"
    audience = "https://api.aionforge.dev"
    agent_id_claim = "https://aionforge.dev/agent_id"
"#;

#[test]
fn declared_deployments_parse_from_toml_and_validate_through_config() {
    // The deployment blocks ride the same figment/serde path as the top-level [auth]/[server].
    // A declared, inactive `prod` (enabled auth) is validated at load alongside `local`.
    let figment =
        Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(DEPLOYMENT_TOML));
    let config: Config = figment.extract().expect("extract");
    config
        .validate()
        .expect("two sound deployments validate at load");
    assert_eq!(config.deployments.len(), 2, "both profiles parse");
    assert!(config.deployments.contains_key("local"));
    assert!(config.deployments["prod"].auth.enabled);
    assert!(!config.deployments["prod"].server.stateful);
    assert_eq!(
        config.deployments["prod"].server.listen.to_string(),
        "0.0.0.0:8443"
    );
    // Nothing is selected yet, so the top-level blocks are untouched defaults.
    assert!(
        !config.auth.enabled,
        "no selection leaves top-level auth default"
    );
}

#[test]
fn default_off_with_no_deployments_round_trips_a_single_block_config_identically() {
    // The backward-compat keystone at the full-config level: a config with no deployments and
    // no selector is left byte-for-byte unchanged by activate_deployment(None).
    let figment = Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(
        r#"
            [server]
            listen = "0.0.0.0:8080"
            stateful = false

            [auth]
            enabled = false
        "#,
    ));
    let mut config: Config = figment.extract().expect("extract");
    config.validate().expect("a single-block config validates");
    let before = config.clone();
    config
        .activate_deployment(None)
        .expect("default-off no-op is Ok");
    assert_eq!(config, before, "default-off mutates nothing");
}

#[test]
fn activating_a_named_deployment_splices_its_auth_and_server_blocks() {
    let figment =
        Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(DEPLOYMENT_TOML));
    let mut config: Config = figment.extract().expect("extract");
    config.validate().expect("validate");
    let expected = config.deployments["prod"].clone();

    config
        .activate_deployment(Some("prod"))
        .expect("prod is declared and valid");

    assert_eq!(config.auth, expected.auth, "auth spliced from prod");
    assert_eq!(config.server, expected.server, "server spliced from prod");
    assert!(config.auth.enabled, "the enabled prod auth is now live");
}

#[test]
fn the_active_deployment_key_selects_for_free_from_the_environment() {
    // The free figment env path: AIONFORGE_ACTIVE_DEPLOYMENT maps to Config::active_deployment
    // via the `__`-split env provider, with no per-key wiring. Proven inside a Jail so the env
    // write is scoped and restored, never a global mutation.
    Jail::expect_with(|jail| {
        clear_inherited_env(jail);
        jail.create_file("config.toml", DEPLOYMENT_TOML)?;
        jail.set_env("AIONFORGE_ACTIVE_DEPLOYMENT", "prod");

        let mut config =
            Config::from_figment(Config::figment(Path::new("config.toml"))).expect("load");
        assert_eq!(
            config.active_deployment.as_deref(),
            Some("prod"),
            "the env var lands on active_deployment for free"
        );
        // Apply the selector exactly as the host does (no flag override here).
        let selected = config.active_deployment.clone();
        config
            .activate_deployment(selected.as_deref())
            .expect("the env-selected prod activates");
        assert!(config.auth.enabled, "the env selector spliced prod's auth");
        Ok(())
    });
}

#[test]
fn the_flag_beats_env_and_file_at_the_host_seam_when_all_three_are_set() {
    // The composed three-tier precedence pinned at the host seam: a file key AND
    // AIONFORGE_ACTIVE_DEPLOYMENT both name `prod`, while the flag names the default-off `local`.
    // The flag must win over both. Reproduces the host's selector expression
    // (`flag.or_else(|| config.active_deployment)`) so the flag-beats-env tier — covered only
    // transitively elsewhere — is pinned directly, inside a Jail so the env write is scoped.
    Jail::expect_with(|jail| {
        clear_inherited_env(jail);
        jail.create_file(
            "config.toml",
            &format!("active_deployment = \"prod\"\n{DEPLOYMENT_TOML}"),
        )?;
        jail.set_env("AIONFORGE_ACTIVE_DEPLOYMENT", "prod");

        let mut config =
            Config::from_figment(Config::figment(Path::new("config.toml"))).expect("load");
        assert_eq!(
            config.active_deployment.as_deref(),
            Some("prod"),
            "env-over-file resolves to prod before the flag is consulted"
        );
        // The host seam: a `--deployment local` flag overrides the resolved prod selector.
        let flag = Some("local".to_owned());
        let selected = flag.or_else(|| config.active_deployment.clone());
        assert_eq!(
            selected.as_deref(),
            Some("local"),
            "the flag wins over env+file"
        );
        config
            .activate_deployment(selected.as_deref())
            .expect("the flag-selected local activates");
        assert!(
            !config.auth.enabled,
            "the flag selected the default-off local profile, beating the prod env+file key"
        );
        Ok(())
    });
}

#[test]
fn an_unknown_active_deployment_is_rejected_without_echoing_the_value() {
    let figment =
        Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(DEPLOYMENT_TOML));
    let mut config: Config = figment.extract().expect("extract");
    let err = config
        .activate_deployment(Some("nonexistent-secret"))
        .expect_err("an unknown deployment is rejected");
    assert!(
        matches!(&err, ConfigError::Invalid { key, .. } if key == "active_deployment"),
        "the error names only the active_deployment key: {err}"
    );
    assert!(
        !err.to_string().contains("nonexistent-secret"),
        "the error never echoes the requested deployment value: {err}"
    );
}

#[test]
fn declared_deployments_with_no_selection_fail_closed() {
    let figment =
        Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(DEPLOYMENT_TOML));
    let mut config: Config = figment.extract().expect("extract");
    let err = config
        .activate_deployment(None)
        .expect_err("declared deployments force an explicit choice");
    assert!(
        matches!(&err, ConfigError::Missing(key) if key == "active_deployment"),
        "the error names the active_deployment key: {err}"
    );
}

#[test]
fn a_broken_inactive_deployment_fails_at_load_locating_the_profile() {
    // An enabled deployment with a cleartext, non-loopback issuer is rejected by the auth
    // validator even though it is never selected — Config::validate validates every declared
    // profile. The surfaced key is located under `deployments.<name>.`, and the broken issuer
    // value is never echoed.
    let figment = Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(
        r#"
            [deployments.broken.auth]
            enabled = true

            [[deployments.broken.auth.issuers]]
            issuer = "http://evil.example"
            audience = "https://api.aionforge.dev"
        "#,
    ));
    let config: Config = figment.extract().expect("extract");
    let err = config
        .validate()
        .expect_err("a broken inactive deployment fails at load");
    match &err {
        ConfigError::Invalid { key, reason } => {
            assert!(
                key.starts_with("deployments.broken."),
                "the key locates the broken profile: {key}"
            );
            assert!(
                !reason.contains("evil.example"),
                "the reason never echoes the offending issuer value: {reason}"
            );
        }
        other => panic!("expected an invalid-key error under the deployment, got {other}"),
    }
    assert!(
        !err.to_string().contains("evil.example"),
        "the rendered error never echoes the offending value: {err}"
    );
}
