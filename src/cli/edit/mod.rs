//! `rdc edit`: local project maintenance. Every action under it works on
//! the project on disk and never contacts Rossum.

pub mod ci;
pub mod env;

use crate::log::{Action, Log};
use anyhow::Context;
use clap::Subcommand;
use clap_complete::ArgValueCandidates;

#[derive(Debug, Subcommand)]
pub enum EditCommand {
    /// Manage the project's environments.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum EnvCommand {
    /// Rename an environment in the local project.
    #[command(long_about = RENAME_LONG_ABOUT)]
    Rename {
        /// The env's current name, as in `rdc.toml`.
        #[arg(add = ArgValueCandidates::new(super::env_name_candidates))]
        old: String,
        /// The new name. Letters, digits, `-` and `_` only.
        new: String,
        /// Print the changes without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

const RENAME_LONG_ABOUT: &str = r#"Rename an environment in the local project. Makes no API calls.

Moves envs/<old>/, secrets/<old>.secrets.json,
secrets/<old>.hook-secrets.json and the env's files under .rdc/. Renames the
env in rdc.toml and .rdc/mapping.toml, and updates the rdc regions of
README.md, CLAUDE.md and .gitlab-ci.yml. In the pipeline's deploy jobs it
changes values equal to <old>, the deploy:<old> job key, and references to
that job such as `needs:`.

It refuses before writing when <new> is already an env, a target path
exists, or another rdc process holds <old>'s lock. It also refuses when a
deploy:<new> job exists. If a step fails, it reverts every change.

It prints the GitLab CI variables (RDC_TOKEN_<ENV>) to rename by hand."#;

pub fn run(command: EditCommand) -> anyhow::Result<()> {
    match command {
        EditCommand::Env { command: EnvCommand::Rename { old, new, dry_run } } => {
            let cwd = std::env::current_dir().context("getting current directory")?;
            let report = env::rename_env(&cwd, &old, &new, dry_run)?;
            print_report(&Log::new(crate::cli::resolve::detect_color_mode()), &report);
            Ok(())
        }
    }
}

fn print_report(log: &Log, r: &env::RenameReport) {
    let (action, verb) = if r.dry_run { (Action::Plan, "would rename") } else { (Action::Done, "renamed") };
    log.event(action, &format!("{verb} env {} -> {}", r.old, r.new));
    for (from, to) in &r.moved {
        log.row(&format!("         moved    {} -> {}", from.display(), to.display()));
    }
    for path in &r.rewritten {
        log.row(&format!("         rewrote  {}", path.display()));
    }
    for change in &r.ci_changes {
        log.row(&format!("         pipeline {change}"));
    }
    for warning in &r.warnings {
        log.event(Action::Warn, warning);
    }
    if !r.follow_ups.is_empty() {
        let lines: Vec<String> = r.follow_ups.iter().map(|f| format!("  {f}")).collect();
        log.block(&format!("still to do in GitLab:\n{}", lines.join("\n")));
    }
}
