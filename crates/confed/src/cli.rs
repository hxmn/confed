//! Command-line surface. Flags here mirror docs/design/04-command-reference.md.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "confed",
    version,
    about = "Offline-first Confluence editor: sync a space to Markdown files",
    long_about = "confed mirrors a Confluence space into Markdown files and moves changes \
                  between disk and server with a git-like command model.\n\n\
                  Every parameter resolves in this order: CLI flag, CONFED_* environment \
                  variable, stored config from `confed init`, then an interactive prompt \
                  (only when stdin is a terminal).",
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone, Default)]
pub struct GlobalArgs {
    /// Machine-readable output. Implies --non-interactive.
    #[arg(long, global = true, env = "CONFED_JSON")]
    pub json: bool,

    /// Never prompt; missing values fail with exit code 2.
    #[arg(long, global = true, env = "CONFED_NON_INTERACTIVE")]
    pub non_interactive: bool,

    /// Confluence base URL (Cloud: https://site.atlassian.net/wiki).
    #[arg(long, global = true, env = "CONFED_BASE_URL")]
    pub base_url: Option<String>,

    /// API token (Cloud) or Personal Access Token (Data Center).
    #[arg(long, global = true, env = "CONFED_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// Cloud account email, or Data Center username for basic auth.
    #[arg(long = "user", global = true, env = "CONFED_USERNAME")]
    pub username: Option<String>,

    /// Space key bound to this directory.
    #[arg(long, global = true, env = "CONFED_SPACE")]
    pub space: Option<String>,

    /// Skip flavor auto-detection.
    #[arg(long, global = true, env = "CONFED_FLAVOR", value_parser = ["cloud", "dc", "datacenter"])]
    pub flavor: Option<String>,

    /// Maximum concurrent API requests.
    #[arg(long, global = true, env = "CONFED_CONCURRENCY")]
    pub concurrency: Option<usize>,

    /// Run as if confed was started in this directory.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    pub directory: Option<PathBuf>,

    /// Answer yes to confirmations (destructive operations are still listed).
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// More detail on stderr; repeat for more.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress non-essential output.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Do not show progress while long operations run.
    #[arg(long, global = true, env = "CONFED_SILENT")]
    pub silent: bool,

    /// Tracing filter, e.g. `confed::http=debug`.
    #[arg(long, global = true, env = "CONFED_LOG")]
    pub log: Option<String>,
}

impl GlobalArgs {
    /// Prompting is possible only on a terminal, and never in machine modes.
    pub fn interactive(&self) -> bool {
        use std::io::IsTerminal;
        !self.json && !self.non_interactive && std::io::stdin().is_terminal()
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Authenticate, bind this directory to a space, and create local state.
    Init(InitArgs),

    /// Init plus a first pull, in one step.
    Clone(CloneArgs),

    /// Download remote state into .state.db without touching working files.
    Fetch(FetchArgs),

    /// Fetch and materialize pages, attachments and comments into files.
    Pull(PullArgs),

    /// Upload local changes.
    Push(PushArgs),

    /// Summary of what changed locally and remotely.
    Status(StatusArgs),

    /// Show differences between base, local files and the server.
    Diff(DiffArgs),

    /// Finish a merge and clear the conflicted state.
    Resolve(ResolveArgs),

    /// Scaffold a new page.
    New(NewArgs),

    /// Rename, move, or reorder a page.
    Mv(MvArgs),

    /// Delete a page locally, and on the server at the next push.
    Rm(RmArgs),

    /// Manage a page's attachments.
    Attach(AttachArgs),

    /// Read and write page comments.
    #[command(subcommand)]
    Comment(CommentCommand),

    /// Version history of a page.
    Log(LogArgs),

    /// Open a page in the browser.
    Open(OpenArgs),

    /// Search the server with CQL or plain text.
    Search(SearchArgs),

    /// List spaces visible to you.
    Spaces(SpacesArgs),

    /// Show the authenticated user and instance capabilities.
    Whoami,

    /// Inspect and change stored settings.
    Config(ConfigArgs),

    /// Check connectivity, credentials, and local state.
    Doctor(DoctorArgs),

    /// Export pages to another format.
    Export(ExportArgs),

    /// Generate a shell completion script.
    Completion(CompletionArgs),

    /// Browse the space, diff, resolve conflicts and sync, interactively.
    Tui,
}

impl Command {
    /// Name used in the JSON envelope.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Clone(_) => "clone",
            Command::Fetch(_) => "fetch",
            Command::Pull(_) => "pull",
            Command::Push(_) => "push",
            Command::Status(_) => "status",
            Command::Diff(_) => "diff",
            Command::Resolve(_) => "resolve",
            Command::New(_) => "new",
            Command::Mv(_) => "mv",
            Command::Rm(_) => "rm",
            Command::Attach(_) => "attach",
            Command::Comment(_) => "comment",
            Command::Log(_) => "log",
            Command::Open(_) => "open",
            Command::Search(_) => "search",
            Command::Spaces(_) => "spaces",
            Command::Whoami => "whoami",
            Command::Config(_) => "config",
            Command::Doctor(_) => "doctor",
            Command::Export(_) => "export",
            Command::Completion(_) => "completion",
            Command::Tui => "tui",
        }
    }
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Where to store credentials.
    #[arg(long, value_parser = ["keyring", "sqlite"])]
    pub credential_store: Option<String>,

    /// Do not generate CLAUDE.md and AGENTS.md.
    #[arg(long)]
    pub no_agent_docs: bool,

    /// Re-initialize a directory that is already bound to a space.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct CloneArgs {
    /// Space key, or a Confluence space URL.
    pub source: String,

    /// Directory to create (defaults to the space key).
    pub directory: Option<PathBuf>,

    #[command(flatten)]
    pub init: InitArgs,
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    /// Fetch only these pages (id or path).
    #[arg(long = "page", value_name = "PAGE")]
    pub pages: Vec<String>,

    /// Only pages modified at or after this RFC 3339 timestamp.
    #[arg(long)]
    pub since: Option<String>,
}

#[derive(Args, Debug)]
pub struct PullArgs {
    /// Paths or globs to pull; default is the whole space.
    pub paths: Vec<String>,

    /// Pull only these pages by id.
    #[arg(long = "page", value_name = "ID")]
    pub pages: Vec<String>,

    /// Only pages carrying this label.
    #[arg(long)]
    pub label: Option<String>,

    /// Restrict with a CQL query.
    #[arg(long)]
    pub cql: Option<String>,

    /// Use the state already fetched.
    #[arg(long)]
    pub no_fetch: bool,

    /// Overwrite local changes.
    #[arg(long)]
    pub force: bool,

    /// Discard local changes and make every tracked page match the server
    /// again. Files that exist only locally are left alone.
    #[arg(long)]
    pub reset: bool,

    /// Do not merge diverged pages; stop instead.
    #[arg(long)]
    pub no_merge: bool,

    /// Report what would be written without writing it.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip downloading attachments.
    #[arg(long)]
    pub no_attachments: bool,

    /// Skip writing comment sidecars.
    #[arg(long)]
    pub no_comments: bool,
}

#[derive(Args, Debug)]
pub struct PushArgs {
    /// Paths or globs to push; default is everything with local changes.
    pub paths: Vec<String>,

    /// Show exactly what would be sent, without sending it.
    #[arg(long, visible_alias = "preview")]
    pub dry_run: bool,

    /// Confirm each page before uploading it.
    #[arg(long)]
    pub interactive: bool,

    /// Version comment recorded on the server.
    #[arg(long, short = 'm')]
    pub message: Option<String>,

    /// Delete pages on the server that were deleted locally.
    #[arg(long)]
    pub allow_delete: bool,

    /// Do not upload attachments.
    #[arg(long)]
    pub no_attachments: bool,

    /// Do not post comment drafts.
    #[arg(long)]
    pub no_comments: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Refresh remote state first.
    #[arg(long)]
    pub fetch: bool,

    /// One line per page, machine-friendly.
    #[arg(long)]
    pub short: bool,

    /// Exit 10 when anything differs.
    #[arg(long)]
    pub exit_code: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Paths or globs to diff.
    pub paths: Vec<String>,

    /// Compare local files against the fetched remote state.
    #[arg(long)]
    pub remote: bool,

    /// Show base, local and remote together.
    #[arg(long)]
    pub base: bool,

    /// Diff the Confluence markup that push would upload, rather than the Markdown.
    #[arg(long = "conf-format", visible_alias = "storage")]
    pub storage: bool,

    /// Summarize instead of showing hunks.
    #[arg(long)]
    pub stat: bool,

    /// List changed paths only.
    #[arg(long)]
    pub name_only: bool,

    /// Exit 10 when there are differences.
    #[arg(long)]
    pub exit_code: bool,
}

#[derive(Args, Debug)]
pub struct ResolveArgs {
    /// Pages to mark resolved.
    pub paths: Vec<String>,

    /// Discard the remote side.
    #[arg(long, conflicts_with = "theirs")]
    pub ours: bool,

    /// Discard the local side.
    #[arg(long)]
    pub theirs: bool,

    /// List unresolved conflicts.
    #[arg(long)]
    pub list: bool,
}

#[derive(Args, Debug)]
pub struct NewArgs {
    /// Path of the new page, e.g. "Runbooks/Database Failover".
    pub path: String,

    /// Page title (defaults to the filename).
    #[arg(long)]
    pub title: Option<String>,

    /// Labels to apply.
    #[arg(long = "label", value_name = "LABEL")]
    pub labels: Vec<String>,

    /// Start from this Markdown file.
    #[arg(long)]
    pub template: Option<PathBuf>,

    /// Create it on the server immediately.
    #[arg(long)]
    pub push: bool,
}

#[derive(Args, Debug)]
pub struct MvArgs {
    /// Page to move (path or id).
    pub source: String,

    /// New path.
    pub destination: Option<String>,

    /// Place before this sibling.
    #[arg(long, conflicts_with_all = ["after", "position"])]
    pub before: Option<String>,

    /// Place after this sibling.
    #[arg(long, conflicts_with = "position")]
    pub after: Option<String>,

    /// Absolute position among siblings.
    #[arg(long)]
    pub position: Option<i64>,

    /// Also change the page title to match the new filename.
    #[arg(long)]
    pub rename_title: bool,

    /// Apply on the server immediately.
    #[arg(long)]
    pub push: bool,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    /// Pages to delete (paths or ids).
    pub paths: Vec<String>,

    /// Record the deletion but keep the local file.
    #[arg(long)]
    pub keep_local: bool,

    /// Delete on the server immediately.
    #[arg(long)]
    pub push: bool,
}

#[derive(Args, Debug)]
pub struct AttachArgs {
    /// Page to attach to (path or id).
    pub page: String,

    /// Files to attach.
    pub files: Vec<PathBuf>,

    /// List the page's attachments.
    #[arg(long)]
    pub list: bool,

    /// Remove an attachment by filename.
    #[arg(long = "rm", value_name = "FILENAME")]
    pub remove: Option<String>,

    /// Upload immediately.
    #[arg(long)]
    pub push: bool,
}

#[derive(Subcommand, Debug)]
pub enum CommentCommand {
    /// Show a page's comments.
    List {
        page: String,
        #[arg(long)]
        unresolved: bool,
        #[arg(long)]
        inline: bool,
    },
    /// Add a comment.
    Add {
        page: String,
        #[arg(long, short = 'm')]
        body: Option<String>,
        /// Anchor an inline comment to this text (Cloud only).
        #[arg(long)]
        anchor: Option<String>,
        #[arg(long)]
        push: bool,
    },
    /// Reply to a comment.
    Reply {
        comment_id: String,
        #[arg(long, short = 'm')]
        body: String,
        #[arg(long)]
        push: bool,
    },
    /// Mark a comment resolved (Cloud only).
    Resolve {
        comment_id: String,
        #[arg(long)]
        push: bool,
    },
}

#[derive(Args, Debug)]
pub struct LogArgs {
    /// Page to inspect (path or id).
    pub page: String,

    /// How many versions to show.
    #[arg(long, default_value = "20")]
    pub limit: usize,

    /// Show confed's local sync log instead of server history.
    #[arg(long)]
    pub local: bool,
}

#[derive(Args, Debug)]
pub struct OpenArgs {
    /// Page to open (path or id). Omit to open the space.
    pub page: Option<String>,

    /// Print the URL instead of launching a browser.
    #[arg(long)]
    pub print: bool,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// CQL query, or plain text.
    pub query: Vec<String>,

    #[arg(long, default_value = "25")]
    pub limit: usize,

    /// Search every space, not just this one.
    #[arg(long)]
    pub all_spaces: bool,
}

#[derive(Args, Debug)]
pub struct SpacesArgs {
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// Show every resolved value and where it came from.
    #[arg(long)]
    pub list: bool,

    /// Read one value.
    #[arg(long, value_name = "KEY")]
    pub get: Option<String>,

    /// Write one value.
    #[arg(long, num_args = 2, value_names = ["KEY", "VALUE"])]
    pub set: Vec<String>,

    /// Remove one value.
    #[arg(long, value_name = "KEY")]
    pub unset: Option<String>,

    /// Keep the credential in .session.db (mode 0600) so reading it never
    /// prompts for a keychain password.
    #[arg(long, conflicts_with = "force_keychain")]
    pub no_keychain: bool,

    /// Move the credential back into the OS keychain.
    #[arg(long)]
    pub force_keychain: bool,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Apply the safe fixes (.gitignore, agent docs).
    #[arg(long)]
    pub fix: bool,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Paths or globs to export.
    pub paths: Vec<String>,

    #[arg(long, default_value = "html", value_parser = ["html", "storage"])]
    pub format: String,

    #[arg(long, default_value = "export")]
    pub out: PathBuf,
}

#[derive(Args, Debug)]
pub struct CompletionArgs {
    #[arg(value_parser = ["bash", "zsh", "fish", "powershell", "elvish"])]
    pub shell: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["confed", "status", "--json", "--space", "DOCS"]).unwrap();
        assert!(cli.global.json);
        assert_eq!(cli.global.space.as_deref(), Some("DOCS"));
        assert_eq!(cli.command.name(), "status");
    }

    #[test]
    fn json_mode_disables_prompting() {
        let cli = Cli::try_parse_from(["confed", "--json", "status"]).unwrap();
        assert!(!cli.global.interactive(), "--json must never prompt");
    }

    #[test]
    fn push_preview_is_an_alias_for_dry_run() {
        let cli = Cli::try_parse_from(["confed", "push", "--preview"]).unwrap();
        match cli.command {
            Command::Push(args) => assert!(args.dry_run),
            other => panic!("expected push, got {other:?}"),
        }
    }

    #[test]
    fn pull_takes_paths_and_scope_filters() {
        let cli =
            Cli::try_parse_from(["confed", "pull", "Handbook/**", "--label", "runbook"]).unwrap();
        match cli.command {
            Command::Pull(args) => {
                assert_eq!(args.paths, ["Handbook/**"]);
                assert_eq!(args.label.as_deref(), Some("runbook"));
            }
            other => panic!("expected pull, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_resolve_sides_are_rejected() {
        assert!(Cli::try_parse_from(["confed", "resolve", "--ours", "--theirs", "p.md"]).is_err());
    }

    #[test]
    fn the_tui_is_a_subcommand_like_any_other() {
        let cli = Cli::try_parse_from(["confed", "tui"]).unwrap();
        assert_eq!(cli.command.name(), "tui");
    }

    #[test]
    fn an_unknown_flavor_is_rejected_at_parse_time() {
        assert!(Cli::try_parse_from(["confed", "--flavor", "sharepoint", "status"]).is_err());
        assert!(Cli::try_parse_from(["confed", "--flavor", "dc", "status"]).is_ok());
    }
}
