use crate::config::ProjectConfig;
use anyhow::{Context, Result, anyhow};
use std::io::IsTerminal;

/// The picker's option list: every env defined in `cfg` except `exclude`,
/// sorted.
///
/// Lifted out of [`pick_env_excluding`] so the picker's content can be pinned
/// (`testdata/prompt_pins/env_picker.txt`): `inquire` draws its widget
/// straight to the terminal, leaving no sink a test could capture.
fn picker_options(cfg: &ProjectConfig, exclude: &[&str]) -> Vec<String> {
    let mut envs: Vec<String> = cfg
        .envs
        .keys()
        .filter(|n| !exclude.contains(&n.as_str()))
        .cloned()
        .collect();
    envs.sort();
    envs
}

/// Resolve an `env` argument for a command. If the user passed an explicit
/// value, return it. Otherwise load `rdc.toml` and present an interactive
/// picker. Non-TTY contexts (CI / piped) get a clear error pointing at
/// the available envs.
///
/// `usage` is the invocation to re-run with the env spelled out (e.g.
/// `rdc sync <env>`). It only reaches the non-TTY error, where naming the
/// command is the whole difference between a dead end and a next step: the
/// picker is unreachable there, so "env argument required" on its own leaves
/// the reader to guess where the argument goes.
pub fn pick_env(question: &str, usage: &str, env: Option<String>) -> Result<String> {
    pick_env_excluding(question, usage, env, &[])
}

/// Like [`pick_env`], but hides any envs in `exclude` from the picker.
/// Used by the one paired command, `rdc migrate <src> <tgt>`, so the second
/// pick can't be the same as the first.
pub fn pick_env_excluding(
    question: &str,
    usage: &str,
    env: Option<String>,
    exclude: &[&str],
) -> Result<String> {
    if let Some(e) = env {
        return Ok(e);
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    let envs = picker_options(&cfg, exclude);

    if envs.is_empty() {
        if exclude.is_empty() {
            return Err(anyhow!(
                "no environments defined in {}; run `rdc init` to add one",
                cfg_path.display()
            ));
        }
        return Err(anyhow!(
            "no other environment available besides {}",
            exclude.join(", ")
        ));
    }

    if !std::io::stdin().is_terminal() {
        // No terminal means no picker, so the env has to be typed. Name the
        // full invocation rather than the missing argument: the single-env
        // auto-select below is also unreachable here, and a reader who knows
        // only "an env is required" still has to find out where it goes.
        return Err(anyhow!(
            "env argument required without a terminal: run `{usage}`. \
             Defined envs: {}",
            envs.join(", ")
        ));
    }

    if envs.len() == 1 {
        let only = envs.into_iter().next().expect("len == 1");
        let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
        log.event(crate::log::Action::Info, &format!("using only defined env: {only}"));
        return Ok(only);
    }

    use inquire::Select;
    use inquire::error::InquireError;
    match Select::new(question, envs).prompt() {
        Ok(s) => Ok(s),
        Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
            Err(anyhow!("cancelled"))
        }
        Err(e) => Err(anyhow!("prompt failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::prompt_pin::{inquire_shape, pin};

    fn three_envs() -> ProjectConfig {
        toml::from_str(
            "[envs.prod]\napi_base = \"https://api.example.com/v1\"\norg_id = 1\n\n\
             [envs.dev]\napi_base = \"https://api.example.com/v1\"\norg_id = 2\n\n\
             [envs.test]\napi_base = \"https://api.example.com/v1\"\norg_id = 3\n",
        )
        .unwrap()
    }

    /// `inquire` prompt, so the pin is the question and the options rdc
    /// composes. Both forms are pinned: the plain picker, and the one
    /// `rdc migrate` uses, which hides the env already chosen as the source.
    #[test]
    fn env_picker_prompt_text_is_pinned() {
        let cfg = three_envs();
        let text = format!(
            "{}\n\n{}",
            inquire_shape("Which env to sync?", &picker_options(&cfg, &[])),
            inquire_shape(
                "Migrate to which env (target)?",
                &picker_options(&cfg, &["dev"]),
            ),
        );
        pin("env_picker", &text);
    }
}
