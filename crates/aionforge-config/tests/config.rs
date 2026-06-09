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
fn an_embedder_timeout_must_be_within_bounds() {
    let mut config = Config::default();
    config.embedder.timeout_ms = 0;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "embedder.timeout_ms"),
        "a zero timeout is rejected"
    );

    let mut config = Config::default();
    config.embedder.timeout_ms = 600_001;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "embedder.timeout_ms"),
        "an absurdly large timeout is rejected"
    );

    let mut config = Config::default();
    config.embedder.timeout_ms = 600_000;
    config.validate().expect("the ceiling itself is allowed");
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
fn signed_writes_bound_the_clock_skew_tolerance() {
    // Off by default: the tolerance is inert and never validated.
    let mut config = Config::default();
    config.security.signed_writes = false;
    config.security.clock_skew_tolerance_ms = 0;
    config
        .validate()
        .expect("an off signed-writes policy ignores the tolerance");

    // On: a zero window would reject every write (skew is always >= 0), so it is a config error.
    let mut config = Config::default();
    config.security.signed_writes = true;
    config.security.clock_skew_tolerance_ms = 0;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "security.clock_skew_tolerance_ms"),
        "a zero tolerance with signed writes on is rejected"
    );

    // On: above the five-minute ceiling is rejected.
    let mut config = Config::default();
    config.security.signed_writes = true;
    config.security.clock_skew_tolerance_ms = 300_001;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "security.clock_skew_tolerance_ms"),
        "a tolerance above the ceiling is rejected"
    );

    // On: the ceiling itself is allowed.
    let mut config = Config::default();
    config.security.signed_writes = true;
    config.security.clock_skew_tolerance_ms = 300_000;
    config.validate().expect("the ceiling itself is allowed");
}

#[test]
fn promotion_gates_are_validated_only_when_enabled() {
    use aionforge_config::CategoryPromotionRule;

    // Off by default: a nonsense quorum and threshold are inert and never validated.
    let mut config = Config::default();
    config.promotion.enabled = false;
    config.promotion.default_k = 1;
    config.promotion.default_threshold = 0.0;
    config
        .validate()
        .expect("an off promotion policy ignores its gates");

    // On: a quorum of one is not a quorum.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.default_k = 1;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.default_k"),
        "a quorum of one is rejected"
    );

    // On: a threshold at or below 0.5 lets the uninformative prior alone clear it.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.default_threshold = 0.5;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.default_threshold"),
        "a threshold at 0.5 is rejected"
    );

    // On: a threshold above 1.0 is unreachable and would lock promotion shut.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.default_threshold = 1.0001;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.default_threshold"),
        "a threshold above 1.0 is rejected"
    );

    // On: a NaN threshold fails every ordered comparison and is rejected.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.default_threshold = f64::NAN;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.default_threshold"),
        "a NaN threshold is rejected"
    );

    // On: an in-range threshold that the quorum cannot reach under the prior is rejected — the
    // two gates would be mutually unsatisfiable. With k = 3 and Beta(1, 1) the posterior tops out
    // at 4/5, so 0.90 is unreachable.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.default_threshold = 0.90;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.default_threshold"),
        "a threshold unreachable at the default k is rejected"
    );

    // On: a non-positive Beta prior is rejected.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.prior_alpha = 0.0;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.prior_alpha"),
        "a zero prior is rejected"
    );

    // On: an empty default category leaves no bucket for uncategorized attestations.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.default_category = "  ".to_string();
    assert!(
        matches!(config.validate(), Err(ConfigError::Missing(key)) if key == "promotion.default_category"),
        "an empty default category is rejected"
    );

    // On: a per-category override is held to the same bounds, naming the category.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.categories.insert(
        "pii".to_string(),
        CategoryPromotionRule {
            k: 5,
            threshold: 1.5,
        },
    );
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.categories.pii.threshold"),
        "a bad per-category threshold is rejected and names the category"
    );

    // On: a per-category override also faces the reachability guard. A threshold of 0.99 needs a
    // far larger quorum than k = 5 can supply under the prior, so the pairing is rejected.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.categories.insert(
        "pii".to_string(),
        CategoryPromotionRule {
            k: 5,
            threshold: 0.99,
        },
    );
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "promotion.categories.pii.threshold"),
        "a per-category threshold unreachable at its k is rejected and names the category"
    );

    // On: sensible higher-k, higher-threshold sensitive overrides validate. With k = 5 the
    // posterior reaches 6/7 ≈ 0.857, so a 0.85 bar (stricter than the 0.80 default on both axes)
    // is satisfiable.
    let mut config = Config::default();
    config.promotion.enabled = true;
    config.promotion.categories.insert(
        "pii".to_string(),
        CategoryPromotionRule {
            k: 5,
            threshold: 0.85,
        },
    );
    config
        .validate()
        .expect("a stricter sensitive-category override is valid");
}

#[test]
fn reliability_weights_are_validated_only_when_enabled() {
    // Off by default: nonsense weights and priors are inert and never validated.
    let mut config = Config::default();
    config.reliability.enabled = false;
    config.reliability.w_agree = 9.0;
    config.reliability.prior_alpha = -1.0;
    config
        .validate()
        .expect("an off reliability policy ignores its weights");

    // On with the defaults validates: Beta(1, 1), w_agree 0.25 < w_contradict 1.0.
    let mut config = Config::default();
    config.reliability.enabled = true;
    config
        .validate()
        .expect("the default reliability weights are valid when enabled");

    // On: a non-positive Beta prior is rejected.
    let mut config = Config::default();
    config.reliability.enabled = true;
    config.reliability.prior_alpha = 0.0;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "reliability.prior_alpha"),
        "a zero prior is rejected"
    );

    // On: a negative weight could push a pseudo-count negative and is rejected.
    let mut config = Config::default();
    config.reliability.enabled = true;
    config.reliability.w_contradict = -1.0;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "reliability.w_contradict"),
        "a negative weight is rejected"
    );

    // On: a NaN weight fails the finiteness check.
    let mut config = Config::default();
    config.reliability.enabled = true;
    config.reliability.w_attest_invalid = f64::NAN;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "reliability.w_attest_invalid"),
        "a NaN weight is rejected"
    );

    // On: the asymmetry guard — agreement gain at or above contradiction decay is rejected.
    let mut config = Config::default();
    config.reliability.enabled = true;
    config.reliability.w_agree = 1.0; // equal to the default w_contradict
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "reliability.w_agree"),
        "agreement gain that equals the decay is rejected (no strict asymmetry)"
    );

    // On: an empty default category leaves no bucket for uncategorized updates.
    let mut config = Config::default();
    config.reliability.enabled = true;
    config.reliability.default_category = "  ".to_string();
    assert!(
        matches!(config.validate(), Err(ConfigError::Missing(key)) if key == "reliability.default_category"),
        "an empty default category is rejected"
    );

    // On: a decay-only posture (w_agree = 0) is valid — zero is strictly below the decay.
    let mut config = Config::default();
    config.reliability.enabled = true;
    config.reliability.w_agree = 0.0;
    config.validate().expect("a decay-only posture is valid");
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

#[test]
fn the_completer_is_off_by_default_and_validates_clean() {
    let config = Config::default();
    assert!(!config.completer.enabled, "chat/completion is opt-in");
    assert_eq!(config.completer.provider, "openai_chat");
    // An off completer is not validated, so the empty default model is fine.
    config
        .validate()
        .expect("a default (disabled) completer validates");
}

#[test]
fn an_enabled_completer_requires_a_model() {
    let mut config = Config::default();
    config.completer.enabled = true;
    config.completer.endpoint = "https://api.openai.com/v1".to_owned();
    config.completer.model = String::new();
    assert!(
        matches!(config.validate(), Err(ConfigError::Missing(key)) if key == "completer.model"),
        "an enabled completer with no model is rejected"
    );
}

#[test]
fn an_enabled_completer_rejects_a_remote_plaintext_endpoint() {
    let mut config = Config::default();
    config.completer.enabled = true;
    config.completer.model = "gpt-4o".to_owned();
    config.completer.endpoint = "http://api.example.com/v1".to_owned();
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "completer.endpoint"),
        "a remote http completer endpoint is rejected"
    );
}

#[test]
fn the_completer_api_key_resolves_from_its_own_variable() {
    let mut config = Config::default();
    config.completer.api_key_env = Some("AIONFORGE_CHAT_KEY".to_owned());
    let secret = config
        .resolve_completer_api_key(|name| (name == "AIONFORGE_CHAT_KEY").then(|| "k".to_owned()))
        .expect("resolve")
        .expect("a key is present");
    assert_eq!(secret.expose_secret(), "k");
    // The embedder and completer keys are independent.
    assert!(
        config
            .resolve_api_key(|_| None)
            .expect("embedder resolve")
            .is_none()
    );
}
