use crate::config::ProjectConfig;
use anyhow::{Context, Result, anyhow};
use std::io::IsTerminal;

/// Resolve an `env` argument for a command. If the user passed an explicit
/// value, return it. Otherwise load `rdc.toml` and present an interactive
/// picker. Non-TTY contexts (CI / piped) get a clear error pointing at
/// the available envs.
pub fn pick_env(question: &str, env: Option<String>) -> Result<String> {
    pick_env_excluding(question, env, &[])
}

/// Like [`pick_env`], but hides any envs in `exclude` from the picker.
/// Used by paired commands (e.g. `rdc deploy <src> <tgt>`) so the second
/// pick can't be the same as the first.
pub fn pick_env_excluding(
    question: &str,
    env: Option<String>,
    exclude: &[&str],
) -> Result<String> {
    if let Some(e) = env {
        return Ok(e);
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    let mut envs: Vec<String> = cfg
        .envs
        .keys()
        .filter(|n| !exclude.iter().any(|x| *x == n.as_str()))
        .cloned()
        .collect();
    envs.sort();

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
        return Err(anyhow!(
            "env argument required when stdin is not a TTY. \
             Defined envs: {}",
            envs.join(", ")
        ));
    }

    if envs.len() == 1 {
        let only = envs.into_iter().next().expect("len == 1");
        let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
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
