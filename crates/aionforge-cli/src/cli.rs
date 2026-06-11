//! Command-line argument parsing for the `aionforge` binary.

use std::path::PathBuf;

use aionforge_config::default_config_path;
use clap::{Args, Parser, Subcommand};

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
}

#[derive(Debug, Args, Clone, Copy)]
pub(crate) struct DoctorArgs {
    /// Emit the complete typed doctor report as JSON.
    #[arg(long)]
    pub(crate) json: bool,
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
        let Command::Doctor(args) = cli.command;
        assert!(args.json);
    }

    #[test]
    fn defaults_config_path_when_unspecified() {
        let cli = Cli::try_parse_from(["aionforge", "doctor"]).expect("parse");

        assert_eq!(cli.host_options().config_path, default_config_path());
        assert!(cli.host_options().data_dir.is_none());
    }
}
