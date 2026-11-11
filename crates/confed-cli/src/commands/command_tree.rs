//! `confed commands` — every command, with its arguments and flags.
//!
//! `--help` is written for a reader and one command at a time. This is the
//! same information for a program: the whole tree in one `--json` result, read
//! off the command-line definition itself, so it cannot fall behind it.

use crate::cli::{Cli, CommandsArgs};
use crate::output::Output;
use clap::{Arg, ArgAction, Command, CommandFactory};
use confed_core::error::{ConfedError, Result};
use serde_json::{json, Value};
use std::fmt::Write;

pub fn run(args: &CommandsArgs) -> Result<Output> {
    let mut root = Cli::command();
    // Building it resolves what the definition leaves implied: how many
    // values a flag takes, and which flags every command inherits.
    root.build();

    // The subtree asked for, or all of it.
    let mut at = &root;
    let mut path = vec![root.get_name().to_string()];
    for name in &args.path {
        let found = at.get_subcommands().filter(|c| listed(c)).find(|c| {
            c.get_name() == name.as_str() || c.get_visible_aliases().any(|alias| alias == name)
        });
        at = found.ok_or_else(|| {
            let known: Vec<&str> =
                at.get_subcommands().filter(|c| listed(c)).map(Command::get_name).collect();
            ConfedError::usage_with_hint(
                format!("`{}` has no command `{name}`", path.join(" ")),
                match known.is_empty() {
                    true => format!("`{}` has no subcommands", path.join(" ")),
                    false => format!("its commands are: {}", known.join(", ")),
                },
            )
        })?;
        path.push(at.get_name().to_string());
    }

    let commands: Vec<Value> = match args.path.is_empty() {
        true => root.get_subcommands().filter(|c| listed(c)).map(|c| describe(c, &path)).collect(),
        false => vec![describe(at, &path[..path.len() - 1])],
    };
    let global_flags: Vec<Value> =
        root.get_arguments().filter(|a| a.is_global_set() && shown(a)).map(flag).collect();

    let mut human = String::new();
    for command in &commands {
        usage_lines(command, &mut human);
    }
    let _ = writeln!(
        human,
        "\nEvery command also takes: {}",
        global_flags
            .iter()
            .filter_map(|f| f["long"].as_str().or(f["short"].as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(Output::new(
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "global_flags": global_flags,
            "commands": commands,
        }),
        human,
    ))
}

/// Whether a subcommand is one of confed's own: clap adds `help` to each.
fn listed(command: &Command) -> bool {
    command.get_name() != "help" && !command.is_hide_set()
}

/// Whether an argument is one a caller gives: clap adds --help and --version.
fn shown(arg: &Arg) -> bool {
    !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version")
}

fn text(styled: Option<&clap::builder::StyledStr>) -> Value {
    json!(styled.map(|s| s.to_string().split_whitespace().collect::<Vec<_>>().join(" ")))
}

/// One command, with the commands under it.
fn describe(command: &Command, parents: &[String]) -> Value {
    let mut path = parents.to_vec();
    path.push(command.get_name().to_string());
    let own = || command.get_arguments().filter(|a| !a.is_global_set() && shown(a));
    json!({
        "name": command.get_name(),
        "path": path.join(" "),
        "about": text(command.get_long_about().or(command.get_about())),
        "aliases": command.get_visible_aliases().collect::<Vec<_>>(),
        "args": own().filter(|a| a.is_positional()).map(positional).collect::<Vec<_>>(),
        "flags": own().filter(|a| !a.is_positional()).map(flag).collect::<Vec<_>>(),
        "commands": command
            .get_subcommands()
            .filter(|c| listed(c))
            .map(|c| describe(c, &path))
            .collect::<Vec<_>>(),
    })
}

/// The name a value is shown under: `<KEY>` in `--space <KEY>`.
fn value_name(arg: &Arg) -> String {
    match arg.get_value_names() {
        Some(names) if !names.is_empty() => {
            names.iter().map(ToString::to_string).collect::<Vec<_>>().join(" ")
        }
        _ => arg.get_id().as_str().to_ascii_uppercase(),
    }
}

/// Whether an argument can be given more than one value.
fn multiple(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Append | ArgAction::Count)
        || arg.get_num_args().is_some_and(|n| n.max_values() > 1)
}

fn values(arg: &Arg) -> Vec<String> {
    match arg.get_action() {
        // A switch: clap lists true and false, which nobody types.
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count => Vec::new(),
        _ => arg.get_possible_values().iter().map(|v| v.get_name().to_string()).collect(),
    }
}

fn positional(arg: &Arg) -> Value {
    let mut out = json!({
        "name": value_name(arg),
        "required": arg.is_required_set(),
        "multiple": multiple(arg),
        "help": text(arg.get_long_help().or(arg.get_help())),
    });
    let allowed = values(arg);
    if !allowed.is_empty() {
        out["values"] = json!(allowed);
    }
    out
}

fn flag(arg: &Arg) -> Value {
    // A switch takes no value; anything else names the one it takes.
    let takes_value = arg.get_action().takes_values();
    let mut out = json!({
        "long": arg.get_long().map(|long| format!("--{long}")),
        "short": arg.get_short().map(|short| format!("-{short}")),
        "value": takes_value.then(|| value_name(arg)),
        "required": arg.is_required_set(),
        "multiple": multiple(arg),
        "help": text(arg.get_long_help().or(arg.get_help())),
    });
    let allowed = values(arg);
    if !allowed.is_empty() {
        out["values"] = json!(allowed);
    }
    let defaults: Vec<String> =
        arg.get_default_values().iter().map(|v| v.to_string_lossy().into_owned()).collect();
    if takes_value && !defaults.is_empty() {
        out["default"] = json!(defaults.join(","));
    }
    if let Some(env) = arg.get_env() {
        out["env"] = json!(env.to_string_lossy());
    }
    out
}

/// A usage line for every command that runs something, the tree flattened.
fn usage_lines(command: &Value, out: &mut String) {
    let children = command["commands"].as_array().map(Vec::as_slice).unwrap_or_default();
    if children.is_empty() {
        let mut line = command["path"].as_str().unwrap_or_default().to_string();
        let list = |key: &str| command[key].as_array().cloned().unwrap_or_default();
        for arg in list("args") {
            let name = arg["name"].as_str().unwrap_or_default();
            let more = if arg["multiple"] == json!(true) { "…" } else { "" };
            let _ = match arg["required"] == json!(true) {
                true => write!(line, " <{name}>{more}"),
                false => write!(line, " [{name}]{more}"),
            };
        }
        for flag in list("flags") {
            let name = flag["long"].as_str().or(flag["short"].as_str()).unwrap_or_default();
            let shown = match flag["value"].as_str() {
                Some(value) => format!("{name} <{value}>"),
                None => name.to_string(),
            };
            let _ = match flag["required"] == json!(true) {
                true => write!(line, " {shown}"),
                false => write!(line, " [{shown}]"),
            };
        }
        let _ = writeln!(out, "{line}");
    }
    for child in children {
        usage_lines(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(path: &[&str]) -> Value {
        let args = CommandsArgs { path: path.iter().map(|p| p.to_string()).collect() };
        run(&args).unwrap().result
    }

    fn named<'a>(list: &'a Value, key: &str, name: &str) -> &'a Value {
        list.as_array()
            .unwrap()
            .iter()
            .find(|entry| entry[key] == json!(name))
            .unwrap_or_else(|| panic!("no {name} in {list}"))
    }

    #[test]
    fn the_whole_tree_is_listed_with_arguments_and_flags() {
        let all = tree(&[]);
        assert_eq!(all["version"], env!("CARGO_PKG_VERSION"));
        let space = named(&all["global_flags"], "long", "--space");
        assert_eq!(space["value"], "KEY");
        assert_eq!(space["env"], "CONFED_SPACE");
        assert_eq!(named(&all["global_flags"], "long", "--json")["value"], Value::Null);
        assert_eq!(named(&all["global_flags"], "long", "--yes")["short"], "-y");
        assert!(all["global_flags"].as_array().unwrap().iter().all(|f| f["long"] != "--help"));

        let admin = named(&all["commands"], "name", "admin");
        let perms = named(&admin["commands"], "name", "perms");
        let copy = named(&perms["commands"], "name", "copy");
        assert_eq!(copy["path"], "confed admin perms copy");
        assert_eq!(copy["commands"], json!([]));
        let from = named(&copy["flags"], "long", "--from");
        assert_eq!((from["required"].clone(), from["value"].clone()), (json!(true), json!("KEY")));
        assert_eq!(named(&copy["flags"], "long", "--as")["required"], false);
        assert_eq!(named(&copy["flags"], "long", "--dry-run")["value"], Value::Null);
        assert!(copy["flags"].as_array().unwrap().iter().all(|f| f["long"] != "--json"));

        let grant = named(&perms["commands"], "name", "grant");
        let subject = &grant["args"][0];
        assert_eq!(
            (subject["name"].clone(), subject["required"].clone()),
            (json!("SUBJECT"), json!(true))
        );
        assert_eq!(grant["args"][1]["required"], false);
        let op = named(&grant["flags"], "long", "--op");
        assert_eq!(
            (op["multiple"].clone(), op["value"].clone()),
            (json!(true), json!("TARGET:KEY"))
        );
        assert!(op["help"].as_str().unwrap().contains("space:read"));

        // Every command is there, and none that clap added.
        let names: Vec<&str> = all["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        for name in ["init", "pull", "push", "admin", "instance", "commands", "tui"] {
            assert!(names.contains(&name), "{name} in {names:?}");
        }
        assert!(!names.contains(&"help"));
        assert_eq!(named(&all["commands"], "name", "instance")["aliases"], json!(["i"]));
    }

    #[test]
    fn a_path_narrows_the_tree_to_one_command() {
        let group = tree(&["admin", "group"]);
        assert_eq!(group["commands"].as_array().unwrap().len(), 1);
        assert_eq!(group["commands"][0]["path"], "confed admin group");
        let create = named(&group["commands"][0]["commands"], "name", "create");
        assert_eq!(named(&create["flags"], "long", "--members-from")["value"], "GROUP");
        // An alias names its command.
        assert_eq!(tree(&["i", "create"])["commands"][0]["path"], "confed instance create");

        let err = run(&CommandsArgs { path: vec!["admin".into(), "nosuch".into()] }).unwrap_err();
        assert_eq!(err.exit_code(), confed_core::ExitCode::Usage);
        assert!(err.hint().unwrap().contains("perms"), "{:?}", err.hint());
    }

    #[test]
    fn values_and_defaults_are_named() {
        let all = tree(&[]);
        let progress = named(&all["global_flags"], "long", "--progress");
        assert_eq!(progress["values"], json!(["auto", "jsonl", "none"]));
        assert_eq!(progress["default"], "auto");
        let completion = tree(&["completion"]);
        assert_eq!(
            completion["commands"][0]["args"][0]["values"],
            json!(["bash", "zsh", "fish", "powershell", "elvish"])
        );
    }

    #[test]
    fn people_get_a_usage_line_per_command() {
        let args = CommandsArgs { path: vec!["admin".into(), "group".into()] };
        let human = run(&args).unwrap().human;
        assert!(
            human.contains("confed admin group create <NAME> [--members-from <GROUP>] [--dry-run]"),
            "{human}"
        );
        assert!(
            human.contains("confed admin group remove <GROUP> <USERS>… [--dry-run]"),
            "{human}"
        );
        assert!(human.contains("Every command also takes: --json"), "{human}");
    }
}
