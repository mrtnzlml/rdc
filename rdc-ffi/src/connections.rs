//! FFI-facing connection management: list/add/edit/validate. Each function
//! re-uses rdc's own file + credential helpers; nothing here reimplements
//! the on-disk format.

use crate::discover::{self, AuthKindRaw, Connection};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AuthKind {
    Token,
    Password,
}

impl From<AuthKindRaw> for AuthKind {
    fn from(r: AuthKindRaw) -> Self {
        match r {
            AuthKindRaw::Token => AuthKind::Token,
            AuthKindRaw::Password => AuthKind::Password,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: String,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl From<&Connection> for ConnectionSummary {
    fn from(c: &Connection) -> Self {
        Self {
            id: c.id().to_string(),
            name: c.name().to_string(),
            api_base: c.api_base.clone(),
            org_id: c.org_id,
            folder: c.folder.display().to_string(),
            auth_kind: c.auth_kind.into(),
            last_sync_unix: c.last_sync_unix,
            file_count: c.file_count,
        }
    }
}

/// List every Connection under `parent`. Non-project folders are skipped.
#[uniffi::export]
pub fn list_connections(parent: String) -> Vec<ConnectionSummary> {
    discover::scan(Path::new(&parent))
        .iter()
        .map(ConnectionSummary::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(parent: &Path, name: &str) {
        let folder = parent.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("rdc.toml"),
            "[envs.main]\napi_base = \"https://example.test/api/v1\"\norg_id = 5\n",
        )
        .unwrap();
    }

    #[test]
    fn list_connections_maps_summaries() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), "alpha");
        let out = list_connections(tmp.path().display().to_string());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "alpha");
        assert_eq!(out[0].id, "alpha");
        assert_eq!(out[0].org_id, 5);
        assert_eq!(out[0].auth_kind, AuthKind::Token);
        assert_eq!(out[0].last_sync_unix, None);
    }
}
