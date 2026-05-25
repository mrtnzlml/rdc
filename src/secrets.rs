use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Compute the environment-variable name rdc looks at for a per-env
/// credential field. `suffix` is `TOKEN`, `USER`, or `PASS`.
///
/// POSIX env-var identifiers are `[A-Za-z_][A-Za-z0-9_]*`, but
/// rdc env names accept `-` and `_` (e.g. `dev-ap`). To produce a
/// name the shell can actually export, every non-alphanumeric
/// character in the env name is mapped to `_` and the whole thing
/// uppercased.
///
/// | env name   | suffix  | env-var               |
/// |------------|---------|-----------------------|
/// | `dev`      | `TOKEN` | `RDC_TOKEN_DEV`       |
/// | `dev-ap`   | `USER`  | `RDC_USER_DEV_AP`     |
/// | `prod_eu`  | `PASS`  | `RDC_PASS_PROD_EU`    |
///
/// The hyphen-vs-underscore collision documented for `env_token_var`
/// still applies (e.g. `dev-ap` and `dev_ap` normalize to the same
/// suffix). The `rdc init` wizard prevents this collision at project
/// creation time.
pub fn env_var_for(env: &str, suffix: &str) -> String {
    let mut out = String::with_capacity("RDC_".len() + suffix.len() + 1 + env.len());
    out.push_str("RDC_");
    out.push_str(suffix);
    out.push('_');
    for c in env.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out
}

/// Outcome of synchronously inspecting the per-env credential
/// configuration (env vars + on-disk secrets file). The async
/// [`resolve_token`] consumes this enum and performs I/O (HTTP login,
/// cache write) when needed.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenLookup {
    /// A token is ready to use.
    Cached {
        token: String,
        expires_at: Option<u64>,
    },
    /// `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are both set and the cache
    /// is missing/expired. Caller (async `resolve_token`) should call
    /// `api::login` and persist the result.
    NeedsLogin { username: String, password: String },
    /// Nothing is configured. `message` is the actionable error to
    /// surface, naming all three options.
    Missing { message: String },
}

/// Treat a cached token as expired if it expires within this window.
/// Protects against using a token that the server has just expired
/// while we were still considering it valid.
pub const TOKEN_EXPIRY_SKEW_SECS: u64 = 60;

/// Token lifetime to record in the cache after a successful login.
/// Matches the Rossum-documented default for `POST /v1/auth/login`
/// (162h). If the server's policy caps the actual lifetime shorter,
/// the mid-run 401 path catches it with one wasted call + a silent
/// re-login.
pub const LOGIN_TOKEN_LIFETIME_SECS: u64 = 162 * 3600;

/// Inspect the per-env credential state and report a [`TokenLookup`].
///
/// Resolution order:
/// 1. `RDC_TOKEN_<ENV>` env var — used as-is, opaque (no expiry tracking).
/// 2. `secrets/<env>.secrets.json` (`{api_token, expires_at?}`) — used if
///    `expires_at` is absent or > `now + TOKEN_EXPIRY_SKEW_SECS`.
/// 3. `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` — returns `NeedsLogin` for the
///    async caller to exchange for a token via `POST /v1/auth/login`.
///
/// Returns `TokenLookup::Missing` if nothing is configured (or only one
/// half of `USER`/`PASS` is set).
pub fn resolve_token_lookup(project_root: &Path, env: &str) -> Result<TokenLookup> {
    resolve_token_lookup_from(project_root, env, |k| std::env::var(k).ok())
}

/// Inner form with an injectable env-getter and clock. Lets tests
/// cover branches without mutating the process-wide environment or
/// the real clock.
fn resolve_token_lookup_from_at<F: Fn(&str) -> Option<String>>(
    project_root: &Path,
    env: &str,
    get_env: F,
    now_unix_secs: u64,
) -> Result<TokenLookup> {
    let token_var = env_var_for(env, "TOKEN");
    let user_var = env_var_for(env, "USER");
    let pass_var = env_var_for(env, "PASS");

    // 1. RDC_TOKEN_<ENV> override always wins.
    if let Some(t) = get_env(&token_var) {
        if !t.is_empty() {
            return Ok(TokenLookup::Cached { token: t, expires_at: None });
        }
    }

    // 2. Cached token in secrets/<env>.secrets.json, if still valid.
    let path = project_root.join("secrets").join(format!("{env}.secrets.json"));
    let mut cached_token_valid: Option<TokenLookup> = None;
    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        #[derive(Deserialize)]
        struct File {
            api_token: String,
            #[serde(default)]
            expires_at: Option<u64>,
        }
        let f: File = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        if !f.api_token.is_empty() {
            let is_valid = match f.expires_at {
                None => true, // no expiry tracking; treat as valid
                Some(exp) => exp > now_unix_secs.saturating_add(TOKEN_EXPIRY_SKEW_SECS),
            };
            if is_valid {
                cached_token_valid = Some(TokenLookup::Cached {
                    token: f.api_token,
                    expires_at: f.expires_at,
                });
            }
        }
    }
    if let Some(lookup) = cached_token_valid {
        return Ok(lookup);
    }

    // 3. RDC_USER_<ENV> + RDC_PASS_<ENV> creds for a fresh login.
    let user_opt = get_env(&user_var).filter(|s| !s.is_empty());
    let pass_opt = get_env(&pass_var).filter(|s| !s.is_empty());
    match (user_opt, pass_opt) {
        (Some(username), Some(password)) => {
            return Ok(TokenLookup::NeedsLogin { username, password });
        }
        (Some(_), None) => {
            return Ok(TokenLookup::Missing {
                message: format!(
                    "only ${user_var} is set; also set ${pass_var} (both required) \
                     or set ${token_var}, or run `rdc auth {env} --username <u>`"
                ),
            });
        }
        (None, Some(_)) => {
            return Ok(TokenLookup::Missing {
                message: format!(
                    "only ${pass_var} is set; also set ${user_var} (both required) \
                     or set ${token_var}, or run `rdc auth {env} --username <u>`"
                ),
            });
        }
        (None, None) => {}
    }

    // 4. Nothing configured.
    Ok(TokenLookup::Missing {
        message: format!(
            "no token for env '{env}': set ${token_var}, \
             set ${user_var} + ${pass_var}, \
             or run `rdc auth {env}`"
        ),
    })
}

/// Production wrapper: real env-getter, real clock.
fn resolve_token_lookup_from<F: Fn(&str) -> Option<String>>(
    project_root: &Path,
    env: &str,
    get_env: F,
) -> Result<TokenLookup> {
    resolve_token_lookup_from_at(project_root, env, get_env, now_unix_secs())
}

fn now_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convenience wrapper that converts a [`TokenLookup`] back into the
/// `Result<String>` shape the existing callers expect. **Sync at this
/// checkpoint — becomes async in Task 5 when the login flow is wired up.**
pub fn resolve_token(project_root: &Path, env: &str) -> Result<String> {
    match resolve_token_lookup(project_root, env)? {
        TokenLookup::Cached { token, .. } => Ok(token),
        TokenLookup::NeedsLogin { .. } => Err(anyhow!(
            "env '{env}' has credentials but rdc cannot log in synchronously here \
             — this is a bug; report it"
        )),
        TokenLookup::Missing { message } => Err(anyhow!(message)),
    }
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

    /// Full slug → K/V map. Exposed so callers can produce merged
    /// outputs (e.g. the deploy template writer that pre-populates the
    /// file with empty placeholders for missing keys) without
    /// re-reading from disk.
    pub fn entries(&self) -> &BTreeMap<String, BTreeMap<String, String>> {
        &self.by_slug
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

/// Write `secrets/<env>.hook-secrets.json` so every hook in
/// `required_per_slug` has an entry with every required key declared —
/// values already filled in by the user are preserved, anything else
/// becomes an empty-string placeholder. Slugs already present in
/// `existing` but not in `required_per_slug` are passed through
/// unchanged so unrelated hooks aren't clobbered.
///
/// Used by `rdc deploy`'s pre-flight when the target's local file lacks
/// values: instead of asking the user to figure out the JSON shape, rdc
/// hands them a fill-in-the-blanks form. Returns the absolute path
/// written so callers can quote it in the actionable error message.
///
/// Pretty-printed with sorted keys (BTreeMap iteration order) so the
/// file is human-editable and re-runs produce stable diffs. The atomic
/// write skips the rename when bytes are unchanged, preserving mtime
/// when nothing needed merging. Mode 0600 on Unix to match the existing
/// `secrets/<env>.secrets.json` convention.
pub fn write_hook_secrets_template(
    project_root: &Path,
    env: &str,
    required_per_slug: &BTreeMap<String, Vec<String>>,
    existing: &HookSecrets,
) -> Result<PathBuf> {
    let mut merged: BTreeMap<String, BTreeMap<String, String>> = existing.entries().clone();
    for (slug, required) in required_per_slug {
        let entry = merged.entry(slug.clone()).or_default();
        for key in required {
            entry.entry(key.clone()).or_insert_with(String::new);
        }
    }

    let body = serde_json::json!({ "hooks": merged });
    let mut bytes = serde_json::to_vec_pretty(&body)
        .context("serializing hook-secrets template")?;
    bytes.push(b'\n');

    let path = hook_secrets_path(project_root, env);
    crate::snapshot::writer::write_atomic(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(path)
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
        let lookup = resolve_token_lookup_from(dir.path(), "dev", |k| {
            (k == "RDC_TOKEN_DEV").then(|| "from-env".to_string())
        })
        .unwrap();
        assert!(matches!(lookup, TokenLookup::Cached { ref token, .. } if token == "from-env"));
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
        let lookup = resolve_token_lookup_from(dir.path(), "dev", |_| None).unwrap();
        assert!(matches!(lookup, TokenLookup::Cached { ref token, .. } if token == "from-file"));
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
        let lookup = resolve_token_lookup_from(dir.path(), "dev", |_| Some(String::new())).unwrap();
        assert!(matches!(lookup, TokenLookup::Cached { ref token, .. } if token == "from-file"));
    }

    #[test]
    fn missing_token_errors_with_actionable_message() {
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "unittest_c", |_| None).unwrap();
        match lookup {
            TokenLookup::Missing { message } => {
                assert!(message.contains("RDC_TOKEN_UNITTEST_C"), "should mention env var: {message}");
                assert!(message.contains("rdc auth unittest_c"), "should mention interactive auth: {message}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn env_token_var_uppercases_and_keeps_alphanumerics() {
        assert_eq!(env_var_for("dev", "TOKEN"), "RDC_TOKEN_DEV");
        assert_eq!(env_var_for("PROD", "TOKEN"), "RDC_TOKEN_PROD");
        assert_eq!(env_var_for("staging42", "TOKEN"), "RDC_TOKEN_STAGING42");
    }

    #[test]
    fn env_token_var_maps_hyphen_to_underscore() {
        // The motivating case: real env names like `dev-ap` need to
        // produce a valid POSIX env-var identifier.
        assert_eq!(env_var_for("dev-ap", "TOKEN"), "RDC_TOKEN_DEV_AP");
        assert_eq!(env_var_for("prod-eu-west-1", "TOKEN"), "RDC_TOKEN_PROD_EU_WEST_1");
    }

    #[test]
    fn env_token_var_preserves_existing_underscores() {
        assert_eq!(env_var_for("dev_ap", "TOKEN"), "RDC_TOKEN_DEV_AP");
    }

    #[test]
    fn env_token_var_collision_between_hyphen_and_underscore_is_documented() {
        // This is the known footgun; the init wizard refuses the
        // second one of these pairs to prevent it inside a project.
        // Documented here so a future change can't silently break it.
        assert_eq!(env_var_for("dev-ap", "TOKEN"), env_var_for("dev_ap", "TOKEN"));
    }

    #[test]
    fn env_var_for_supports_arbitrary_suffix() {
        assert_eq!(env_var_for("dev", "TOKEN"), "RDC_TOKEN_DEV");
        assert_eq!(env_var_for("dev", "USER"), "RDC_USER_DEV");
        assert_eq!(env_var_for("dev", "PASS"), "RDC_PASS_DEV");
        assert_eq!(env_var_for("dev-ap", "USER"), "RDC_USER_DEV_AP");
        assert_eq!(env_var_for("prod-eu-west-1", "PASS"), "RDC_PASS_PROD_EU_WEST_1");
    }

    #[test]
    fn resolve_token_uses_normalized_env_var_for_hyphenated_env() {
        // `dev-ap` env must resolve via `$RDC_TOKEN_DEV_AP`, not the
        // invalid `$RDC_TOKEN_DEV-AP` (which no shell can export).
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "dev-ap", |k| {
            (k == "RDC_TOKEN_DEV_AP").then(|| "from-env".to_string())
        })
        .unwrap();
        assert!(matches!(lookup, TokenLookup::Cached { ref token, .. } if token == "from-env"));
    }

    #[test]
    fn resolve_token_missing_message_quotes_normalized_var_name() {
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "dev-ap", |_| None).unwrap();
        match lookup {
            TokenLookup::Missing { message } => {
                assert!(message.contains("RDC_TOKEN_DEV_AP"), "must point at actual env-var name: {message}");
                assert!(!message.contains("RDC_TOKEN_DEV-AP"), "must not mention hyphenated form: {message}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
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

    fn required(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(slug, keys)| {
                (
                    (*slug).to_string(),
                    keys.iter().map(|k| (*k).to_string()).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    fn read_template(dir: &Path, env: &str) -> serde_json::Value {
        let raw = std::fs::read_to_string(hook_secrets_path(dir, env)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn write_template_creates_file_with_empty_placeholders() {
        // First-deploy case: no local file yet, no prior values. The
        // template must materialize the full required shape so the user
        // can just fill in values without reverse-engineering the JSON.
        let dir = TempDir::new().unwrap();
        let req = required(&[
            ("master-data-hub", &["mdh_api_token", "mdh_endpoint"]),
            ("notify-slack", &["signing_secret"]),
        ]);
        let existing = HookSecrets::default();
        let path = write_hook_secrets_template(dir.path(), "test-mtr", &req, &existing).unwrap();
        assert_eq!(path, hook_secrets_path(dir.path(), "test-mtr"));

        let v = read_template(dir.path(), "test-mtr");
        assert_eq!(
            v,
            serde_json::json!({
                "hooks": {
                    "master-data-hub": { "mdh_api_token": "", "mdh_endpoint": "" },
                    "notify-slack":    { "signing_secret": "" }
                }
            })
        );
    }

    #[test]
    fn write_template_preserves_existing_values() {
        // User has already filled in some keys for a previous deploy;
        // re-running for a new hook must NOT wipe what they typed.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/test-mtr.hook-secrets.json"),
            r#"{ "hooks": { "master-data-hub": { "mdh_api_token": "kept-by-user" } } }"#,
        )
        .unwrap();
        let existing = load_hook_secrets(dir.path(), "test-mtr").unwrap();
        let req = required(&[
            ("master-data-hub", &["mdh_api_token", "mdh_endpoint"]),
            ("notify-slack", &["signing_secret"]),
        ]);
        write_hook_secrets_template(dir.path(), "test-mtr", &req, &existing).unwrap();

        let v = read_template(dir.path(), "test-mtr");
        assert_eq!(v["hooks"]["master-data-hub"]["mdh_api_token"], "kept-by-user");
        assert_eq!(v["hooks"]["master-data-hub"]["mdh_endpoint"], "");
        assert_eq!(v["hooks"]["notify-slack"]["signing_secret"], "");
    }

    #[test]
    fn write_template_passes_through_unrelated_slugs() {
        // A slug already in the file but outside this deploy's scope
        // belongs to another deploy / another hook; never wipe it.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/test-mtr.hook-secrets.json"),
            r#"{ "hooks": { "old-hook": { "legacy_token": "still-here" } } }"#,
        )
        .unwrap();
        let existing = load_hook_secrets(dir.path(), "test-mtr").unwrap();
        let req = required(&[("new-hook", &["new_key"])]);
        write_hook_secrets_template(dir.path(), "test-mtr", &req, &existing).unwrap();

        let v = read_template(dir.path(), "test-mtr");
        assert_eq!(v["hooks"]["old-hook"]["legacy_token"], "still-here");
        assert_eq!(v["hooks"]["new-hook"]["new_key"], "");
    }

    #[test]
    fn write_template_creates_secrets_dir_when_missing() {
        // Fresh `rdc init` projects have `secrets/` but a paranoid test
        // wipes it; the writer must reconstruct the parent itself rather
        // than 500ing on a missing directory.
        let dir = TempDir::new().unwrap();
        let req = required(&[("h", &["k"])]);
        let existing = HookSecrets::default();
        write_hook_secrets_template(dir.path(), "test-mtr", &req, &existing).unwrap();
        assert!(hook_secrets_path(dir.path(), "test-mtr").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_template_chmods_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let req = required(&[("h", &["k"])]);
        let existing = HookSecrets::default();
        let path = write_hook_secrets_template(dir.path(), "test-mtr", &req, &existing).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "hook-secrets template must be owner-only");
    }

    #[test]
    fn lookup_returns_cached_with_expires_at_from_file() {
        // Use the clock-injected variant so the recorded expiry stays
        // in the future relative to the test clock regardless of
        // wall-clock time.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"abc","expires_at":2000}"#,
        )
        .unwrap();
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", |_| None, 1000).unwrap();
        match lookup {
            TokenLookup::Cached { token, expires_at } => {
                assert_eq!(token, "abc");
                assert_eq!(expires_at, Some(2000));
            }
            other => panic!("expected Cached, got {other:?}"),
        }
    }

    #[test]
    fn lookup_returns_cached_without_expires_at_when_field_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"abc"}"#,
        )
        .unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "dev", |_| None).unwrap();
        match lookup {
            TokenLookup::Cached { token, expires_at } => {
                assert_eq!(token, "abc");
                assert_eq!(expires_at, None);
            }
            other => panic!("expected Cached, got {other:?}"),
        }
    }

    #[test]
    fn lookup_returns_token_env_var_with_no_expiry() {
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "dev", |k| {
            (k == "RDC_TOKEN_DEV").then(|| "from-env".to_string())
        })
        .unwrap();
        match lookup {
            TokenLookup::Cached { token, expires_at } => {
                assert_eq!(token, "from-env");
                assert_eq!(expires_at, None, "env-var tokens are opaque, no expiry tracking");
            }
            other => panic!("expected Cached, got {other:?}"),
        }
    }

    #[test]
    fn lookup_returns_missing_with_actionable_message_when_nothing_configured() {
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "dev", |_| None).unwrap();
        match lookup {
            TokenLookup::Missing { message } => {
                assert!(message.contains("RDC_TOKEN_DEV"), "missing message: {message}");
                assert!(message.contains("rdc auth dev"), "missing message: {message}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn lookup_with_non_expired_cache_returns_cached() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"abc","expires_at":2000}"#,
        )
        .unwrap();
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", |_| None, 1000).unwrap();
        assert!(matches!(lookup, TokenLookup::Cached { ref token, .. } if token == "abc"));
    }

    #[test]
    fn lookup_with_expired_cache_falls_through_to_creds() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"stale","expires_at":1000}"#,
        )
        .unwrap();
        let get_env = |k: &str| match k {
            "RDC_USER_DEV" => Some("alice".to_string()),
            "RDC_PASS_DEV" => Some("hunter2".to_string()),
            _ => None,
        };
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", get_env, 2000).unwrap();
        match lookup {
            TokenLookup::NeedsLogin { username, password } => {
                assert_eq!(username, "alice");
                assert_eq!(password, "hunter2");
            }
            other => panic!("expected NeedsLogin, got {other:?}"),
        }
    }

    #[test]
    fn lookup_skew_within_60s_of_expiry_treated_as_expired() {
        // expires_at = now + 30s -> within the 60s skew, treat as expired
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"about-to-expire","expires_at":1030}"#,
        )
        .unwrap();
        let get_env = |k: &str| match k {
            "RDC_USER_DEV" => Some("alice".to_string()),
            "RDC_PASS_DEV" => Some("pw".to_string()),
            _ => None,
        };
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", get_env, 1000).unwrap();
        assert!(matches!(lookup, TokenLookup::NeedsLogin { .. }));
    }

    #[test]
    fn lookup_creds_only_no_cache_returns_needs_login() {
        let dir = TempDir::new().unwrap();
        let get_env = |k: &str| match k {
            "RDC_USER_DEV" => Some("alice".to_string()),
            "RDC_PASS_DEV" => Some("pw".to_string()),
            _ => None,
        };
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", get_env, 1000).unwrap();
        assert!(matches!(lookup, TokenLookup::NeedsLogin { ref username, .. } if username == "alice"));
    }

    #[test]
    fn lookup_creds_one_missing_errors_naming_the_missing_var() {
        let dir = TempDir::new().unwrap();
        let get_env_user_only = |k: &str| match k {
            "RDC_USER_DEV" => Some("alice".to_string()),
            _ => None,
        };
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", get_env_user_only, 1000).unwrap();
        match lookup {
            TokenLookup::Missing { message } => {
                assert!(message.contains("RDC_PASS_DEV"), "must name the missing var: {message}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn lookup_missing_message_names_all_three_options() {
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", |_| None, 1000).unwrap();
        match lookup {
            TokenLookup::Missing { message } => {
                assert!(message.contains("RDC_TOKEN_DEV"), "names env-var token option: {message}");
                assert!(message.contains("RDC_USER_DEV"), "names creds option: {message}");
                assert!(message.contains("RDC_PASS_DEV"), "names creds option: {message}");
                assert!(message.contains("rdc auth dev"), "names interactive option: {message}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn token_env_var_wins_even_if_cache_is_expired() {
        // RDC_TOKEN_DEV is the explicit override; it always wins, no
        // matter what the cache says.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.secrets.json"),
            r#"{"api_token":"stale","expires_at":1000}"#,
        )
        .unwrap();
        let get_env = |k: &str| (k == "RDC_TOKEN_DEV").then(|| "override".to_string());
        let lookup = resolve_token_lookup_from_at(dir.path(), "dev", get_env, 2000).unwrap();
        assert!(matches!(lookup, TokenLookup::Cached { ref token, .. } if token == "override"));
    }

    #[test]
    fn write_template_load_round_trip_is_lossless() {
        // The reader and writer must agree on the JSON shape — write a
        // template, load it back, and verify every slug+key the writer
        // wrote appears in the loaded view.
        let dir = TempDir::new().unwrap();
        let req = required(&[
            ("alpha", &["k1", "k2"]),
            ("beta", &["bk"]),
        ]);
        let existing = HookSecrets::default();
        write_hook_secrets_template(dir.path(), "test-mtr", &req, &existing).unwrap();
        let loaded = load_hook_secrets(dir.path(), "test-mtr").unwrap();
        assert!(loaded.was_loaded());
        let alpha = loaded.for_slug("alpha").expect("alpha entry");
        assert_eq!(alpha.get("k1").map(String::as_str), Some(""));
        assert_eq!(alpha.get("k2").map(String::as_str), Some(""));
        let beta = loaded.for_slug("beta").expect("beta entry");
        assert_eq!(beta.get("bk").map(String::as_str), Some(""));
    }
}
