//! confed — an offline-first Confluence editor.
//!
//! The binary only wires the pieces together: the commands live in
//! `confed-cli`, the interactive views in `confed-tui`.

fn main() -> std::process::ExitCode {
    confed_cli::main(confed_cli::Views { tui: confed_tui::run, pick_page: confed_tui::pick_page })
}
