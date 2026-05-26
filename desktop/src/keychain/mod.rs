use anyhow::Result;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

pub mod fake;
#[cfg(target_os = "macos")]
pub mod macos;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEntry {
    pub token: String,
    /// `None` for raw tokens (token-mode Connections) where the app has
    /// no derived expiry. `Some(unix_seconds)` for tokens issued via
    /// `/v1/auth/login` where the app assumes the documented 162 h
    /// default lifetime.
    pub expires_at_unix: Option<i64>,
}

pub trait Keychain: Send + Sync {
    /// Inserts or replaces the token entry for `id`. Last write wins.
    fn put_token(&self, id: Ulid, entry: &TokenEntry) -> Result<()>;
    fn get_token(&self, id: Ulid) -> Result<Option<TokenEntry>>;

    /// Inserts or replaces the username + password for `id`. Last write wins.
    fn put_credentials(&self, id: Ulid, username: &str, password: &str) -> Result<()>;
    fn get_credentials(&self, id: Ulid) -> Result<Option<(String, String)>>;

    fn delete_all(&self, id: Ulid) -> Result<()>;
}
