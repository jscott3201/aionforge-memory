//! Acceptance tests for the layered configuration model.
//!
//! Pins the T07 contract: config loads from defaults, a file, the environment, and
//! flags with documented precedence; missing or out-of-range required keys fail with a
//! clear, key-naming message; and secrets never reach the config or a log.

// figment::Jail's closure returns Result<(), figment::Error>, whose Err variant is
// large; that is the harness's type, not ours, so allow it in this test file.
#![allow(clippy::result_large_err)]

use std::path::Path;

use aionforge_config::{Config, ConfigError, default_config_path};

use figment::Jail;
use figment::providers::{Format, Toml};
use secrecy::ExposeSecret;

#[test]
fn defaults_are_usable_and_valid() {
    let config = Config::default();
    config.validate().expect("defaults validate");
    assert_eq!(config.embedder.dimension, 1536);
    assert_eq!(config.retrieval.fusion_constant, 60);
    assert!(config.security.redaction);
    assert!(!config.security.signed_writes);
    // The store config is derived from the embedder dimension.
    assert_eq!(config.store_config().embedding_dimension, 1536);
}

#[test]
fn layers_apply_in_precedence_order() {
    Jail::expect_with(|jail| {
        // File layer overrides defaults; it sets dimension and model.
        jail.create_file(
            "config.toml",
            "[embedder]\ndimension = 100\nmodel = \"from-file\"\n",
        )?;
        // Environment layer overrides the file's dimension (nested on `__`).
        jail.set_env("AIONFORGE_EMBEDDER__DIMENSION", "200");

        let base = Config::figment(Path::new("config.toml"));
        let from_env = Config::from_figment(base.clone()).expect("load file + env");
        assert_eq!(from_env.embedder.dimension, 200, "env beats file");
        assert_eq!(from_env.embedder.model, "from-file", "file beats default");

        // Flags layer (merged last) overrides the environment.
        let with_flags = base.merge(Toml::string("[embedder]\ndimension = 300\n"));
        let from_flags = Config::from_figment(with_flags).expect("load with flags");
        assert_eq!(from_flags.embedder.dimension, 300, "flags beat env");
        assert_eq!(from_flags.embedder.model, "from-file", "file still applies");
        Ok(())
    });
}

#[test]
fn environment_sets_the_data_directory() {
    Jail::expect_with(|jail| {
        jail.set_env("AIONFORGE_PERSISTENCE__DATA_DIR", "/srv/aionforge-test");
        let config =
            Config::from_figment(Config::figment(Path::new("config.toml"))).expect("load from env");
        assert_eq!(config.data_dir(), Path::new("/srv/aionforge-test"));
        Ok(())
    });
}

#[test]
fn a_missing_file_is_skipped() {
    Jail::expect_with(|jail| {
        // No config.toml created; loading falls back to defaults + env.
        let _ = jail;
        let config = Config::from_figment(Config::figment(Path::new("config.toml")))
            .expect("load with no file");
        assert_eq!(config.embedder.dimension, 1536);
        Ok(())
    });
}

#[test]
fn load_reads_the_default_config_path() {
    Jail::expect_with(|jail| {
        // Point HOME at the jail so the default path resolves inside it, then drop a
        // config file where `Config::load` will look for it.
        let home = jail.directory().to_path_buf();
        jail.set_env("HOME", home.to_string_lossy().to_string());
        std::fs::create_dir_all(home.join(".aionforge")).expect("create data dir");
        std::fs::write(
            home.join(".aionforge/config.toml"),
            "[embedder]\ndimension = 256\n",
        )
        .expect("write config file");

        let config = Config::load().expect("load from the default path");
        assert_eq!(
            config.embedder.dimension, 256,
            "the default-path file is read"
        );
        Ok(())
    });
}

#[test]
fn the_default_config_path_sits_under_the_data_dir() {
    assert!(default_config_path().ends_with(".aionforge/config.toml"));
}

#[test]
fn a_zero_dimension_fails_clearly() {
    let mut config = Config::default();
    config.embedder.dimension = 0;
    let error = config.validate().expect_err("zero dimension is rejected");
    assert!(
        matches!(&error, ConfigError::Invalid { key, .. } if key == "embedder.dimension"),
        "error names the key: {error}"
    );
}

#[test]
fn an_enabled_embedder_requires_an_endpoint_and_model() {
    let mut config = Config::default();
    config.embedder.enabled = true;
    config.embedder.endpoint = String::new();
    assert!(
        matches!(config.validate(), Err(ConfigError::Missing(key)) if key == "embedder.endpoint")
    );

    let mut config = Config::default();
    config.embedder.model = "   ".to_owned();
    assert!(matches!(config.validate(), Err(ConfigError::Missing(key)) if key == "embedder.model"));
}

#[test]
fn a_disabled_embedder_skips_endpoint_validation() {
    let mut config = Config::default();
    config.embedder.enabled = false;
    config.embedder.endpoint = String::new();
    config.embedder.model = String::new();
    config
        .validate()
        .expect("a disabled embedder needs no endpoint");
}

#[test]
fn plain_http_is_rejected_unless_loopback() {
    let allowed = [
        "http://localhost:1234/v1",
        "http://127.0.0.1:1234/v1",
        "https://api.example.com/v1",
    ];
    for endpoint in allowed {
        let mut config = Config::default();
        config.embedder.endpoint = endpoint.to_owned();
        config
            .validate()
            .unwrap_or_else(|e| panic!("{endpoint} should pass: {e}"));
    }

    let mut config = Config::default();
    config.embedder.endpoint = "http://api.example.com/v1".to_owned();
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "embedder.endpoint"),
        "remote plain http is rejected"
    );
}

#[test]
fn ipv6_loopback_over_plain_http_is_allowed() {
    for endpoint in ["http://[::1]:1234/v1", "http://[::1]/v1"] {
        let mut config = Config::default();
        config.embedder.endpoint = endpoint.to_owned();
        config
            .validate()
            .unwrap_or_else(|e| panic!("{endpoint} is loopback and should pass: {e}"));
    }

    // A non-loopback IPv6 host over plain http is still rejected.
    let mut config = Config::default();
    config.embedder.endpoint = "http://[2001:db8::1]:1234/v1".to_owned();
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "embedder.endpoint"),
        "a remote IPv6 host over plain http is rejected"
    );
}

#[test]
fn the_api_key_lives_in_the_environment_not_the_config() {
    let mut config = Config::default();
    config.embedder.api_key_env = Some("AIONFORGE_TEST_KEY".to_owned());

    // The config carries the variable name, never a key. Its Debug shows the name only.
    let debug = format!("{config:?}");
    assert!(
        debug.contains("AIONFORGE_TEST_KEY"),
        "the env var name is fine to log"
    );
    assert!(
        !debug.contains("super-secret-value"),
        "no key value is anywhere in the config"
    );

    // Resolution reads the named variable into a SecretString that redacts in logs.
    let secret = config
        .resolve_api_key(|name| {
            (name == "AIONFORGE_TEST_KEY").then(|| "super-secret-value".to_owned())
        })
        .expect("resolve")
        .expect("a key is present");
    assert_eq!(secret.expose_secret(), "super-secret-value");
    let secret_debug = format!("{secret:?}");
    assert!(
        !secret_debug.contains("super-secret-value"),
        "the secret redacts in its Debug output: {secret_debug}"
    );
}

#[test]
fn a_named_but_unset_api_key_variable_fails_clearly() {
    let mut config = Config::default();
    config.embedder.api_key_env = Some("AIONFORGE_MISSING_KEY".to_owned());
    let error = config
        .resolve_api_key(|_| None)
        .expect_err("an unset named variable is an error");
    match error {
        ConfigError::SecretEnvMissing(name) => assert_eq!(name, "AIONFORGE_MISSING_KEY"),
        other => panic!("wrong error: {other}"),
    }
}

#[test]
fn no_api_key_variable_resolves_to_none() {
    let config = Config::default();
    assert!(config.embedder.api_key_env.is_none());
    assert!(
        config
            .resolve_api_key(|_| panic!("lookup must not run when no variable is named"))
            .expect("resolve")
            .is_none()
    );
}

#[test]
fn a_non_integer_dimension_in_the_environment_fails_clearly() {
    Jail::expect_with(|jail| {
        jail.set_env("AIONFORGE_EMBEDDER__DIMENSION", "not-a-number");
        let error = Config::from_figment(Config::figment(Path::new("config.toml")))
            .expect_err("a non-integer dimension is rejected");
        // The loader's message names the field; it is the Load variant.
        match error {
            ConfigError::Load(message) => assert!(
                message.to_ascii_uppercase().contains("DIMENSION"),
                "the message names the offending field: {message}"
            ),
            other => panic!("expected a Load error, got: {other}"),
        }
        Ok(())
    });
}

#[test]
fn malformed_toml_fails_clearly() {
    Jail::expect_with(|jail| {
        jail.create_file("config.toml", "[embedder]\ndimension = = =\n")?;
        let error = Config::from_figment(Config::figment(Path::new("config.toml")))
            .expect_err("malformed TOML is rejected");
        assert!(
            matches!(error, ConfigError::Load(_)),
            "a parse failure is a Load error: {error}"
        );
        Ok(())
    });
}
