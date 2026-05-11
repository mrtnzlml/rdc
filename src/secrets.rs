use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Resolve the API token for an environment.
///
/// Resolution order:
/// 1. `RDC_TOKEN_<UPPER_ENV>` environment variable.
/// 2. `secrets/<env>.secrets.json` with shape `{ "api_token": "..." }`.
///
/// Returns an actionable error if neither source is present.
pub fn resolve_token(project_root: &Path, env: &str) -> Result<String> {
    resolve_token_from(project_root, env, |k| std::env::var(k).ok())
}

/// Inner form with an injectable env-getter. Lets tests cover the env-var
/// branch without mutating the process-wide environment, which is unsound
/// to do concurrently with other tests reading env vars.
fn resolve_token_from<F: Fn(&str) -> Option<String>>(
    project_root: &Path,
    env: &str,
    get_env: F,
) -> Result<String> {
    let env_var = format!("RDC_TOKEN_{}", env.to_uppercase());
    if let Some(t) = get_env(&env_var) {
        if !t.is_empty() {
            return Ok(t);
        }
    }

    let path = project_root.join("secrets").join(format!("{env}.secrets.json"));
    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        #[derive(Deserialize)]
        struct File {
            api_token: String,
        }
        let f: File = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        if f.api_token.is_empty() {
            return Err(anyhow!(
                "{} has empty api_token; set ${env_var} or fill in the file",
                path.display()
            ));
        }
        return Ok(f.api_token);
    }

    Err(anyhow!(
        "no token for env '{env}': set ${env_var} or write {}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn env_var_wins() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"from-file"}"#,
        )
        .unwrap();
        let token = resolve_token_from(dir.path(), "dev", |k| {
            (k == "RDC_TOKEN_DEV").then(|| "from-env".to_string())
        })
        .unwrap();
        assert_eq!(token, "from-env");
    }

    #[test]
    fn file_used_when_env_var_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"from-file"}"#,
        )
        .unwrap();
        let token = resolve_token_from(dir.path(), "dev", |_| None).unwrap();
        assert_eq!(token, "from-file");
    }

    #[test]
    fn env_var_with_empty_value_falls_through_to_file() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"from-file"}"#,
        )
        .unwrap();
        let token = resolve_token_from(dir.path(), "dev", |_| Some(String::new())).unwrap();
        assert_eq!(token, "from-file");
    }

    #[test]
    fn missing_token_errors_with_actionable_message() {
        let dir = TempDir::new().unwrap();
        let err = resolve_token_from(dir.path(), "unittest_c", |_| None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("RDC_TOKEN_UNITTEST_C"), "should mention env var: {msg}");
        assert!(msg.contains("secrets/unittest_c.secrets.json"), "should mention file path: {msg}");
    }
}
