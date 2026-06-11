//! The aionforge single binary: serve, doctor, migrate, and recover subcommands.

use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;

mod cli;
mod doctor;
mod error;
mod host;

use crate::cli::{Cli, Command};
use crate::error::CliError;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();

    match run(cli, &mut stdout) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(stderr, "error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli, output: &mut impl Write) -> Result<ExitCode, CliError> {
    let host_options = cli.host_options();
    match cli.command {
        Command::Doctor(args) => {
            let outcome = doctor::run(&host_options, args)?;
            writeln!(output, "{}", outcome.rendered)?;
            Ok(if outcome.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
    }
}
