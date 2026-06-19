//! Global tracing-subscriber installation for the `aionforge` binary (logging-foundation,
//! task #9 PR1).
//!
//! Every library crate instruments with the `tracing` facade and leaves the sink to the
//! binary; this is that sink. It is installed once, synchronously, at the top of `main`
//! before any subcommand dispatches — `doctor` and `recover` run synchronously and emit
//! `tracing` events during store operations, and the tokio runtime only exists for
//! `serve`, so a later (or async) install would miss those paths.
//!
//! # Invariant: stderr only
//! The subscriber MUST write to **stderr**. The MCP stdio transport carries the JSON-RPC
//! protocol on **stdout**, so a formatter writing there would corrupt the protocol stream
//! and break every connected client. `tracing_subscriber::fmt` defaults to stdout, hence
//! the explicit `.with_writer(std::io::stderr)` below — do not remove it.

use crate::cli::LogFormat;
use tracing_subscriber::EnvFilter;

/// Install the process-global tracing subscriber, rendering `format` to stderr.
///
/// Level/target filtering comes from the `RUST_LOG` environment variable via
/// [`tracing_subscriber::EnvFilter`]; absent or unparseable, it defaults to `info`.
///
/// Idempotent and infallible by contract: installation can only fail if a global
/// subscriber is already set (e.g. a second call), which is treated as a no-op. Logging
/// setup must never be the reason a server fails to start.
pub(crate) fn init(format: LogFormat) {
    let rust_log = std::env::var("RUST_LOG").ok();
    let filter = build_filter(rust_log.as_deref());

    // Build the common fmt subscriber, pinned to stderr (never stdout — see module docs).
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);

    // `try_init` returns Err only when a global subscriber is already installed; ignore it
    // so a redundant call (or a test that already initialized) is a harmless no-op.
    let _ = match format {
        LogFormat::Json => builder.json().try_init(),
        LogFormat::Text => builder.try_init(),
    };
}

/// The default `EnvFilter` directives when `RUST_LOG` is unset: informative for the
/// application, but quiet for chatty dependencies that would otherwise drown the signal.
const RMCP_TOWER_TARGET: &str = "rmcp::transport::streamable_http_server::tower";
const DEFAULT_DIRECTIVES: &str = "info,rmcp::transport::streamable_http_server::tower=warn";
const RMCP_TOWER_WARN_DIRECTIVE: &str = "rmcp::transport::streamable_http_server::tower=warn";

/// Build the process filter while preserving the rmcp streamable-HTTP tower WARN floor.
///
/// Upstream rmcp currently logs a benign close-session path as ERROR on normal disconnect. We keep
/// that module at WARN by default and reapply the directive over `RUST_LOG`, because parsing
/// `RUST_LOG=info` replaces the defaults wholesale. The floor is WARN rather than OFF so genuine
/// host/origin rejection warnings from the same module still surface; a real serve-failure ERROR
/// in that target is also demoted to WARN by this module-level filter.
fn build_filter(env: Option<&str>) -> EnvFilter {
    let (filter, operator_rmcp_override) = match env {
        Some(value) => match EnvFilter::try_new(value) {
            Ok(filter) => (filter, exact_rmcp_tower_directive(value)),
            Err(_) => (EnvFilter::new(DEFAULT_DIRECTIVES), None),
        },
        None => (EnvFilter::new(DEFAULT_DIRECTIVES), None),
    };

    let filter = filter.add_directive(
        RMCP_TOWER_WARN_DIRECTIVE
            .parse()
            .expect("rmcp tower directive is valid"),
    );

    if let Some(directive) = operator_rmcp_override {
        return filter.add_directive(
            directive
                .parse()
                .expect("RUST_LOG rmcp tower directive parsed earlier"),
        );
    }

    filter
}

fn exact_rmcp_tower_directive(env: &str) -> Option<&str> {
    env.split(',').map(str::trim).rev().find(|directive| {
        directive
            .strip_prefix(RMCP_TOWER_TARGET)
            .is_some_and(|suffix| suffix.starts_with('='))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::Level;
    use tracing_subscriber::{
        layer::{Context, Layer},
        prelude::*,
    };

    const SERVE_TARGET: &str = "aionforge::serve";

    #[derive(Clone, Default)]
    struct RecordingLayer {
        events: Arc<Mutex<Vec<(String, Level)>>>,
    }

    impl<S> Layer<S> for RecordingLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let metadata = event.metadata();
            self.events
                .lock()
                .expect("events lock")
                .push((metadata.target().to_string(), *metadata.level()));
        }
    }

    fn captured_levels(env: Option<&str>, target: &str) -> Vec<Level> {
        let layer = RecordingLayer::default();
        let events = Arc::clone(&layer.events);
        let subscriber = tracing_subscriber::registry()
            .with(build_filter(env))
            .with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::event!(target: RMCP_TOWER_TARGET, Level::DEBUG, "debug");
            tracing::event!(target: RMCP_TOWER_TARGET, Level::INFO, "info");
            tracing::event!(target: RMCP_TOWER_TARGET, Level::WARN, "warn");
            tracing::event!(target: RMCP_TOWER_TARGET, Level::ERROR, "error");

            tracing::event!(target: SERVE_TARGET, Level::DEBUG, "debug");
            tracing::event!(target: SERVE_TARGET, Level::INFO, "info");
            tracing::event!(target: SERVE_TARGET, Level::WARN, "warn");
            tracing::event!(target: SERVE_TARGET, Level::ERROR, "error");
        });

        events
            .lock()
            .expect("events lock")
            .iter()
            .filter_map(|(event_target, level)| (event_target == target).then_some(*level))
            .collect()
    }

    #[test]
    fn default_filter_keeps_rmcp_tower_at_warn() {
        assert_eq!(
            captured_levels(None, RMCP_TOWER_TARGET),
            vec![Level::WARN, Level::ERROR]
        );
        assert_eq!(
            captured_levels(None, SERVE_TARGET),
            vec![Level::INFO, Level::WARN, Level::ERROR]
        );
    }

    #[test]
    fn rust_log_info_keeps_rmcp_tower_at_warn() {
        assert_eq!(
            captured_levels(Some("info"), RMCP_TOWER_TARGET),
            vec![Level::WARN, Level::ERROR]
        );
    }

    #[test]
    fn exact_target_rust_log_override_can_raise_rmcp_tower() {
        assert_eq!(
            captured_levels(
                Some("rmcp::transport::streamable_http_server::tower=debug"),
                RMCP_TOWER_TARGET,
            ),
            vec![Level::DEBUG, Level::INFO, Level::WARN, Level::ERROR]
        );
    }
}
