//! Routes a parsed command to the standalone or the in-vault handler.

use anyhow::Result;

use super::Output;
use super::args::{Cli, Command};
use super::lifecycle_cmd;
use super::session::{Session, finish};
use super::standalone::run_standalone;
use super::vault_cmd::run_in_vault;

pub(crate) fn run(cli: Cli) -> Result<Output> {
    let Cli {
        vault: root,
        json,
        command,
    } = cli;
    if matches!(
        command,
        Command::Mcp { .. }
            | Command::Note { .. }
            | Command::Init { .. }
            | Command::Watch { .. }
            | Command::Setup { .. }
            | Command::Uninstall
    ) {
        return run_standalone(&root, json, command);
    }
    // `wi commit` ingests on its own. `wi import` commits the originals it names together with
    // their sidecars in one bulk commit, then takes in other editor changes before the closing
    // flush. Every other command takes in editor changes first.
    let ingest = !matches!(
        command,
        Command::Commit | Command::Import { .. } | Command::Sync { .. }
    );
    let session = Session::open(&root, ingest)?;
    let reports_commits = reports_commits(&command);
    let result = run_in_vault(&session, command);
    finish(&session, result, reports_commits, json)
}

/// Commands whose JSON carries the number of commits their flush made.
pub(crate) fn reports_commits(command: &Command) -> bool {
    matches!(
        command,
        Command::Write { .. }
            | Command::Edit { .. }
            | Command::Move { .. }
            | Command::Archive { .. }
            | Command::Revert { .. }
            | Command::Capture(_)
            | Command::Adopt { .. }
            | Command::Sweep
            | Command::Work {
                command: lifecycle_cmd::WorkCommand::Close { .. }
                    | lifecycle_cmd::WorkCommand::OpenConflict { .. }
            }
    )
}
