use clap::{ColorChoice, CommandFactory, FromArgMatches};
use rdc::cli::{run, Cli};
use std::io::IsTerminal;

#[tokio::main]
async fn main() {
    // Handle the COMPLETE=<shell> rdc invocation that the shell makes
    // to fetch completion candidates / emit its setup script. Must run
    // before any other stdout writes — clap_complete's protocol assumes
    // stdout is reserved for its output. If the env var isn't set this
    // is a cheap no-op and falls through to the normal CLI path.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let cli = parse_with_color_choice();
    if let Err(err) = run(cli).await {
        let log = rdc::log::Log::new(rdc::cli::resolve::detect_color_mode(false));
        log.event(rdc::log::Action::Fail, &format!("{err:#}"));
        std::process::exit(1);
    }
}

/// Build the clap Command, downgrade its colour choice to `Never` when
/// any of the three standard disable triggers fires, then parse.
///
/// Three triggers, evaluated *before* clap renders anything:
/// 1. `--no-color` (or `--no-color=true`) anywhere in argv. We can't
///    rely on the parsed flag here because clap has already styled
///    `--help` / `-V` / error output by the time the flag is parsed.
/// 2. `NO_COLOR` env var set to any non-empty value
///    (<https://no-color.org>).
/// 3. stdout isn't a TTY.
///
/// Each trigger is independent; any one is sufficient.
fn parse_with_color_choice() -> Cli {
    let disable = no_color_flag_in_argv()
        || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || !std::io::stdout().is_terminal();
    let mut cmd = Cli::command();
    if disable {
        cmd = cmd.color(ColorChoice::Never);
    }
    let matches = cmd.get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// Tiny argv pre-scan for `--no-color`. Stops at the first standalone
/// `--` so it doesn't accidentally trip on a sub-argument that happens
/// to spell the flag.
fn no_color_flag_in_argv() -> bool {
    for arg in std::env::args_os().skip(1) {
        let s = arg.to_string_lossy();
        if s == "--" {
            return false;
        }
        if s == "--no-color" || s == "--no-color=true" {
            return true;
        }
    }
    false
}
