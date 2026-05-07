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

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};

pub async fn run(env: &str, token_arg: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)
        .with_context(|| format!("loading project config from {}", cfg_path.display()))?;
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
                    "no token provided — pass `--token <value>` or pipe via stdin"
                ));
            }
            trimmed
        }
    };

    // Validate by hitting the org endpoint.
    let client = RossumClient::new(env_cfg.api_base.clone(), new_token.clone())
        .context("constructing Rossum API client")?;
    let org = client
        .get_organization(env_cfg.org_id, None)
        .await
        .with_context(|| format!("validating token against {}/organizations/{}", env_cfg.api_base, env_cfg.org_id))?;

    // Write to secrets file (atomic).
    let secrets_path = paths.secrets_file();
    if let Some(parent) = secrets_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::json!({"api_token": new_token});
    let mut bytes = serde_json::to_vec_pretty(&body)
        .context("serializing token JSON")?;
    bytes.push(b'\n');
    write_atomic(&secrets_path, &bytes)
        .with_context(|| format!("writing {}", secrets_path.display()))?;

    // Best-effort restrict the file to the owner.
    set_owner_only_read(&secrets_path);

    println!(
        "Token written to {} (validated against org '{}', id {}).",
        secrets_path.display(),
        org.name,
        org.id,
    );
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_read(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only_read(_path: &std::path::Path) {}
