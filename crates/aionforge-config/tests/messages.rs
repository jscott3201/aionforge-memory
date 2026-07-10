//! Acceptance tests for layered `[messages]` retention and long-poll configuration.

// figment::Jail's closure returns Result<(), figment::Error>, whose Err variant is
// large; that is the harness's type, not ours, so allow it in this test file.
#![allow(clippy::result_large_err)]

use std::path::Path;

use aionforge_config::{Config, ConfigError};
use figment::Jail;
use figment::providers::{Format, Toml};

/// Enter a config-loading jail with no inherited `AIONFORGE_*` variables.
fn clear_inherited_env(jail: &mut Jail) {
    let home = std::env::var_os("HOME");
    jail.clear_env();
    if let Some(home) = home {
        jail.set_env("HOME", home.to_string_lossy());
    }
}

#[test]
fn defaults_enable_ack_aware_retention_and_bound_waits() {
    let config = Config::default();
    assert!(config.messages.retention_enabled);
    assert_eq!(config.messages.retention_acked_days, 30);
    assert_eq!(config.messages.retention_unacked_days, 90);
    assert_eq!(config.messages.wait_default_seconds, 25);
    assert_eq!(config.messages.wait_max_seconds, 55);
    assert_eq!(config.messages.wait_max_concurrent, 256);
    assert_eq!(config.messages.wait_max_recipients, 256);
    config.validate().expect("defaults validate");
}

#[test]
fn message_runtime_layers_apply_in_precedence_order() {
    Jail::expect_with(|jail| {
        clear_inherited_env(jail);
        // File values override the compiled defaults.
        jail.create_file(
            "config.toml",
            "[messages]\nretention_enabled = false\nretention_acked_days = 14\n\
             retention_unacked_days = 45\nwait_default_seconds = 12\n\
             wait_max_seconds = 40\nwait_max_concurrent = 64\nwait_max_recipients = 80\n",
        )?;
        // Nested environment values override individual file fields.
        jail.set_env("AIONFORGE_MESSAGES__RETENTION_ENABLED", "true");
        jail.set_env("AIONFORGE_MESSAGES__RETENTION_ACKED_DAYS", "21");
        jail.set_env("AIONFORGE_MESSAGES__WAIT_DEFAULT_SECONDS", "18");
        jail.set_env("AIONFORGE_MESSAGES__WAIT_MAX_SECONDS", "50");
        jail.set_env("AIONFORGE_MESSAGES__WAIT_MAX_CONCURRENT", "96");
        jail.set_env("AIONFORGE_MESSAGES__WAIT_MAX_RECIPIENTS", "128");

        let base = Config::figment(Path::new("config.toml"));
        let from_env = Config::from_figment(base.clone()).expect("load file + env");
        assert!(from_env.messages.retention_enabled, "env beats file");
        assert_eq!(from_env.messages.retention_acked_days, 21, "env beats file");
        assert_eq!(
            from_env.messages.retention_unacked_days, 45,
            "file beats default"
        );
        assert_eq!(from_env.messages.wait_default_seconds, 18, "env beats file");
        assert_eq!(from_env.messages.wait_max_seconds, 50, "env beats file");
        assert_eq!(from_env.messages.wait_max_concurrent, 96, "env beats file");
        assert_eq!(from_env.messages.wait_max_recipients, 128, "env beats file");

        // A caller-provided flags layer is merged last, exactly like the production host seam.
        let with_flags = base.merge(Toml::string(
            "[messages]\nretention_enabled = false\nretention_acked_days = 3\n\
             retention_unacked_days = 10\nwait_default_seconds = 4\n\
             wait_max_seconds = 8\nwait_max_concurrent = 2\nwait_max_recipients = 3\n",
        ));
        let from_flags = Config::from_figment(with_flags).expect("load with flags");
        assert!(!from_flags.messages.retention_enabled, "flags beat env");
        assert_eq!(
            from_flags.messages.retention_acked_days, 3,
            "flags beat env"
        );
        assert_eq!(
            from_flags.messages.retention_unacked_days, 10,
            "flags beat file"
        );
        assert_eq!(
            from_flags.messages.wait_default_seconds, 4,
            "flags beat env"
        );
        assert_eq!(from_flags.messages.wait_max_seconds, 8, "flags beat env");
        assert_eq!(from_flags.messages.wait_max_concurrent, 2, "flags beat env");
        assert_eq!(from_flags.messages.wait_max_recipients, 3, "flags beat env");
        Ok(())
    });
}

#[test]
fn invalid_message_wait_bounds_fail_config_validation_with_the_exact_key() {
    fn assert_invalid(key: &str, configure: impl FnOnce(&mut Config)) {
        let mut config = Config::default();
        configure(&mut config);
        assert!(
            matches!(config.validate(), Err(ConfigError::Invalid { key: actual, .. }) if actual == key),
            "invalid wait bound must name {key}",
        );
    }
    assert_invalid("messages.wait_max_seconds", |config| {
        config.messages.wait_max_seconds = 0;
    });
    assert_invalid("messages.wait_default_seconds", |config| {
        config.messages.wait_default_seconds = 0;
    });
    assert_invalid("messages.wait_max_concurrent", |config| {
        config.messages.wait_max_concurrent = 0;
    });
    assert_invalid("messages.wait_max_recipients", |config| {
        config.messages.wait_max_recipients = 0;
    });
    assert_invalid("messages.wait_default_seconds", |config| {
        config.messages.wait_default_seconds = config.messages.wait_max_seconds + 1;
    });
}
