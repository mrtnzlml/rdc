use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ulid::Ulid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    Token,
    Password,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ConnectionStatus {
    Never,
    Ok,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: Ulid,
    pub name: String,
    pub slug: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: PathBuf,
    pub auth_kind: AuthKind,
    #[serde(default)]
    pub last_sync_unix: Option<i64>,
    #[serde(default = "default_status")]
    pub last_status: ConnectionStatus,
    #[serde(default)]
    pub file_count: u64,
}

fn default_status() -> ConnectionStatus {
    ConnectionStatus::Never
}
