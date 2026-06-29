//! FFI-facing connection management: list/add/edit/validate. Each function
//! re-uses rdc's own file + credential helpers; nothing here reimplements
//! the on-disk format.

use crate::discover::{self, AuthKindRaw, Connection};
use crate::error::{map_err, op, FfiError};
use std::collections::HashSet;
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

#[derive(Debug, Clone, uniffi::Record)]
pub struct AddConnectionInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct EditCredentialsInput {
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// List every Connection under `parent`. Non-project folders are skipped.
#[uniffi::export]
pub fn list_connections(parent: String) -> Vec<ConnectionSummary> {
    discover::scan(Path::new(&parent))
        .iter()
        .map(ConnectionSummary::from)
        .collect()
}

/// Write credentials via rdc's own helpers. Empty strings are rejected
/// the same way the original Tauri command did.
fn write_credentials(
    folder: &Path,
    auth: AuthKind,
    token: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), FfiError> {
    match auth {
        AuthKind::Token => {
            let t = token
                .filter(|s| !s.is_empty())
                .ok_or_else(|| op("Token is required.".into()))?;
            rdc::secrets::write_secrets_file(folder, "main", t, None).map_err(map_err)?;
        }
        AuthKind::Password => {
            let u = username
                .filter(|s| !s.is_empty())
                .ok_or_else(|| op("Username is required.".into()))?;
            let p = password
                .filter(|s| !s.is_empty())
                .ok_or_else(|| op("Password is required.".into()))?;
            rdc::secrets::save_password_credentials(folder, "main", u, p).map_err(map_err)?;
        }
    }
    Ok(())
}

/// Create a new Connection: write `rdc.toml` + secrets under a unique slug.
#[uniffi::export]
pub fn add_connection(
    parent: String,
    input: AddConnectionInput,
) -> Result<ConnectionSummary, FfiError> {
    let parent = std::path::PathBuf::from(parent);
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let slug = rdc::slug::slugify_unique(&input.name, &used);
    let folder = parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| op(format!("creating folder: {e}")))?;

    let api_base = input.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!(
        "[envs.main]\napi_base = \"{api_base}\"\norg_id = {}\n",
        input.org_id
    );
    std::fs::write(folder.join("rdc.toml"), rdc_toml)
        .map_err(|e| op(format!("writing rdc.toml: {e}")))?;

    write_credentials(
        &folder,
        input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )?;

    discover::find(&parent, &slug)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| op("Connection not found after add".into()))
}

/// Replace a Connection's stored credentials. Existing secrets are wiped
/// first so a token↔password mode flip leaves no stale fields behind.
#[uniffi::export]
pub fn edit_credentials(folder: String, input: EditCredentialsInput) -> Result<(), FfiError> {
    let folder = std::path::PathBuf::from(folder);
    if !folder.join("rdc.toml").exists() {
        return Err(op("Connection not found".into()));
    }
    let _ = std::fs::remove_file(folder.join("secrets/main.secrets.json"));
    write_credentials(
        &folder,
        input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )
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

    #[test]
    fn add_connection_writes_files_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let input = AddConnectionInput {
            name: "Acme Prod".into(),
            api_base: "https://example.test/api/v1/".into(),
            org_id: 42,
            auth_kind: AuthKind::Token,
            token: Some("tok-123".into()),
            username: None,
            password: None,
        };
        let summary = add_connection(tmp.path().display().to_string(), input).unwrap();
        assert_eq!(summary.org_id, 42);
        assert_eq!(summary.api_base, "https://example.test/api/v1"); // trailing slash trimmed
        let folder = tmp.path().join(&summary.id);
        assert!(folder.join("rdc.toml").exists());
        assert!(folder.join("secrets/main.secrets.json").exists());
    }

    #[test]
    fn add_connection_rejects_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        let input = AddConnectionInput {
            name: "x".into(),
            api_base: "https://example.test/api/v1".into(),
            org_id: 1,
            auth_kind: AuthKind::Token,
            token: Some(String::new()),
            username: None,
            password: None,
        };
        let err = add_connection(tmp.path().display().to_string(), input).unwrap_err();
        assert!(format!("{err}").contains("Token is required"));
    }

    #[test]
    fn edit_credentials_flips_token_to_password() {
        let tmp = tempfile::tempdir().unwrap();
        let added = add_connection(
            tmp.path().display().to_string(),
            AddConnectionInput {
                name: "acme".into(),
                api_base: "https://example.test/api/v1".into(),
                org_id: 1,
                auth_kind: AuthKind::Token,
                token: Some("tok".into()),
                username: None,
                password: None,
            },
        )
        .unwrap();

        edit_credentials(
            added.folder.clone(),
            EditCredentialsInput {
                auth_kind: AuthKind::Password,
                token: None,
                username: Some("user".into()),
                password: Some("pass".into()),
            },
        )
        .unwrap();

        // Discovery now reports password auth.
        let listed = list_connections(tmp.path().display().to_string());
        assert_eq!(listed[0].auth_kind, AuthKind::Password);
    }
}
