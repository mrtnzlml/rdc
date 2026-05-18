use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Compute the environment-variable name rdc looks at for an env's API
/// token. POSIX env-var identifiers are `[A-Za-z_][A-Za-z0-9_]*`, but
/// rdc env names accept `-` and `_` (e.g. `dev-ap`, `prod_eu`). To
/// produce a name the shell can actually export, every non-alphanumeric
/// character in the env name is mapped to `_` (and the whole thing
/// uppercased). So:
///
/// | env name   | env-var               |
/// |------------|-----------------------|
/// | `dev`      | `RDC_TOKEN_DEV`       |
/// | `dev-ap`   | `RDC_TOKEN_DEV_AP`    |
/// | `dev_ap`   | `RDC_TOKEN_DEV_AP`    |
/// | `prod-EU`  | `RDC_TOKEN_PROD_EU`   |
///
/// Note the collision in row 2 vs 3: `dev-ap` and `dev_ap` both
/// normalize to the same variable. The `rdc init` wizard refuses to
/// add a new env whose name would collide with an existing one's
/// normalized form, so this can't happen by accident inside a single
/// project — and the secrets-file path (`secrets/<env>.secrets.json`)
/// stays exact and unambiguous either way.
pub fn env_token_var(env: &str) -> String {
    let mut out = String::with_capacity("RDC_TOKEN_".len() + env.len());
    out.push_str("RDC_TOKEN_");
    for c in env.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out
}

/// Resolve the API token for an environment.
///
/// Resolution order:
/// 1. The environment variable returned by [`env_token_var`] —
///    `RDC_TOKEN_DEV` for env `dev`, `RDC_TOKEN_DEV_AP` for `dev-ap`.
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
    let env_var = env_token_var(env);
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

/// Per-env, per-hook secret values that ship to the Rossum API in the
/// `secrets` top-level field of `POST /hooks/` and `PATCH /hooks/<id>`.
///
/// Stored at `secrets/<env>.hook-secrets.json` — gitignored alongside
/// the API-token file (the project-wide `/secrets` rule in `.gitignore`
/// already covers it). Shape on disk:
///
/// ```json
/// {
///   "hooks": {
///     "master-data-hub": { "mdh_api_token": "abc..." },
///     "notify-slack":    { "signing_secret": "xyz..." }
///   }
/// }
/// ```
///
/// Values are never read back from the server (`GET /hooks/<id>` does
/// not return `secrets`; `GET /hooks/<id>/secrets_keys` exposes the
/// list of key names only). This struct is the canonical local source.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HookSecrets {
    /// slug → (key → value).
    by_slug: BTreeMap<String, BTreeMap<String, String>>,
    /// Path the values came from. `None` when the file did not exist —
    /// distinguishes "no file" from "file with empty hooks map".
    source: Option<PathBuf>,
}

impl HookSecrets {
    /// Look up the K/V map for a hook slug. `None` when the slug has no
    /// entry; callers should treat that the same as "no secrets to send".
    pub fn for_slug(&self, slug: &str) -> Option<&BTreeMap<String, String>> {
        self.by_slug.get(slug)
    }

    /// All slugs present in the local secrets file. Used to detect
    /// typo slugs that don't match any hook on push.
    pub fn slugs(&self) -> impl Iterator<Item = &String> {
        self.by_slug.keys()
    }

    /// True if the file existed on disk (even if it had no hooks).
    pub fn was_loaded(&self) -> bool {
        self.source.is_some()
    }
}

/// Path resolver — exposed so callers can quote it in error messages
/// without duplicating the convention.
pub fn hook_secrets_path(project_root: &Path, env: &str) -> PathBuf {
    project_root
        .join("secrets")
        .join(format!("{env}.hook-secrets.json"))
}

/// Load `secrets/<env>.hook-secrets.json`. Returns an empty
/// `HookSecrets` when the file does not exist — callers always get a
/// usable value back, so injection sites don't need to branch on
/// "file present?". Malformed JSON propagates as a hard error so
/// typos surface loudly instead of silently dropping secrets on push.
pub fn load_hook_secrets(project_root: &Path, env: &str) -> Result<HookSecrets> {
    let path = hook_secrets_path(project_root, env);
    if !path.exists() {
        return Ok(HookSecrets::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(HookSecrets {
            by_slug: BTreeMap::new(),
            source: Some(path),
        });
    }
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        hooks: BTreeMap<String, BTreeMap<String, String>>,
    }
    let f: File = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(HookSecrets {
        by_slug: f.hooks,
        source: Some(path),
    })
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

    #[test]
    fn env_token_var_uppercases_and_keeps_alphanumerics() {
        assert_eq!(env_token_var("dev"), "RDC_TOKEN_DEV");
        assert_eq!(env_token_var("PROD"), "RDC_TOKEN_PROD");
        assert_eq!(env_token_var("staging42"), "RDC_TOKEN_STAGING42");
    }

    #[test]
    fn env_token_var_maps_hyphen_to_underscore() {
        // The motivating case: real env names like `dev-ap` need to
        // produce a valid POSIX env-var identifier.
        assert_eq!(env_token_var("dev-ap"), "RDC_TOKEN_DEV_AP");
        assert_eq!(env_token_var("prod-eu-west-1"), "RDC_TOKEN_PROD_EU_WEST_1");
    }

    #[test]
    fn env_token_var_preserves_existing_underscores() {
        assert_eq!(env_token_var("dev_ap"), "RDC_TOKEN_DEV_AP");
    }

    #[test]
    fn env_token_var_collision_between_hyphen_and_underscore_is_documented() {
        // This is the known footgun; the init wizard refuses the
        // second one of these pairs to prevent it inside a project.
        // Documented here so a future change can't silently break it.
        assert_eq!(env_token_var("dev-ap"), env_token_var("dev_ap"));
    }

    #[test]
    fn resolve_token_uses_normalized_env_var_for_hyphenated_env() {
        // `dev-ap` env must resolve via `$RDC_TOKEN_DEV_AP`, not the
        // invalid `$RDC_TOKEN_DEV-AP` (which no shell can export).
        let dir = TempDir::new().unwrap();
        let token = resolve_token_from(dir.path(), "dev-ap", |k| {
            (k == "RDC_TOKEN_DEV_AP").then(|| "from-env".to_string())
        })
        .unwrap();
        assert_eq!(token, "from-env");
    }

    #[test]
    fn resolve_token_missing_message_quotes_normalized_var_name() {
        let dir = TempDir::new().unwrap();
        let err = resolve_token_from(dir.path(), "dev-ap", |_| None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RDC_TOKEN_DEV_AP"),
            "error must point at the actual env-var name: {msg}"
        );
        assert!(
            !msg.contains("RDC_TOKEN_DEV-AP"),
            "error must NOT mention the invalid hyphenated form: {msg}"
        );
    }

    #[test]
    fn hook_secrets_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let s = load_hook_secrets(dir.path(), "dev").unwrap();
        assert!(s.for_slug("anything").is_none());
        assert_eq!(s.slugs().count(), 0);
        assert!(!s.was_loaded(), "missing file should report not-loaded");
    }

    #[test]
    fn hook_secrets_loads_populated_file() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.hook-secrets.json"),
            r#"{
              "hooks": {
                "master-data-hub": { "mdh_api_token": "abc", "mdh_endpoint": "https://x" },
                "notify-slack":    { "signing_secret": "xyz" }
              }
            }"#,
        )
        .unwrap();
        let s = load_hook_secrets(dir.path(), "dev").unwrap();
        let mdh = s.for_slug("master-data-hub").expect("mdh entry");
        assert_eq!(mdh.get("mdh_api_token").map(String::as_str), Some("abc"));
        assert_eq!(mdh.get("mdh_endpoint").map(String::as_str), Some("https://x"));
        let slack = s.for_slug("notify-slack").expect("slack entry");
        assert_eq!(slack.get("signing_secret").map(String::as_str), Some("xyz"));
        assert!(s.for_slug("unrelated").is_none());
        let slugs: Vec<&String> = s.slugs().collect();
        assert_eq!(slugs.len(), 2, "should report both slugs (sorted)");
        assert!(s.was_loaded());
    }

    #[test]
    fn hook_secrets_empty_file_is_loaded_but_empty() {
        // An empty file is a valid "I have a project-level secrets
        // file but no values yet" state — distinct from "file missing"
        // because the user has signalled intent by creating it.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/dev.hook-secrets.json"), "").unwrap();
        let s = load_hook_secrets(dir.path(), "dev").unwrap();
        assert_eq!(s.slugs().count(), 0);
        assert!(s.was_loaded(), "empty file is still loaded, just has no entries");
    }

    #[test]
    fn hook_secrets_malformed_json_errors_loudly() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.hook-secrets.json"),
            "{ not valid json",
        )
        .unwrap();
        let err = load_hook_secrets(dir.path(), "dev").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("secrets/dev.hook-secrets.json"), "must surface path: {msg}");
        assert!(msg.contains("parsing") || msg.contains("expected"), "must surface parse error: {msg}");
    }

    #[test]
    fn hook_secrets_missing_hooks_key_is_treated_as_empty() {
        // `{}` (no top-level `hooks` key) is a benign state. Don't reject —
        // serde default builds an empty BTreeMap.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/dev.hook-secrets.json"), "{}").unwrap();
        let s = load_hook_secrets(dir.path(), "dev").unwrap();
        assert_eq!(s.slugs().count(), 0);
        assert!(s.was_loaded());
    }

    #[test]
    fn hook_secrets_path_uses_per_env_filename() {
        let dir = TempDir::new().unwrap();
        let p = hook_secrets_path(dir.path(), "prod");
        assert_eq!(
            p,
            dir.path().join("secrets").join("prod.hook-secrets.json")
        );
    }
}
