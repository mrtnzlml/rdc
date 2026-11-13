//! `rdc auth <env> [--token <value> | --username <user> | --browser | --no-browser]` — set or refresh
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
//! - `--browser` — take the token in a one-time local web form (see
//!   [`crate::cli::auth_browser`]); for callers with no terminal.
//! - None of these — see `pick_source`: read the token from stdin when it
//!   is a pipe or a file (`rdc auth dev < token.txt`, CI); otherwise open the
//!   browser form, or show a masked prompt on a terminal when no browser can
//!   be used (SSH, Linux without a display) or `--no-browser` is passed.

use crate::api::{anyhow_has_status, RossumClient};
use crate::config::{EnvConfig, ProjectConfig};
use crate::log::Action;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};
use std::io::IsTerminal;
use std::path::Path;

pub async fn run(
    env: &str,
    token_arg: Option<String>,
    username_arg: Option<String>,
    browser: bool,
    no_browser: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    let env_cfg = cfg.env_or_err(env)?;
    let paths = Paths::for_env(&cwd, env);

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());

    let source = match (&token_arg, &username_arg, browser) {
        (None, None, false) => pick_source(
            stdin_is_pipe_or_file(),
            std::io::stdin().is_terminal(),
            no_browser,
            browser_usable(|k| std::env::var(k).ok()),
        ),
        _ => Source::Stdin,
    };
    if source == Source::Nothing {
        return Err(anyhow!(
            "no token provided, and rdc cannot ask for one: stdin is neither a terminal \
             nor a pipe, and {}; pass `--token <value>`, pipe a token via stdin, \
             use `--username <U>`, or set $RDC_TOKEN_<ENV>",
            if no_browser { "--no-browser was passed" } else { "no browser can be opened (SSH session or no display)" }
        ));
    }

    if browser || source == Source::Browser {
        let org_name = crate::cli::auth_browser::run(env_cfg, &cwd, env).await?;
        log.event(
            Action::Auth,
            &format!("saved token to {} (org '{}')", paths.secrets_file().display(), org_name),
        );
        return Ok(());
    }

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
        crate::secrets::write_login_token(&cwd, env, &token)?;
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
        None if source == Source::Prompt => read_token_for_auth(env)?,
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)
                .context("reading token from stdin")?;
            let trimmed = buf.trim().to_string();
            if trimmed.is_empty() {
                return Err(anyhow!(
                    "no token provided; pass `--token <value>`, pipe a token via stdin, \
                     use `--username <U>` to log in with credentials, \
                     or use `--browser` to enter the token in a local web form"
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

/// Where `rdc auth <env>` with no credential flag takes the token from.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    Stdin,
    Browser,
    Prompt,
    Nothing,
}

/// Piped or redirected stdin always wins, so scripts and CI keep working.
/// Otherwise the browser form, unless it cannot be used or `--no-browser`
/// was passed; then a terminal prompt, if there is a terminal.
fn pick_source(stdin_piped: bool, stdin_tty: bool, no_browser: bool, browser_ok: bool) -> Source {
    if stdin_piped {
        Source::Stdin
    } else if !no_browser && browser_ok {
        Source::Browser
    } else if stdin_tty {
        Source::Prompt
    } else {
        Source::Nothing
    }
}

/// False over SSH (the form would open on the remote machine's 127.0.0.1)
/// and on Linux and other Unixes without a display. `var` reads one
/// environment variable.
fn browser_usable(var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |k: &str| var(k).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_CLIENT") || set("SSH_TTY") {
        return false;
    }
    cfg!(any(target_os = "macos", windows)) || set("DISPLAY") || set("WAYLAND_DISPLAY")
}

/// True when stdin is a pipe or a regular file, i.e. someone sent a token.
/// A terminal, `/dev/null` (Claude Code's `!` commands) or a closed stdin
/// is false.
#[cfg(unix)]
fn stdin_is_pipe_or_file() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileTypeExt;
    let Ok(fd) = std::io::stdin().as_fd().try_clone_to_owned() else {
        return false;
    };
    std::fs::File::from(fd)
        .metadata()
        .map(|m| m.file_type().is_fifo() || m.file_type().is_file())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn stdin_is_pipe_or_file() -> bool {
    !std::io::stdin().is_terminal()
}

/// Masked terminal prompt for `rdc auth <env>` when the browser form is
/// not used.
fn read_token_for_auth(env: &str) -> Result<String> {
    use inquire::{Password, PasswordDisplayMode};
    let token = Password::new(&format!("API token for env '{env}'"))
        .with_display_mode(PasswordDisplayMode::Masked)
        .without_confirmation()
        .with_help_message("Ctrl+C to cancel")
        .prompt()
        .map_err(|e| anyhow!("token prompt failed: {e}"))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(anyhow!("empty token"));
    }
    Ok(token)
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
    let progress = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
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

/// Recover from a 401 on a token call.
///
/// Three paths, tried in order:
/// - **Non-TTY with creds set:** `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are
///   exchanged for a fresh token via `POST /v1/auth/login`. The new
///   token and computed expiry land in `secrets/<env>.secrets.json`.
///   Returns `Ok(())` silently — `with_401_retry` then retries the
///   failed operation once.
/// - **TTY:** prompts for a new token via `inquire::Password` (or, under
///   `--watch`, through the stdin coordinator — see `read_new_token`),
///   validates it, and saves it (same path as `rdc auth --token`).
/// - **Non-TTY without creds:** actionable error naming the three
///   recovery options (env vars, `rdc auth`, TTY re-run).
pub async fn refresh_token_for_401(env: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        // Non-TTY: try silent re-login from RDC_USER_<ENV> + RDC_PASS_<ENV>
        // before erroring. This is the CI / cron path.
        let user_var = crate::secrets::env_var_for(env, "USER");
        let pass_var = crate::secrets::env_var_for(env, "PASS");
        let user_opt = std::env::var(&user_var).ok().filter(|s| !s.is_empty());
        let pass_opt = std::env::var(&pass_var).ok().filter(|s| !s.is_empty());
        if let (Some(username), Some(password)) = (user_opt, pass_opt) {
            let cwd = std::env::current_dir().context("getting current directory")?;
            let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
            let env_cfg = cfg.env_or_err(env)?;
            let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
            log.event(
                Action::Auth,
                &format!("token for env '{env}' rejected (401); silent re-login from ${user_var}"),
            );
            let token = crate::api::login(&env_cfg.api_base, &username, &password)
                .await
                .with_context(|| format!("silent re-login for env '{env}'"))?;
            crate::secrets::write_login_token(&cwd, env, &token)?;
            log.event(Action::Auth, &format!("silent re-login OK for env '{env}'"));
            return Ok(());
        }
        return Err(anyhow!(
            "token for env '{env}' was rejected (401). \
             Re-run on a TTY to refresh interactively, set ${user_var} + ${pass_var} \
             for silent re-login, or run `rdc auth {env} --token <new-token>`."
        ));
    }

    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let env_cfg = cfg.env_or_err(env)?;
    let paths = Paths::for_env(&cwd, env);
    let secrets_path = paths.secrets_file();

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
    log.event(
        Action::Auth,
        &format!("token for env '{env}' rejected (401); refreshing"),
    );

    loop {
        let Some(new_token) = read_new_token().await? else {
            return Err(anyhow!("token refresh cancelled"));
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

/// Read one token for [`refresh_token_for_401`]. `Ok(None)` = cancelled.
///
/// Under `rdc sync --watch` the watch reader owns stdin (`cli::stdin_coord`),
/// and inquire reading the terminal directly races it: the reader can take
/// the whole pasted line, and the prompt never sees it (issue #2). So while
/// the coordinator is active the token is read as a coordinated line, with
/// terminal echo off. An empty line cancels there, because Ctrl-C does not
/// interrupt a coordinated read.
async fn read_new_token() -> Result<Option<String>> {
    if crate::cli::stdin_coord::is_active() {
        eprint!("? New API token (input hidden; empty line cancels): ");
        let _echo = EchoOff::new();
        let line = crate::cli::stdin_coord::read_line_coordinated().await?;
        eprintln!();
        return Ok(line.filter(|l| !l.trim().is_empty()));
    }

    use inquire::error::InquireError;
    use inquire::{Password, PasswordDisplayMode};
    match Password::new("New API token")
        .with_display_mode(PasswordDisplayMode::Masked)
        .without_confirmation()
        .with_help_message("Ctrl+C to cancel")
        .prompt()
    {
        Ok(s) => Ok(Some(s)),
        Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(anyhow!("token prompt failed: {e}")),
    }
}

/// Turns terminal echo off until dropped, so a token typed into a
/// coordinated read does not appear on screen. Uses `stty` on the inherited
/// stdin rather than termios, to keep `unsafe` out of the crate. A no-op
/// where `stty` is missing or fails (Windows): the input is then visible.
struct EchoOff(Option<String>);

impl EchoOff {
    fn new() -> Self {
        use std::process::{Command, Stdio};
        let saved = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if saved.is_some() {
            let _ = Command::new("stty").arg("-echo").stdin(Stdio::inherit()).status();
        }
        EchoOff(saved)
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            let _ = std::process::Command::new("stty")
                .arg(saved)
                .stdin(std::process::Stdio::inherit())
                .status();
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_stdin_wins_over_everything() {
        for (tty, no_browser, ok) in [(false, false, true), (true, true, false), (false, true, true)] {
            assert_eq!(pick_source(true, tty, no_browser, ok), Source::Stdin);
        }
    }

    #[test]
    fn browser_is_the_default_without_piped_stdin() {
        assert_eq!(pick_source(false, true, false, true), Source::Browser);
        assert_eq!(pick_source(false, false, false, true), Source::Browser);
    }

    #[test]
    fn terminal_prompt_without_a_usable_browser_or_with_no_browser() {
        assert_eq!(pick_source(false, true, false, false), Source::Prompt);
        assert_eq!(pick_source(false, true, true, true), Source::Prompt);
    }

    #[test]
    fn nothing_without_terminal_or_browser() {
        assert_eq!(pick_source(false, false, false, false), Source::Nothing);
        assert_eq!(pick_source(false, false, true, true), Source::Nothing);
    }

    #[test]
    fn ssh_disables_the_browser() {
        for k in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
            let env = |name: &str| (name == k || name == "DISPLAY").then(|| "x".to_string());
            assert!(!browser_usable(env), "{k}");
        }
    }

    #[test]
    fn display_decides_on_linux() {
        let display = |name: &str| (name == "DISPLAY").then(|| "x".to_string());
        assert!(browser_usable(display));
        let none = |_: &str| None;
        assert_eq!(browser_usable(none), cfg!(any(target_os = "macos", windows)));
    }
}
