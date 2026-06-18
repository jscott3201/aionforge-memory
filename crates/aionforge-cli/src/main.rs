//! The aionforge single binary: serve, doctor, and recover subcommands.

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

mod cli;
mod consolidation_config;
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

    // DO NOT hold the process-global stdout/stderr locks across `run`. `run` parks the main
    // thread for the entire lifetime of the server inside `runtime.block_on(serve::run(...))`,
    // and the tracing subscriber installed above writes to `std::io::stderr`. If main held the
    // std reentrant stdio lock here, a tokio WORKER thread emitting a `tracing` event during a
    // request (e.g. rmcp's `info!("create new session")` on the first stateful `initialize`)
    // would block forever in `Stderr::write_all` waiting on a lock the parked main thread owns
    // — a cross-thread deadlock: the request never returns and (because the blocking write IS
    // the log emission) no log line ever appears. That was the #252 `serve http` `initialize`
    // hang. The locks must be acquired narrowly, only on the synchronous paths that print
    // (Doctor/Recover stdout below, and the error arm's stderr), never spanning the async serve.
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            // Freshly acquire stderr only in the (terminal) error arm — no worker thread is
            // running here, so this can never race the subscriber's stderr writer. (Using
            // `writeln!` to a scoped lock rather than `eprintln!` also keeps the workspace's
            // `clippy::print_stderr` lint satisfied.)
            let _ = writeln!(std::io::stderr().lock(), "error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, CliError> {
    let host_options = cli.host_options();
    match cli.command {
        Command::Doctor(args) => {
            let outcome = doctor::run(&host_options, args)?;
            // Lock stdout only here, scoped to this synchronous arm — never across the async
            // serve (see the deadlock note in `main`). The Serve arm never writes to stdout.
            writeln!(std::io::stdout().lock(), "{}", outcome.rendered)?;
            Ok(if outcome.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Command::Recover(args) => {
            let outcome = recover::run(&host_options, args)?;
            // Lock stdout only here, scoped to this synchronous arm — see the Doctor arm above.
            writeln!(std::io::stdout().lock(), "{}", outcome.rendered)?;
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
