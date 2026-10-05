//! `confed completion <shell>`.

use crate::cli::{Cli, CompletionArgs};
use crate::output::Output;
use clap::CommandFactory;
use confed_core::error::{ConfedError, Result};
use serde_json::json;

pub fn run(args: &CompletionArgs) -> Result<Output> {
    let shell: clap_complete::Shell = args
        .shell
        .parse()
        .map_err(|_| ConfedError::usage(format!("unknown shell `{}`", args.shell)))?;

    let mut command = Cli::command();
    let mut buffer = Vec::new();
    clap_complete::generate(shell, &mut command, "confed", &mut buffer);
    let script = String::from_utf8_lossy(&buffer).to_string();

    Ok(Output::new(json!({ "shell": args.shell, "script": script.clone() }), script))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_shell_generates_a_script() {
        for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
            let out = run(&CompletionArgs { shell: shell.into() }).unwrap();
            assert!(!out.human.is_empty(), "{shell} produced nothing");
            assert!(out.human.contains("confed"));
        }
    }

    #[test]
    fn an_unknown_shell_is_a_usage_error() {
        let err = run(&CompletionArgs { shell: "csh".into() }).unwrap_err();
        assert_eq!(err.exit_code(), confed_core::ExitCode::Usage);
    }
}
