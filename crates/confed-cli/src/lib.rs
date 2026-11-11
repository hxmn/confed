//! The commands of confed, the offline-first Confluence editor.
//!
//! [`main`] is the whole program: it parses the arguments, runs the command and
//! reports it. The `confed` binary calls it with the TUI's entry points, which
//! live in `confed-tui` and build on the [`context`], [`output`] and
//! [`commands`] here.

pub mod changelog;
pub mod cli;
pub mod commands;
pub mod context;
pub mod output;
pub mod pageref;
pub mod progress;
pub mod prompt;

use clap::{CommandFactory, Parser};
use cli::{Cli, Command, GlobalArgs};
use confed_core::error::{ConfedError, Result};
use confed_core::workspace::Workspace;
use context::Context;
use output::Output;
use std::time::Instant;

/// The interactive views, which live in `confed-tui`. The app passes them in;
/// they are parameters so this crate does not depend on the TUI, which itself
/// builds on this crate's context and commands.
#[derive(Clone, Copy)]
pub struct Views {
    /// `confed tui`.
    pub tui: TuiLauncher,
    /// The page picker behind `confed config --set rules_page_id`.
    pub pick_page: PagePicker,
}

/// Starts the interactive views for `confed tui`: `confed_tui::run`.
pub type TuiLauncher = fn(Context) -> Result<Output>;

/// Lets the user choose one page of the workspace's space, starting on the
/// page with the given id: `confed_tui::pick_page`. Returns the chosen page's
/// id, or `None` when the user backs out.
pub type PagePicker = fn(&Workspace, Option<&str>) -> Result<Option<String>>;

/// Run confed for this process's arguments and return its exit code.
pub fn main(views: Views) -> std::process::ExitCode {
    let mut cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return usage_error(error),
    };
    init_tracing(&cli.global);
    if cli.global.select.is_some() {
        cli.global.json = true;
    }

    let started = Instant::now();
    let command_name = cli.command.name();
    let json = cli.global.json;
    let select = cli.global.select.clone();

    let exit = match run(cli, views) {
        Ok(out) => {
            output::emit(
                command_name,
                &out,
                json,
                select.as_deref(),
                started.elapsed().as_millis(),
            );
            out.exit
        }
        Err(e) => {
            output::emit_error(command_name, &e, json, started.elapsed().as_millis());
            e.exit_code()
        }
    };
    std::process::ExitCode::from(exit.as_i32() as u8)
}

/// A command line clap could not read. Asked for JSON — `--json`,
/// `--select`, or `CONFED_JSON` — it is answered with the error envelope like
/// every other failure, so a script reading stdout gets JSON whatever it
/// typed; otherwise clap reports it, as it does help and the version.
fn usage_error(error: clap::Error) -> std::process::ExitCode {
    use clap::error::ErrorKind;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json_flag =
        args.iter().any(|a| a == "--json" || a == "--select" || a.starts_with("--select="));
    let json_env = std::env::var("CONFED_JSON")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    let informational = matches!(error.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion);
    if informational || !(json_flag || json_env) {
        error.exit();
    }
    let rendered = error.to_string();
    let message = rendered
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("could not read the command line")
        .trim_start_matches("error: ")
        .to_string();
    // The command it was meant to be, as far as there is one.
    let known = Cli::command();
    let command = args
        .iter()
        .find(|a| known.find_subcommand(a.as_str()).is_some())
        .cloned()
        .unwrap_or_else(|| "confed".to_string());
    let error = ConfedError::usage_with_hint(
        message,
        "run `confed --help` for the commands and their flags",
    );
    output::emit_error(&command, &error, true, 0);
    std::process::ExitCode::from(error.exit_code().as_i32() as u8)
}

fn run(cli: Cli, views: Views) -> Result<Output> {
    let Cli { global, command } = cli;

    // These describe the binary itself: no config, no workspace, no network.
    match &command {
        Command::Completion(args) => return commands::completion::run(args),
        Command::Version(args) => return commands::version::run(args),
        _ => {}
    }

    let ctx = Context::build(global)?;
    let guide = stale_guide_warning(&ctx, &command);
    let output = run_in(ctx, command, views)?;
    Ok(match guide {
        Some(warning) => output.warn(warning),
        None => output,
    })
}

/// The agent guide (`CLAUDE.md`/`AGENTS.md`) describes the confed that wrote
/// it. After an upgrade an agent would follow obsolete rules, so every command
/// in the workspace says so until it is regenerated.
fn stale_guide_warning(ctx: &Context, command: &Command) -> Option<String> {
    if let Command::Instance(command) = command {
        return stale_instance_guides(ctx, command);
    }
    if matches!(command, Command::Doctor(_) | Command::Init(_) | Command::Clone(_)) {
        return None;
    }
    let root = ctx.workspace().ok()?.root().to_path_buf();
    let (_, stale) = commands::agent_docs::audit(&root);
    let (name, written_by) = stale.first()?;
    Some(format!(
        "{name} was written by {} and describes that version, not {}; \
         run `confed doctor --fix` to regenerate it",
        commands::agent_docs::writer(written_by),
        commands::agent_docs::VERSION
    ))
}

/// The same for an instance root: its own guide and every member's, named
/// in one warning, with the one command that rewrites them all.
fn stale_instance_guides(ctx: &Context, command: &cli::InstanceCommand) -> Option<String> {
    use cli::InstanceCommand as I;
    if matches!(command, I::Init(_) | I::Adopt(_) | I::Doctor(_) | I::Add(_) | I::Remove(_)) {
        return None;
    }
    let instance = ctx.instance().ok()?;
    let mut stale: Vec<String> = Vec::new();
    let (_, root) = commands::agent_docs::audit(instance.root());
    if let Some((name, by)) = root.first() {
        stale.push(format!("{name} ({})", commands::agent_docs::writer(by)));
    }
    for member in instance.enabled_members().ok()? {
        let (_, old) = commands::agent_docs::audit(&instance.member_dir(&member));
        if let Some((name, by)) = old.first() {
            stale.push(format!("{}/{name} ({})", member.dir, commands::agent_docs::writer(by)));
        }
    }
    if stale.is_empty() {
        return None;
    }
    Some(format!(
        "agent docs describe another confed than this one ({}): {}; run `confed instance \
         doctor --fix` to regenerate them",
        commands::agent_docs::VERSION,
        stale.join(", ")
    ))
}

fn run_in(mut ctx: Context, command: Command, views: Views) -> Result<Output> {
    // Purely local commands never touch the network or the keyring.
    match &command {
        Command::Config(args) => return commands::config::run(&mut ctx, args, views.pick_page),
        Command::Status(args) if !args.fetch => return commands::status::run(&mut ctx, args),
        Command::Diff(args) if !args.remote => return commands::diff::run(&mut ctx, args),
        Command::Resolve(args) => return commands::resolve::run(&mut ctx, args),
        Command::New(args) if !args.push => return commands::new::run(&mut ctx, args),
        Command::Log(args) if args.local => return commands::log::run_local(&mut ctx, args),
        Command::Mkdocs(args) => return commands::mkdocs::run(&mut ctx, args),
        Command::Tree(args) if !args.remote => return commands::tree::run(&mut ctx, args),
        Command::Mv(args) if !args.push => return commands::mv::run(&mut ctx, args),
        Command::Rm(args) if !args.push || args.dry_run => {
            return commands::rm::run(&mut ctx, args)
        }
        _ => {}
    }

    // The TUI owns its own runtime: its event loop is synchronous and hands work
    // to tokio, rather than being driven by it.
    if matches!(command, Command::Tui) {
        return (views.tui)(ctx);
    }

    // Credentials are read here, outside the runtime: the OS keyring blocks.
    let mut prepared_instance = None;
    let prepared_init = match &command {
        Command::Init(args) => Some(commands::init::prepare(&mut ctx, args)?),
        Command::Clone(args) => Some(commands::clone::prepare(&mut ctx, args)?),
        Command::Instance(cmd) => {
            prepared_instance = Some(commands::instance::prepare(&mut ctx, cmd)?);
            None
        }
        Command::Admin(_) => {
            commands::admin::prepare(&mut ctx)?;
            None
        }
        _ => {
            // A credential is read only by a command that will use one. Any
            // other loads it itself, should it turn out to need the server.
            if needs_credentials(&command) {
                ctx.preload_credentials()?;
            }
            None
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ConfedError::io("starting the async runtime", e))?;

    let graceful = stops_between_pages(&command);
    let output = runtime.block_on(async {
        watch_interrupts(graceful);
        dispatch(ctx, command, prepared_init, prepared_instance).await
    })?;
    // Stopped by Ctrl-C between pages: the outcome lists what was done, and
    // the exit code says it is not all.
    if graceful && confed_core::interrupt::requested() {
        let mut output = output.warn(
            "interrupted: stopped after the page in hand; what is listed was done, and running \
             the command again finishes the rest",
        );
        output.exit = confed_core::ExitCode::Interrupted;
        return Ok(output);
    }
    Ok(output)
}

/// Whether `command` is sure to talk to the server. Adding attachments or
/// comment drafts without `--push` only writes files, and must work with no
/// credential at all.
fn needs_credentials(command: &Command) -> bool {
    use cli::CommentCommand as C;
    match command {
        Command::Attach(args) => args.push || (args.list && args.remote),
        Command::Comment(C::Add { push, .. } | C::Reply { push, .. } | C::Resolve { push, .. }) => {
            *push
        }
        Command::Comment(C::List { refresh, .. }) => *refresh,
        _ => true,
    }
}

/// Whether `command` runs through the engine's page loops, which look at
/// the interrupt flag between pages. Any other command has no point at
/// which to stop cleanly, so Ctrl-C ends it at once.
fn stops_between_pages(command: &Command) -> bool {
    use cli::InstanceCommand as I;
    match command {
        Command::Fetch(_) | Command::Pull(_) | Command::Push(_) | Command::Clone(_) => true,
        Command::Status(args) => args.fetch,
        Command::Diff(args) => args.remote,
        Command::New(args) => args.push,
        Command::Mv(args) => args.push,
        Command::Rm(args) => args.push,
        Command::Comment(_) => true,
        Command::Instance(I::Fetch(_) | I::Pull(_) | I::Push(_) | I::Clone(_)) => true,
        _ => false,
    }
}

/// For a command that stops between pages, the first Ctrl-C asks it to
/// stop at the next page boundary, so no page file is left out of step with
/// what `.state.db` records for it; the second quits at once, for a command
/// stuck waiting. Any other command quits on the first.
fn watch_interrupts(graceful: bool) {
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if !graceful || confed_core::interrupt::requested() {
                eprintln!("confed: interrupted");
                std::process::exit(confed_core::ExitCode::Interrupted.as_i32());
            }
            confed_core::interrupt::request();
            eprintln!(
                "confed: stopping after the page in hand (press Ctrl-C again to quit at once)"
            );
        }
    });
}

async fn dispatch(
    mut ctx: Context,
    command: Command,
    prepared_init: Option<commands::init::InitInput>,
    prepared_instance: Option<commands::instance::Prepared>,
) -> Result<Output> {
    match command {
        Command::Instance(cmd) => {
            commands::instance::run(&mut ctx, cmd, prepared_instance.expect("prepared above")).await
        }
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
        Command::Mv(args) => commands::mv::run_and_push(&mut ctx, &args).await,
        Command::Rm(args) => commands::rm::run_and_push(&mut ctx, &args).await,
        Command::Attach(args) => commands::attach::run(&mut ctx, &args).await,
        Command::Comment(args) => commands::comment::run(&mut ctx, &args).await,
        Command::Log(args) => commands::log::run(&mut ctx, &args).await,
        Command::Open(args) => commands::open::run(&mut ctx, &args).await,
        Command::Search(args) => commands::search::run(&mut ctx, &args).await,
        Command::Spaces(args) => commands::spaces::run(&mut ctx, &args).await,
        Command::Space(args) => commands::space::run(&mut ctx, &args).await,
        Command::User(args) => commands::user::run(&mut ctx, &args).await,
        Command::Admin(args) => commands::admin::run(&mut ctx, &args).await,
        Command::Whoami(args) => commands::whoami::run(&mut ctx, &args).await,
        Command::Doctor(args) => commands::doctor::run(&mut ctx, &args).await,
        Command::Export(args) => commands::export::run(&mut ctx, &args).await,
        Command::Tree(args) => commands::tree::run_remote(&mut ctx, &args).await,
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
        // Targets match by prefix: `confed` covers every confed crate.
        (None, false, 1) => EnvFilter::new("confed=info"),
        (None, false, 2) => EnvFilter::new("confed=debug"),
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
            ExitCode::Interrupted,
        ] {
            assert_eq!(code.as_i32() as u8 as i32, code.as_i32());
        }
    }
}
