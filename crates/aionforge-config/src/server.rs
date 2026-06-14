//! Streamable HTTP server-knob configuration (fork#6, PR1.5).
//!
//! Its own module because the serve-HTTP transport posture is a coherent unit: the bind
//! address, the Host/Origin allow-lists the browser-facing transport enforces, and
//! whether sessions are stateful. Promoting these knobs from CLI-flag-only into the
//! layered [`Config`](crate::Config) is what lets a console deployment carry its own
//! `[server]` block; the CLI flags remain the highest-precedence override, merged on top
//! of this block by the host.
//!
//! There is **no master switch**: an HTTP server always binds *somewhere*, so the
//! all-default posture (loopback `127.0.0.1:3918`, stateful sessions, no explicit
//! allow-lists) is exactly today's behavior, not an off state. An empty allow-list is
//! meaningful — it defers to the transport library's built-in loopback defaults — so the
//! only thing [`ServerHttpConfig::validate`] rejects is a *blank* (whitespace-only) entry,
//! which can never be a real host or origin and would otherwise silently widen or narrow
//! the allow-list.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The default Streamable HTTP bind address: loopback on port 3918. Kept as the single
/// source of truth for the [`ServerHttpConfig::listen`] default so a no-flag, no-config
/// invocation binds exactly where it does today.
const DEFAULT_LISTEN: &str = "127.0.0.1:3918";

/// Streamable HTTP server posture: the bind address, the Host/Origin allow-lists, and the
/// stateful-session flag (fork#6, PR1.5).
///
/// Promoted into the layered config so a deployment can carry its own `[server]` block;
/// the CLI flags override these fields field-for-field when present. The all-default
/// posture reproduces today's flag-free behavior exactly: bind loopback `127.0.0.1:3918`,
/// run stateful sessions, and leave the allow-lists empty so the transport applies its own
/// loopback defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerHttpConfig {
    /// The TCP address the Streamable HTTP transport binds. Defaults to loopback
    /// `127.0.0.1:3918`. A typed [`SocketAddr`], so it is always structurally valid and
    /// needs no validation.
    pub listen: SocketAddr,
    /// The accepted HTTP `Host` headers. Empty by default, which defers to the transport's
    /// built-in loopback defaults; a non-empty list replaces them wholesale. No entry may
    /// be blank (see [`ServerHttpConfig::validate`]).
    pub allowed_hosts: Vec<String>,
    /// The accepted browser `Origin` values. Empty by default, which defers to the
    /// transport's built-in loopback defaults; a non-empty list replaces them wholesale. No
    /// entry may be blank (see [`ServerHttpConfig::validate`]).
    pub allowed_origins: Vec<String>,
    /// Whether Streamable HTTP sessions are stateful. `true` by default (stateful sessions,
    /// today's behavior); a stateless deployment sets `false`.
    pub stateful: bool,
}

impl Default for ServerHttpConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN
                .parse()
                .expect("the default listen address is a valid socket address"),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            stateful: true,
        }
    }
}

impl ServerHttpConfig {
    /// Validate the server posture. The address is a typed [`SocketAddr`] and always
    /// structurally valid; the allow-lists are checked only for *blank* (trimmed-empty)
    /// entries, which can never be a real host or origin. An empty list is valid and
    /// defers to the transport's loopback defaults.
    ///
    /// # Errors
    /// Returns [`ConfigError`] naming the offending key (`server.allowed_hosts` or
    /// `server.allowed_origins`), never quoting a value.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.allowed_hosts.iter().any(|host| host.trim().is_empty()) {
            return Err(ConfigError::invalid(
                "server.allowed_hosts",
                "must not contain a blank host",
            ));
        }
        if self
            .allowed_origins
            .iter()
            .any(|origin| origin.trim().is_empty())
        {
            return Err(ConfigError::invalid(
                "server.allowed_origins",
                "must not contain a blank origin",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_posture_is_todays_behavior_and_validates() {
        let config = ServerHttpConfig::default();
        assert_eq!(
            config.listen,
            "127.0.0.1:3918".parse::<SocketAddr>().expect("addr"),
            "the default binds loopback port 3918"
        );
        assert!(config.stateful, "sessions are stateful by default");
        assert!(
            config.allowed_hosts.is_empty(),
            "no explicit host allow-list by default"
        );
        assert!(
            config.allowed_origins.is_empty(),
            "no explicit origin allow-list by default"
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_blank_host_entry_is_rejected_and_names_the_key() {
        // A whitespace-only host can never match a real `Host` header, so it is a
        // configuration error rather than a silently-ignored entry.
        let config = ServerHttpConfig {
            allowed_hosts: vec!["localhost".into(), "   ".into()],
            ..ServerHttpConfig::default()
        };
        let err = config
            .validate()
            .expect_err("a blank host entry is rejected");
        assert!(
            err.to_string().contains("server.allowed_hosts"),
            "the error names the offending key: {err}"
        );
    }

    #[test]
    fn a_blank_origin_entry_is_rejected_and_names_the_key() {
        let config = ServerHttpConfig {
            allowed_origins: vec!["http://localhost:3000".into(), "".into()],
            ..ServerHttpConfig::default()
        };
        let err = config
            .validate()
            .expect_err("a blank origin entry is rejected");
        assert!(
            err.to_string().contains("server.allowed_origins"),
            "the error names the offending key: {err}"
        );
    }

    #[test]
    fn the_posture_round_trips_through_json() {
        let config = ServerHttpConfig {
            listen: "0.0.0.0:8080".parse().expect("addr"),
            allowed_hosts: vec!["console.example".into()],
            allowed_origins: vec!["https://console.example".into()],
            stateful: false,
        };

        // The file/env layers ride these same serde impls (figment), so the round trip
        // pins the wire shape: the socket address renders as its string form.
        let json = serde_json::to_string(&config).expect("serialize");
        let back: ServerHttpConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, config);
    }
}
