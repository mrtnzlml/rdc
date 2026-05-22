//! `rdc auth <env> [--token <value>]` — set or refresh an env's API token
//! (per spec §6).
//!
//! The new token is validated by `GET /organizations/{org_id}` before being
//! written, so a typo is caught immediately. The token is written to
//! `secrets/<env>.secrets.json` with mode 0600 on Unix.
//!
//! Two ways to provide the token:
//! - `--token <value>` flag (CI-friendly).
//! - Read from stdin (e.g. `read -s T && echo $T | rdc auth dev`).
//!
//! No interactive prompt — that keeps the binary TTY-free and avoids a
//! new dep for password input.

use crate::api::{anyhow_has_status, RossumClient};
use crate::config::{EnvConfig, ProjectConfig};
use crate::log::Action;
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use std::io::IsTerminal;
use std::path::Path;

pub async fn run(env: &str, token_arg: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let paths = Paths::for_env(&cwd, env);

    let new_token = match token_arg {
        Some(t) => t,
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)
                .context("reading token from stdin")?;
            let trimmed = buf.trim().to_string();
            if trimmed.is_empty() {
                return Err(anyhow!(
                    "no token provided; pass `--token <value>` or pipe via stdin"
                ));
            }
            trimmed
        }
    };

    let org_name = validate_and_save_token(env_cfg, &paths.secrets_file(), &new_token).await?;

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    log.event(
        Action::Auth,
        &format!(
            "saved token to {} (org '{}')",
            paths.secrets_file().display(),
            org_name,
        ),
    );
    Ok(())
}

/// Validate `token` by calling `GET /organizations/<id>`. On success,
/// write it atomically to `secrets_path` with mode 0600 (Unix) and
/// return the organization name. On failure, propagate.
///
/// A short-lived ProgressLog surrounds the validation GET so the user
/// sees a spinner while rdc is waiting on the Rossum API.
pub(crate) async fn validate_and_save_token(
    env_cfg: &EnvConfig,
    secrets_path: &Path,
    token: &str,
) -> Result<String> {
    let client = RossumClient::new(env_cfg.api_base.clone(), token.to_string())
        .context("constructing Rossum API client")?;

    let progress = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    progress.event(Action::Auth, &format!("validating token (GET /organizations/{})", env_cfg.org_id));
    let org_result = client
        .get_organization(env_cfg.org_id, Some(progress.clone()))
        .await
        .with_context(|| {
            format!(
                "validating token against {}/organizations/{}",
                env_cfg.api_base, env_cfg.org_id
            )
        });
    let org = match org_result {
        Ok(o) => o,
        Err(e) => {
            progress.event(Action::Auth, "fail token validation");
            return Err(e);
        }
    };
    progress.event(Action::Auth, &format!("done validated against org '{}'", org.name));

    if let Some(parent) = secrets_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::json!({ "api_token": token });
    let mut bytes = serde_json::to_vec_pretty(&body).context("serializing token JSON")?;
    bytes.push(b'\n');
    write_atomic(secrets_path, &bytes)
        .with_context(|| format!("writing {}", secrets_path.display()))?;
    set_owner_only_read(secrets_path);
    Ok(org.name)
}

/// Interactive token refresh. Called when an API call returns 401: prompts
/// the user for a new token, validates it, saves it to the env's secrets
/// file, and returns. On non-TTY contexts, returns an error pointing at
/// `rdc auth <env>` instead of blocking.
pub async fn refresh_token_interactively(env: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "token for env '{env}' was rejected (401). \
             Re-run on a TTY to refresh interactively, or run \
             `rdc auth {env} --token <new-token>`."
        ));
    }

    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let paths = Paths::for_env(&cwd, env);
    let secrets_path = paths.secrets_file();

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    log.event(
        Action::Auth,
        &format!("token for env '{env}' rejected (401); refreshing"),
    );

    use inquire::error::InquireError;
    use inquire::{Password, PasswordDisplayMode};

    loop {
        let new_token = match Password::new("New API token")
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .with_help_message("Ctrl+C to cancel")
            .prompt()
        {
            Ok(s) => s,
            Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
                return Err(anyhow!("token refresh cancelled"));
            }
            Err(e) => return Err(anyhow!("token prompt failed: {e}")),
        };
        let trimmed = new_token.trim();
        if trimmed.is_empty() {
            log.event(Action::Auth, "empty input; paste the token, or Ctrl+C to abort");
            continue;
        }
        match validate_and_save_token(env_cfg, &secrets_path, trimmed).await {
            Ok(_org_name) => {
                log.event(
                    Action::Auth,
                    &format!("saved token to {}", secrets_path.display()),
                );
                return Ok(());
            }
            Err(e) if anyhow_has_status(&e, 401) => {
                log.event(Action::Auth, "rejected by server (401); try again, or Ctrl+C to abort");
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
fn set_owner_only_read(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only_read(_path: &std::path::Path) {}
