//! confed — an offline-first Confluence editor.

mod changelog;
mod cli;
mod commands;
mod context;
mod output;
mod progress;
mod prompt;
mod tui;

use clap::Parser;
use cli::{Cli, Command, GlobalArgs};
use confed_core::error::{ConfedError, Result};
use context::Context;
use output::Output;
use std::time::Instant;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.global);

    let started = Instant::now();
    let command_name = cli.command.name();
    let json = cli.global.json;

    let exit = match run(cli) {
        Ok(out) => {
            output::emit(command_name, &out, json, started.elapsed().as_millis());
            out.exit
        }
        Err(e) => {
            output::emit_error(command_name, &e, json, started.elapsed().as_millis());
            e.exit_code()
        }
    };
    std::process::ExitCode::from(exit.as_i32() as u8)
}

fn run(cli: Cli) -> Result<Output> {
    let Cli { global, command } = cli;

    // These describe the binary itself: no config, no workspace, no network.
    match &command {
        Command::Completion(args) => return commands::completion::run(args),
        Command::Version(args) => return commands::version::run(args),
        _ => {}
    }

    let mut ctx = Context::build(global)?;

    // Purely local commands never touch the network or the keyring.
    match &command {
        Command::Config(args) => return commands::config::run(&mut ctx, args),
        Command::Status(args) if !args.fetch => return commands::status::run(&mut ctx, args),
        Command::Diff(args) if !args.remote => return commands::diff::run(&mut ctx, args),
        Command::Resolve(args) => return commands::resolve::run(&mut ctx, args),
        Command::New(args) if !args.push => return commands::new::run(&mut ctx, args),
        Command::Mkdocs(args) => return commands::mkdocs::run(&mut ctx, args),
        _ => {}
    }

    // The TUI owns its own runtime: its event loop is synchronous and hands work
    // to tokio, rather than being driven by it.
    if matches!(command, Command::Tui) {
        return tui::run(ctx);
    }

    // Credentials are read here, outside the runtime: the OS keyring blocks.
    let prepared_init = match &command {
        Command::Init(args) => Some(commands::init::prepare(&mut ctx, args)?),
        Command::Clone(args) => Some(commands::init::prepare(&mut ctx, &args.init)?),
        _ => {
            ctx.preload_credentials()?;
            None
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ConfedError::io("starting the async runtime", e))?;

    runtime.block_on(dispatch(ctx, command, prepared_init))
}

async fn dispatch(
    mut ctx: Context,
    command: Command,
    prepared_init: Option<commands::init::InitInput>,
) -> Result<Output> {
    match command {
        Command::Init(args) => {
            commands::init::run(&mut ctx, &args, prepared_init.expect("prepared above")).await
        }
        Command::Clone(args) => {
            commands::clone::run(&mut ctx, &args, prepared_init.expect("prepared above")).await
        }
        Command::Fetch(args) => commands::fetch::run(&mut ctx, &args).await,
        Command::Pull(args) => commands::pull::run(&mut ctx, &args).await,
        Command::Push(args) => commands::push::run(&mut ctx, &args).await,
        Command::Status(args) => commands::status::run_with_fetch(&mut ctx, &args).await,
        Command::Diff(args) => commands::diff::run_remote(&mut ctx, &args).await,
        Command::New(args) => commands::new::run_and_push(&mut ctx, &args).await,
        Command::Mv(args) => commands::mv::run(&mut ctx, &args).await,
        Command::Rm(args) => commands::rm::run(&mut ctx, &args).await,
        Command::Attach(args) => commands::attach::run(&mut ctx, &args).await,
        Command::Comment(args) => commands::comment::run(&mut ctx, &args).await,
        Command::Log(args) => commands::log::run(&mut ctx, &args).await,
        Command::Open(args) => commands::open::run(&mut ctx, &args).await,
        Command::Search(args) => commands::search::run(&mut ctx, &args).await,
        Command::Spaces(args) => commands::spaces::run(&mut ctx, &args).await,
        Command::Whoami => commands::whoami::run(&mut ctx).await,
        Command::Doctor(args) => commands::doctor::run(&mut ctx, &args).await,
        Command::Export(args) => commands::export::run(&mut ctx, &args).await,
        Command::Config(_)
        | Command::Resolve(_)
        | Command::Version(_)
        | Command::Completion(_)
        | Command::Mkdocs(_)
        | Command::Tui => {
            unreachable!("handled before the runtime starts")
        }
    }
}

/// Logs go to stderr so `--json` on stdout stays parseable.
fn init_tracing(global: &GlobalArgs) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = match (&global.log, global.quiet, global.verbose) {
        (Some(explicit), _, _) => EnvFilter::new(explicit.clone()),
        (None, true, _) => EnvFilter::new("error"),
        (None, false, 0) => EnvFilter::new("warn"),
        (None, false, 1) => EnvFilter::new("confed=info,confed_core=info,confed_api=info"),
        (None, false, 2) => EnvFilter::new("confed=debug,confed_core=debug,confed_api=debug"),
        (None, false, _) => EnvFilter::new("trace"),
    };

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(global.verbose > 1)
        .without_time()
        .try_init();
}

#[cfg(test)]
mod tests {
    use confed_core::error::ExitCode;

    #[test]
    fn exit_codes_leave_the_process_with_the_documented_number() {
        // A sanity check that the cast in main preserves the contract.
        for code in [
            ExitCode::Ok,
            ExitCode::Usage,
            ExitCode::Conflict,
            ExitCode::State,
            ExitCode::Differences,
        ] {
            assert_eq!(code.as_i32() as u8 as i32, code.as_i32());
        }
    }
}
