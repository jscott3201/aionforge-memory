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

use figment::providers::{Format, Serialized, Toml};
use figment::{Figment, Jail};
use secrecy::ExposeSecret;

/// Enter a config-loading jail with no inherited `AIONFORGE_*` variables.
///
/// The layered loader reads `Env::prefixed("AIONFORGE_")` (`load.rs`) from the live process
/// environment, and figment's [`Jail`] restores env *writes* on drop but does not clear
/// inherited variables at entry — so a developer machine that exports `AIONFORGE_EMBEDDER__*`
/// (or any `AIONFORGE_*`) leaks into these layering assertions, which `Config::from_figment`
/// extracts and validates as one whole config.
///
/// `set_env` cannot neutralise a variable a test does not name (e.g. an ambient
/// `AIONFORGE_EMBEDDER__MODEL` that would beat the file layer), so the inherited ones must be
/// *removed*. figment exposes only the whole-environment `clear_env` — there is no namespaced
/// removal, and `unsafe { remove_var }` is forbidden workspace-wide — so we clear everything
/// and immediately restore `HOME`, the one inherited variable the many non-jail tests in this
/// binary read concurrently (via `Config::default` → `default_data_dir`). [`Jail`] serializes
/// jail-vs-jail tests under a global lock and restores the full environment on drop, so this
/// is race-free against other jails and self-undoing; the brief window before `HOME` is
/// restored is logically harmless (a cleared `HOME` only yields a non-empty `./.aionforge`,
/// which `validate` accepts and no assertion inspects).
fn clear_inherited_env(jail: &mut Jail) {
    let home = std::env::var_os("HOME");
    jail.clear_env();
    if let Some(home) = home {
        jail.set_env("HOME", home.to_string_lossy());
    }
}

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
        clear_inherited_env(jail);
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
        clear_inherited_env(jail);
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
        clear_inherited_env(jail);
        // No config.toml created; loading falls back to defaults + env.
        let config = Config::from_figment(Config::figment(Path::new("config.toml")))
            .expect("load with no file");
        assert_eq!(config.embedder.dimension, 1536);
        Ok(())
    });
}

#[test]
fn load_reads_the_default_config_path() {
    Jail::expect_with(|jail| {
        clear_inherited_env(jail);
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
fn a_native_dimension_must_exceed_the_output_dimension() {
    // Matryoshka truncation only reduces, so a native dimension that is not strictly larger
    // than the output dimension is a misconfiguration.
    let mut config = Config::default();
    config.embedder.dimension = 1536;
    config.embedder.native_dimension = Some(1536);
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "embedder.native_dimension"),
        "a native dimension equal to the output is rejected",
    );

    config.embedder.native_dimension = Some(3072);
    config
        .validate()
        .expect("a native dimension above the output is allowed");
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
fn a_min_relevance_outside_the_unit_interval_is_rejected() {
    // The absolute relevance floor is a fraction of the dense cosine similarity in [0, 1]; an
    // out-of-range value is almost certainly a typo (e.g. 50 meaning 50%) that would silently
    // empty every recall, so it must fail loudly rather than degrade recall in production.
    let mut config = Config::default();
    config.retrieval.min_relevance = 1.5;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "retrieval.min_relevance"),
        "a floor above 1.0 is rejected"
    );

    let mut config = Config::default();
    config.retrieval.min_relevance = -0.1;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "retrieval.min_relevance"),
        "a negative floor is rejected"
    );

    // A NaN is rejected too — `!(0.0..=1.0).contains(&NaN)` is true, so the `!(…)` guard catches
    // it (every ordered comparison against NaN is false). The validate() comment claims this.
    let mut config = Config::default();
    config.retrieval.min_relevance = f64::NAN;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "retrieval.min_relevance"),
        "a NaN floor is rejected"
    );

    // The unit-interval endpoints are both valid, and the default is 0.0 (the floor is off).
    for value in [0.0, 1.0] {
        let mut config = Config::default();
        config.retrieval.min_relevance = value;
        config.validate().expect("an in-range floor validates");
    }
    assert_eq!(
        Config::default().retrieval.min_relevance,
        0.0,
        "the default floor is off"
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
fn the_clock_skew_tolerance_is_bounded_unconditionally() {
    // The always-on core-block edit gate consumes the window regardless of the
    // signed-writes switch (05 §4), so a zero tolerance is a config error even with
    // every optional subsystem off — it would silently refuse every identity edit.
    let mut config = Config::default();
    config.security.signed_writes = false;
    config.security.clock_skew_tolerance_ms = 0;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "security.clock_skew_tolerance_ms"),
        "a zero tolerance is rejected even with signed writes off"
    );

    // Above the five-minute ceiling is rejected.
    let mut config = Config::default();
    config.security.signed_writes = true;
    config.security.clock_skew_tolerance_ms = 300_001;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "security.clock_skew_tolerance_ms"),
        "a tolerance above the ceiling is rejected"
    );

    // The ceiling itself is allowed.
    let mut config = Config::default();
    config.security.signed_writes = true;
    config.security.clock_skew_tolerance_ms = 300_000;
    config.validate().expect("the ceiling itself is allowed");
}

#[test]
fn the_core_block_posture_parses_from_toml_and_validates_through_config() {
    let reviewer = "0197b0aa-3c5e-8000-8000-000000000000";
    let figment =
        Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(&format!(
            r#"
                [core_block]
                redline_requires_human = true
                human_attester_ids = ["{reviewer}"]

                [core_block.default_rule]
                k = 1

                [core_block.rules.pii]
                k = 2
                require_human = true
            "#
        )));
    let config: Config = figment.extract().expect("extract");
    config.validate().expect("a sound posture validates");
    assert!(config.core_block.redline_requires_human);
    assert_eq!(config.core_block.rules["pii"].k, 2);
    assert_eq!(
        config
            .core_block
            .human_attester_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![reviewer.to_string()],
        "uuid strings land as typed ids"
    );

    // The whole-config validate runs the core-block rules: a zero k fails closed.
    let mut config = config;
    config.core_block.default_rule.k = 0;
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "core_block.default_rule.k"),
        "the offending key is named"
    );
}

#[test]
fn the_server_block_posture_parses_from_toml_and_validates_through_config() {
    // TOML — not JSON — is the real figment wire format, and `server.listen` is a typed
    // `SocketAddr` that deserializes from a bare string. Pin that the [server] block round
    // trips through a full Config load (the path PR1's JSON-only unit test does not cover)
    // and validates.
    let figment = Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(
        r#"
            [server]
            listen = "0.0.0.0:8080"
            allowed_hosts = ["console.example"]
            allowed_origins = ["https://console.example"]
            stateful = false
        "#,
    ));
    let config: Config = figment.extract().expect("extract");
    config.validate().expect("a sound server posture validates");
    assert_eq!(
        config.server.listen.to_string(),
        "0.0.0.0:8080",
        "the listen SocketAddr parses from its TOML string form"
    );
    assert!(
        !config.server.stateful,
        "stateful = false parses through TOML"
    );
    assert_eq!(
        config.server.allowed_hosts,
        vec!["console.example".to_string()]
    );
    assert_eq!(
        config.server.allowed_origins,
        vec!["https://console.example".to_string()]
    );

    // The whole-config validate runs the server block: a blank origin fails closed,
    // naming the key (never the value).
    let mut config = config;
    config.server.allowed_origins = vec![String::new()];
    assert!(
        matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == "server.allowed_origins"),
        "the offending key is named"
    );
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
fn decay_half_lives_are_validated_only_when_enabled() {
    // Off by default: a zero half-life is inert and never validated.
    let mut config = Config::default();
    config.decay.episodic_half_life_secs = 0;
    config
        .validate()
        .expect("an off decay policy ignores its half-lives");

    // On with the defaults validates: 7 days episodic, 365 days semantic.
    let mut config = Config::default();
    config.decay.enabled = true;
    config
        .validate()
        .expect("the default half-lives are valid when enabled");

    // On: a zero half-life would silently never decay (the pure function treats it as
    // inert), so it is rejected as a visible misconfiguration — per tier.
    for tier_key in [
        "decay.episodic_half_life_secs",
        "decay.semantic_half_life_secs",
    ] {
        let mut config = Config::default();
        config.decay.enabled = true;
        match tier_key {
            "decay.episodic_half_life_secs" => config.decay.episodic_half_life_secs = 0,
            _ => config.decay.semantic_half_life_secs = 0,
        }
        assert!(
            matches!(config.validate(), Err(ConfigError::Invalid { key, .. }) if key == tier_key),
            "a zero {tier_key} is rejected"
        );
    }
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
        clear_inherited_env(jail);
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
        clear_inherited_env(jail);
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
fn audit_signing_is_off_by_default() {
    let config = Config::default();
    assert!(
        !config.security.sign_audit_events,
        "audit signing is opt-in (doing nothing leaves events unsigned)"
    );
    assert!(
        config.security.audit_key_env.is_none(),
        "self-custody is the default; no env var is named"
    );
    assert!(
        config.security.trusted_audit_keys.is_empty(),
        "the reserved federation pin list is empty"
    );
    config
        .validate()
        .expect("the default (off) audit-signing posture validates");
}

#[test]
fn the_environment_enables_audit_signing() {
    Jail::expect_with(|jail| {
        clear_inherited_env(jail);
        jail.set_env("AIONFORGE_SECURITY__SIGN_AUDIT_EVENTS", "true");
        let config =
            Config::from_figment(Config::figment(Path::new("config.toml"))).expect("load from env");
        assert!(
            config.security.sign_audit_events,
            "the env layer turns audit signing on"
        );
        Ok(())
    });
}

#[test]
fn the_audit_seed_resolves_from_its_own_variable() {
    let mut config = Config::default();
    config.security.audit_key_env = Some("AIONFORGE_AUDIT_SEED".to_owned());
    // The value is an opaque base64 seed to the config layer; it is not decoded here.
    let secret = config
        .resolve_audit_seed(|name| {
            (name == "AIONFORGE_AUDIT_SEED").then(|| "c2VlZC1ieXRlcw==".to_owned())
        })
        .expect("resolve")
        .expect("a seed is present");
    assert_eq!(secret.expose_secret(), "c2VlZC1ieXRlcw==");
    let secret_debug = format!("{secret:?}");
    assert!(
        !secret_debug.contains("c2VlZC1ieXRlcw=="),
        "the seed redacts in its Debug output: {secret_debug}"
    );
    // The audit seed is independent of the embedder API key.
    assert!(
        config
            .resolve_api_key(|_| None)
            .expect("embedder resolve")
            .is_none()
    );
}

#[test]
fn a_named_but_unset_audit_seed_variable_fails_clearly() {
    let mut config = Config::default();
    config.security.audit_key_env = Some("AIONFORGE_MISSING_SEED".to_owned());
    let error = config
        .resolve_audit_seed(|_| None)
        .expect_err("an unset named seed variable is an error");
    match error {
        ConfigError::SecretEnvMissing(name) => assert_eq!(name, "AIONFORGE_MISSING_SEED"),
        other => panic!("wrong error: {other}"),
    }
}

#[test]
fn no_audit_seed_variable_resolves_to_none() {
    let config = Config::default();
    assert!(config.security.audit_key_env.is_none());
    assert!(
        config
            .resolve_audit_seed(|_| panic!("lookup must not run when no variable is named"))
            .expect("resolve")
            .is_none()
    );
}
