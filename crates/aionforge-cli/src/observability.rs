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

/// Install the process-global tracing subscriber, rendering `format` to stderr.
///
/// Level/target filtering comes from the `RUST_LOG` environment variable via
/// [`tracing_subscriber::EnvFilter`]; absent or unparseable, it defaults to `info`.
///
/// Idempotent and infallible by contract: installation can only fail if a global
/// subscriber is already set (e.g. a second call), which is treated as a no-op. Logging
/// setup must never be the reason a server fails to start.
pub(crate) fn init(format: LogFormat) {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_DIRECTIVES));

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
const DEFAULT_DIRECTIVES: &str = "info";
