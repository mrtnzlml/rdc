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
        let log = rdc::log::Log::new(rdc::cli::resolve::detect_color_mode());
        log.event(rdc::log::Action::Fail, &format!("{err:#}"));
        std::process::exit(1);
    }
}

/// Build the clap Command, downgrade its colour choice to `Never` when
/// either standard disable trigger fires, then parse.
///
/// Two triggers, evaluated *before* clap renders anything:
/// 1. `NO_COLOR` env var set to any non-empty value
///    (<https://no-color.org>).
/// 2. stdout isn't a TTY.
///
/// Each trigger is independent; either one is sufficient.
fn parse_with_color_choice() -> Cli {
    let disable = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || !std::io::stdout().is_terminal();
    let mut cmd = Cli::command();
    if disable {
        cmd = cmd.color(ColorChoice::Never);
    }
    let matches = cmd.get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}
