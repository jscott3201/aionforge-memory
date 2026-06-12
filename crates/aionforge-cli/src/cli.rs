//! Command-line argument parsing for the `aionforge` binary.

use std::net::SocketAddr;
use std::path::PathBuf;

use aionforge_config::default_config_path;
use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Address for Streamable HTTP. Ignored for stdio.
    #[arg(long, default_value = "127.0.0.1:3918")]
    pub(crate) listen: SocketAddr,
    /// Allowed HTTP Host header. Repeat to override the loopback defaults.
    #[arg(long = "allowed-host", value_name = "HOST")]
    pub(crate) allowed_hosts: Vec<String>,
    /// Allowed browser Origin. Repeat to override the loopback defaults.
    #[arg(long = "allowed-origin", value_name = "ORIGIN")]
    pub(crate) allowed_origins: Vec<String>,
    /// Disable stateful Streamable HTTP sessions.
    #[arg(long)]
    pub(crate) stateless: bool,
    /// Prefer JSON responses for stateless Streamable HTTP calls.
    #[arg(long)]
    pub(crate) json_response: bool,
    /// Maximum accepted HTTP request body bytes.
    #[arg(long, value_name = "BYTES")]
    pub(crate) max_request_body_bytes: Option<usize>,
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
            "127.0.0.1:4927".parse::<SocketAddr>().expect("addr")
        );
        assert_eq!(args.allowed_hosts, vec!["localhost"]);
        assert_eq!(args.allowed_origins, vec!["http://localhost:3000"]);
        assert!(args.stateless);
        assert!(args.json_response);
        assert_eq!(args.max_request_body_bytes, Some(4096));
    }
}
