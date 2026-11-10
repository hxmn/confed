//! The environment every spawned `confed` starts from: none of the
//! developer's. A `CONFED_*` variable, a proxy, a keyring session or a colour
//! setting in the shell that runs the tests cannot change what they exercise,
//! and a variable added to confed later needs no list kept up to date here.

use std::process::Command;

/// The confed binary, with an empty environment apart from what any process
/// needs to run: `PATH`, and on Windows `SYSTEMROOT`, without which sockets
/// cannot be opened.
pub fn confed_command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_confed"));
    cmd.env_clear();
    for keep in ["PATH", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(keep) {
            cmd.env(keep, value);
        }
    }
    cmd.env("NO_COLOR", "1");
    cmd
}
