use crate::connection::{AuthKind, Connection};
use crate::keychain::{Keychain, TokenEntry};
use serde::Deserialize;
use thiserror::Error;

const TOKEN_LIFETIME_SECS: i64 = 162 * 3600;
const EXPIRY_SKEW_SECS: i64 = 60;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("Sign-in expired. Edit credentials to enter a new token.")]
    SignInExpired,
    #[error("No credentials stored for this Connection. Edit credentials to set them.")]
    Missing,
    #[error("Wrong username or password.")]
    BadPassword,
    #[error("Couldn't reach Rossum at {0}. Check your internet connection.")]
    Network(String),
    #[error("Rossum returned an unexpected error: {0}")]
    Other(String),
}

#[derive(Debug, Clone)]
pub struct TokenSource {
    pub token: String,
    pub refreshed: bool,
}

pub async fn resolve_token_for_sync<K: Keychain + ?Sized>(
    conn: &Connection,
    kc: &K,
) -> Result<TokenSource, ResolveError> {
    let now = now_unix();

    // Step 1: try cached token.
    if let Some(entry) = kc.get_token(conn.id).map_err(io_err)? {
        match entry.expires_at_unix {
            None => return Ok(TokenSource { token: entry.token, refreshed: false }),
            Some(exp) if exp.saturating_sub(EXPIRY_SKEW_SECS) > now => {
                return Ok(TokenSource { token: entry.token, refreshed: false });
            }
            _ => {} // expired; fall through
        }
    }

    // Step 2: fall back per auth kind.
    match conn.auth_kind {
        AuthKind::Token => Err(ResolveError::SignInExpired),
        AuthKind::Password => {
            let creds = kc
                .get_credentials(conn.id)
                .map_err(io_err)?
                .ok_or(ResolveError::Missing)?;
            let token = login(&conn.api_base, &creds.0, &creds.1).await?;
            let entry = TokenEntry {
                token: token.clone(),
                expires_at_unix: Some(now + TOKEN_LIFETIME_SECS),
            };
            kc.put_token(conn.id, &entry).map_err(io_err)?;
            Ok(TokenSource { token, refreshed: true })
        }
    }
}

#[derive(Deserialize)]
struct LoginResponse {
    key: String,
    #[allow(dead_code)]
    domain: Option<String>,
}

pub(crate) async fn login(api_base: &str, username: &str, password: &str) -> Result<String, ResolveError> {
    let url = format!("{}/auth/login", api_base.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .map_err(|e| ResolveError::Network(e.to_string()))?;
    if resp.status() == 401 {
        return Err(ResolveError::BadPassword);
    }
    if !resp.status().is_success() {
        return Err(ResolveError::Other(format!("{} {}", resp.status(), url)));
    }
    let body: LoginResponse = resp
        .json()
        .await
        .map_err(|e| ResolveError::Other(format!("parsing login response: {e}")))?;
    Ok(body.key)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn io_err(e: anyhow::Error) -> ResolveError {
    ResolveError::Other(format!("{e:#}"))
}
