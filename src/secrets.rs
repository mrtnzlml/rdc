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
    let env_var = format!("RDC_TOKEN_{}", env.to_uppercase());
    if let Ok(t) = std::env::var(&env_var) {
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
        // SAFETY: env vars are process-global; tests run in parallel by default
        // but each uses a unique env name to avoid collisions.
        std::env::set_var("RDC_TOKEN_UNITTEST_A", "from-env");
        let token = resolve_token(dir.path(), "unittest_a").unwrap();
        assert_eq!(token, "from-env");
        std::env::remove_var("RDC_TOKEN_UNITTEST_A");
    }

    #[test]
    fn file_used_when_env_var_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/unittest_b.secrets.json"),
            r#"{"api_token":"from-file"}"#,
        )
        .unwrap();
        let token = resolve_token(dir.path(), "unittest_b").unwrap();
        assert_eq!(token, "from-file");
    }

    #[test]
    fn missing_token_errors_with_actionable_message() {
        let dir = TempDir::new().unwrap();
        let err = resolve_token(dir.path(), "unittest_c").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("RDC_TOKEN_UNITTEST_C"), "should mention env var: {msg}");
        assert!(msg.contains("secrets/unittest_c.secrets.json"), "should mention file path: {msg}");
    }
}
