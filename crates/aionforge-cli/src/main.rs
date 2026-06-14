//! The aionforge single binary: serve, doctor, and recover subcommands.

use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;

mod cli;
mod doctor;
mod error;
mod host;
mod observability;
mod recover;
mod serve;

use crate::cli::{Cli, Command};
use crate::error::CliError;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Install the global tracing subscriber BEFORE dispatch: every subcommand (including the
    // synchronous doctor/recover, which emit store traces) is then covered, and it is in
    // place before the serve command builds its tokio runtime. Writes to stderr only — the
    // stdio transport owns stdout for the MCP protocol.
    observability::init(cli.log_format());
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
        Command::Recover(args) => {
            let outcome = recover::run(&host_options, args)?;
            writeln!(output, "{}", outcome.rendered)?;
            Ok(if outcome.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Command::Serve(args) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(serve::run(&host_options, args))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
