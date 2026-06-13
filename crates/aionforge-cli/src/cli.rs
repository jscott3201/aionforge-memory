//! Command-line argument parsing for the `aionforge` binary.

use std::net::SocketAddr;
use std::path::PathBuf;

use aionforge_config::default_config_path;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::host::HostOptions;

#[derive(Debug, Parser)]
#[command(
    name = "aionforge",
    version,
    about = "Operate an Aionforge Memory store"
)]
pub(crate) struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Path to the layered TOML configuration file"
    )]
    config: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Override persistence.data_dir after file and environment layers"
    )]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Command,
}

impl Cli {
    pub(crate) fn host_options(&self) -> HostOptions {
        HostOptions {
            config_path: self.config.clone().unwrap_or_else(default_config_path),
            data_dir: self.data_dir.clone(),
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Inspect schema, indexes, providers, lag, capacity, and embedder binding.
    Doctor(DoctorArgs),
    /// Recover an existing WAL-backed store and report post-replay health.
    Recover(RecoverArgs),
    /// Serve the Aionforge Memory MCP surface over stdio or Streamable HTTP.
    Serve(ServeArgs),
}

#[derive(Debug, Args, Clone, Copy)]
pub(crate) struct DoctorArgs {
    /// Emit the complete typed doctor report as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args, Clone, Copy)]
pub(crate) struct RecoverArgs {
    /// Emit the complete typed recovery report as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args, Clone)]
pub(crate) struct ServeArgs {
    /// Transport to serve.
    #[arg(value_enum, default_value_t = ServeTransport::Stdio)]
    pub(crate) transport: ServeTransport,
    /// Address for Streamable HTTP. Ignored for stdio. Omitted inherits `server.listen`
    /// from config (loopback `127.0.0.1:3918` by default).
    #[arg(long)]
    pub(crate) listen: Option<SocketAddr>,
    /// Allowed HTTP Host header. Repeat to override the loopback defaults. Empty inherits
    /// `server.allowed_hosts` from config.
    #[arg(long = "allowed-host", value_name = "HOST")]
    pub(crate) allowed_hosts: Vec<String>,
    /// Allowed browser Origin. Repeat to override the loopback defaults. Empty inherits
    /// `server.allowed_origins` from config.
    #[arg(long = "allowed-origin", value_name = "ORIGIN")]
    pub(crate) allowed_origins: Vec<String>,
    /// Streamable HTTP session posture override, as a mutually exclusive `--stateless` /
    /// `--stateful` pair. Bare `--stateless` disables sessions; `--stateful` forces them on;
    /// omitting both inherits `server.stateful` from config. Modeled as two `SetTrue`
    /// switches (rather than `--stateless[=<bool>]`) so neither flag ever consumes a
    /// following token, keeping flag-before-positional ordering (`serve --stateless http`)
    /// working.
    #[command(flatten)]
    pub(crate) session: SessionPostureArgs,
    /// Prefer JSON responses for stateless Streamable HTTP calls.
    #[arg(long)]
    pub(crate) json_response: bool,
    /// Maximum accepted HTTP request body bytes.
    #[arg(long, value_name = "BYTES")]
    pub(crate) max_request_body_bytes: Option<usize>,
}

/// The mutually exclusive `--stateless` / `--stateful` session-posture override.
///
/// Two `SetTrue` switches in a `multiple = false` group: clap rejects supplying both, and
/// neither flag consumes a following positional token, so `serve --stateless http` parses
/// (transport `http`, posture stateless) rather than treating `http` as a flag value.
#[derive(Debug, Args, Clone, Copy)]
#[group(multiple = false)]
pub(crate) struct SessionPostureArgs {
    /// Disable stateful Streamable HTTP sessions.
    #[arg(long, action = ArgAction::SetTrue)]
    stateless: bool,
    /// Force stateful Streamable HTTP sessions on, overriding the config default.
    #[arg(long, action = ArgAction::SetTrue)]
    stateful: bool,
}

impl SessionPostureArgs {
    /// Construct the posture switches from the folded tri-state the host consumes: `None`
    /// leaves both off (inherit config), `Some(true)` sets `--stateless`, `Some(false)` sets
    /// `--stateful`. The dual of [`Self::stateless`]; used to build `ServeArgs` in tests.
    #[cfg(test)]
    pub(crate) fn from_stateless(stateless: Option<bool>) -> Self {
        Self {
            stateless: stateless == Some(true),
            stateful: stateless == Some(false),
        }
    }

    /// Fold the two switches into the tri-state override the host consumes: `Some(true)`
    /// for `--stateless`, `Some(false)` for `--stateful`, and `None` (inherit config) when
    /// neither is given. The `multiple = false` group guarantees they are never both set.
    pub(crate) fn stateless(self) -> Option<bool> {
        match (self.stateless, self.stateful) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            (false, false) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ServeTransport {
    /// MCP over the process stdio stream.
    Stdio,
    /// MCP Streamable HTTP over TCP.
    Http,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_global_paths_and_doctor_json() {
        let cli = Cli::try_parse_from([
            "aionforge",
            "--config",
            "/tmp/aionforge.toml",
            "--data-dir",
            "/tmp/aionforge-data",
            "doctor",
            "--json",
        ])
        .expect("parse");

        assert_eq!(
            cli.host_options().config_path,
            PathBuf::from("/tmp/aionforge.toml")
        );
        assert_eq!(
            cli.host_options().data_dir,
            Some(PathBuf::from("/tmp/aionforge-data"))
        );
        match cli.command {
            Command::Doctor(args) => assert!(args.json),
            Command::Recover(_) => panic!("expected doctor command"),
            Command::Serve(_) => panic!("expected doctor command"),
        }
    }

    #[test]
    fn defaults_config_path_when_unspecified() {
        let cli = Cli::try_parse_from(["aionforge", "doctor"]).expect("parse");

        assert_eq!(cli.host_options().config_path, default_config_path());
        assert!(cli.host_options().data_dir.is_none());
    }

    #[test]
    fn parses_recover_json() {
        let cli = Cli::try_parse_from(["aionforge", "recover", "--json"]).expect("parse");

        match cli.command {
            Command::Recover(args) => assert!(args.json),
            Command::Doctor(_) | Command::Serve(_) => panic!("expected recover command"),
        }
    }

    #[test]
    fn parses_serve_http_options() {
        let cli = Cli::try_parse_from([
            "aionforge",
            "serve",
            "http",
            "--listen",
            "127.0.0.1:4927",
            "--allowed-host",
            "localhost",
            "--allowed-origin",
            "http://localhost:3000",
            "--stateless",
            "--json-response",
            "--max-request-body-bytes",
            "4096",
        ])
        .expect("parse");

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(args.transport, ServeTransport::Http);
        assert_eq!(
            args.listen,
            Some("127.0.0.1:4927".parse::<SocketAddr>().expect("addr"))
        );
        assert_eq!(args.allowed_hosts, vec!["localhost"]);
        assert_eq!(args.allowed_origins, vec!["http://localhost:3000"]);
        assert_eq!(args.session.stateless(), Some(true));
        assert!(args.json_response);
        assert_eq!(args.max_request_body_bytes, Some(4096));
    }

    #[test]
    fn serve_http_without_flags_inherits_from_config() {
        // The promoted knobs are absent: `listen` and `stateless` are `None` and the
        // allow-lists are empty, the signal the host reads as "inherit the [server]
        // config block" rather than an explicit override.
        let cli = Cli::try_parse_from(["aionforge", "serve", "http"]).expect("parse");

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(args.transport, ServeTransport::Http);
        assert_eq!(args.listen, None, "no --listen inherits config");
        assert_eq!(
            args.session.stateless(),
            None,
            "no --stateless inherits config"
        );
        assert!(
            args.allowed_hosts.is_empty(),
            "no --allowed-host inherits config"
        );
        assert!(
            args.allowed_origins.is_empty(),
            "no --allowed-origin inherits config"
        );
    }

    /// Regression: a bare `--stateless` BEFORE the positional `transport` must still parse
    /// (transport `http`, posture stateless). The prior `num_args = 0..=1` tri-state made
    /// clap greedily read `http` as the flag value and error out; the `SetTrue` switch pair
    /// never consumes a following token.
    #[test]
    fn serve_stateless_before_positional_transport_parses() {
        let cli = Cli::try_parse_from(["aionforge", "serve", "--stateless", "http"])
            .expect("--stateless before the positional transport parses");

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(args.transport, ServeTransport::Http);
        assert_eq!(
            args.session.stateless(),
            Some(true),
            "bare --stateless still means stateless"
        );
    }

    /// `--stateful` forces sessions on, the explicit replacement for the dropped
    /// `--stateless=false` value form.
    #[test]
    fn serve_stateful_flag_forces_sessions_on() {
        let cli = Cli::try_parse_from(["aionforge", "serve", "http", "--stateful"]).expect("parse");

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(args.session.stateless(), Some(false));
    }

    /// The `multiple = false` group rejects supplying both posture switches at once.
    #[test]
    fn serve_rejects_both_stateless_and_stateful() {
        let result =
            Cli::try_parse_from(["aionforge", "serve", "http", "--stateless", "--stateful"]);
        assert!(
            result.is_err(),
            "--stateless and --stateful are mutually exclusive"
        );
    }
}
