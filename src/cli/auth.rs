//! `rdc auth <env> [--token <value> | --username <user>]` — set or refresh
//! an env's API token (per spec §6).
//!
//! The new token is validated by `GET /organizations/{org_id}` before being
//! written, so a typo is caught immediately. The token is written to
//! `secrets/<env>.secrets.json` with mode 0600 on Unix.
//!
//! Three ways to provide credentials:
//! - `--token <value>` flag (CI-friendly).
//! - `--username <user>` — reads password from stdin (or prompts via
//!   `inquire::Password` on TTY), calls `POST /v1/auth/login`, caches the
//!   issued token + computed expiry (162h from now).
//! - Neither — read a token from stdin (e.g. `read -s T && echo $T | rdc auth dev`).

use crate::api::{anyhow_has_status, RossumClient};
use crate::config::{EnvConfig, ProjectConfig};
use crate::log::Action;
use crate::paths::Paths;
use crate::secrets::LOGIN_TOKEN_LIFETIME_SECS;
use anyhow::{anyhow, Context, Result};
use std::io::IsTerminal;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn run(
    env: &str,
    token_arg: Option<String>,
    username_arg: Option<String>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let paths = Paths::for_env(&cwd, env);

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));

    if let Some(username) = username_arg {
        // --username flow: read password, exchange for a token via
        // POST /v1/auth/login, validate, and cache with computed expiry.
        let password = read_password_for_login()?;
        log.event(
            Action::Auth,
            &format!("logging in to {} as '{}'", env_cfg.api_base, username),
        );
        let token = crate::api::login(&env_cfg.api_base, &username, &password)
            .await
            .with_context(|| {
                format!("logging in to env '{env}' as '{username}'")
            })?;
        let org_name = validate_token(env_cfg, &token).await?;
        let expires_at = now_unix_secs().saturating_add(LOGIN_TOKEN_LIFETIME_SECS);
        crate::secrets::write_secrets_file(&cwd, env, &token, Some(expires_at))?;
        log.event(
            Action::Auth,
            &format!(
                "saved token to {} (org '{}')",
                paths.secrets_file().display(),
                org_name,
            ),
        );
        return Ok(());
    }

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
                    "no token provided; pass `--token <value>`, pipe a token via stdin, \
                     or use `--username <U>` to log in with credentials"
                ));
            }
            trimmed
        }
    };

    let org_name = validate_and_save_token(env_cfg, &cwd, env, &new_token).await?;

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

/// Read a password from stdin (non-TTY) or prompt via `inquire::Password`
/// (TTY). Trims trailing newline on the piped path. Used by the
/// `--username` flow in [`run`] to obtain the password without echoing
/// it to the screen.
fn read_password_for_login() -> Result<String> {
    if std::io::stdin().is_terminal() {
        use inquire::{Password, PasswordDisplayMode};
        let pw = Password::new("Password")
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .with_help_message("Ctrl+C to cancel")
            .prompt()
            .map_err(|e| anyhow!("password prompt failed: {e}"))?;
        if pw.is_empty() {
            return Err(anyhow!("empty password"));
        }
        Ok(pw)
    } else {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading password from stdin")?;
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            return Err(anyhow!(
                "no password provided; pipe one on stdin (`echo $PASS | rdc auth ...`) or run on a TTY"
            ));
        }
        Ok(trimmed)
    }
}

/// Validate a token by hitting `GET /organizations/<id>`. Returns the
/// organization's name on success. Extracted so both `--token` and
/// `--username` flows can validate without duplicating the request.
async fn validate_token(env_cfg: &EnvConfig, token: &str) -> Result<String> {
    let client = RossumClient::new(env_cfg.api_base.clone(), token.to_string())
        .context("constructing Rossum API client")?;
    let progress = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    progress.event(
        Action::Auth,
        &format!("validating token (GET /organizations/{})", env_cfg.org_id),
    );
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
    progress.event(
        Action::Auth,
        &format!("done validated against org '{}'", org.name),
    );
    Ok(org.name)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Validate `token` by calling `GET /organizations/<id>`. On success,
/// write it atomically to `<project_root>/secrets/<env>.secrets.json`
/// with mode 0600 (Unix) and return the organization name. On failure,
/// propagate.
///
/// `expires_at` is recorded as `None` — opaque tokens supplied via
/// `rdc auth` (CLI flag, stdin, or interactive prompt) carry no
/// machine-readable expiry, so we don't fabricate one. The `--username`
/// flow uses a separate path that records the computed 162h expiry.
pub(crate) async fn validate_and_save_token(
    env_cfg: &EnvConfig,
    project_root: &Path,
    env: &str,
    token: &str,
) -> Result<String> {
    let org_name = validate_token(env_cfg, token).await?;
    crate::secrets::write_secrets_file(project_root, env, token, None)?;
    Ok(org_name)
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
        match validate_and_save_token(env_cfg, &cwd, env, trimmed).await {
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

