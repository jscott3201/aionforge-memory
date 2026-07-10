//! Acceptance tests for the layered `[messages]` retention configuration.

// figment::Jail's closure returns Result<(), figment::Error>, whose Err variant is
// large; that is the harness's type, not ours, so allow it in this test file.
#![allow(clippy::result_large_err)]

use std::path::Path;

use aionforge_config::Config;
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
fn defaults_enable_ack_aware_retention() {
    let config = Config::default();
    assert!(config.messages.retention_enabled);
    assert_eq!(config.messages.retention_acked_days, 30);
    assert_eq!(config.messages.retention_unacked_days, 90);
    config.validate().expect("defaults validate");
}

#[test]
fn message_retention_layers_apply_in_precedence_order() {
    Jail::expect_with(|jail| {
        clear_inherited_env(jail);
        // File values override the compiled defaults.
        jail.create_file(
            "config.toml",
            "[messages]\nretention_enabled = false\nretention_acked_days = 14\n\
             retention_unacked_days = 45\n",
        )?;
        // Nested environment values override individual file fields.
        jail.set_env("AIONFORGE_MESSAGES__RETENTION_ENABLED", "true");
        jail.set_env("AIONFORGE_MESSAGES__RETENTION_ACKED_DAYS", "21");

        let base = Config::figment(Path::new("config.toml"));
        let from_env = Config::from_figment(base.clone()).expect("load file + env");
        assert!(from_env.messages.retention_enabled, "env beats file");
        assert_eq!(from_env.messages.retention_acked_days, 21, "env beats file");
        assert_eq!(
            from_env.messages.retention_unacked_days, 45,
            "file beats default"
        );

        // A caller-provided flags layer is merged last, exactly like the production host seam.
        let with_flags = base.merge(Toml::string(
            "[messages]\nretention_enabled = false\nretention_acked_days = 3\n\
             retention_unacked_days = 10\n",
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
        Ok(())
    });
}
