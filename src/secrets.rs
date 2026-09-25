use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// On-disk shape of `secrets/<env>.secrets.json`. All fields are
/// optional so the file can hold a token (CLI / token-mode add),
/// username + password (desktop app's password-mode add, ahead of any
/// successful login), or both (after a login has cached a token while
/// the credentials remain available for silent re-login).
///
/// The desktop app stores password-mode credentials here so rdc's own
/// `resolve_token` can see them and run the same re-login flow used for
/// the CLI's `RDC_USER` / `RDC_PASS` env-var path.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct SecretsFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Read `secrets/<env>.secrets.json` if present. Returns an empty
/// `SecretsFile` when the file does not exist. Errors only on
/// malformed JSON or unreadable files.
pub fn read_secrets_file(project_root: &Path, env: &str) -> Result<SecretsFile> {
    let path = project_root.join("secrets").join(format!("{env}.secrets.json"));
    if !path.exists() {
        return Ok(SecretsFile::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let parsed: SecretsFile = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed)
}

fn write_secrets_file_full(
    project_root: &Path,
    env: &str,
    file: &SecretsFile,
) -> Result<PathBuf> {
    let path = project_root.join("secrets").join(format!("{env}.secrets.json"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(file).context("serializing secrets JSON")?;
    bytes.push(b'\n');
    crate::snapshot::writer::write_atomic(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

/// Persist a password-mode credential pair to `secrets/<env>.secrets.json`,
/// preserving any existing `api_token` / `expires_at` already in the file.
/// Used by the desktop app at Add Connection / Edit Credentials time.
///
/// rdc's own `resolve_token` (which the desktop bridge calls before each
/// sync) reads these fields and runs the standard
/// `NeedsLogin` → `POST /v1/auth/login` → cache flow when the token is
/// missing or expired.
pub fn save_password_credentials(
    project_root: &Path,
    env: &str,
    username: &str,
    password: &str,
) -> Result<PathBuf> {
    let mut current = read_secrets_file(project_root, env).unwrap_or_default();
    current.username = Some(username.to_string());
    current.password = Some(password.to_string());
    write_secrets_file_full(project_root, env, &current)
}

/// The normalized, shell-safe suffix rdc appends to a per-env credential
/// variable name: ASCII alphanumerics uppercased, every other character `_`
/// (so the shell can export it). `dev-us` -> `DEV_US`.
pub fn env_var_suffix(env: &str) -> String {
    env.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Compute the environment-variable name rdc looks at for a per-env
/// credential field. `suffix` is `TOKEN`, `USER`, or `PASS`.
///
/// POSIX env-var identifiers are `[A-Za-z_][A-Za-z0-9_]*`, but
/// rdc env names accept `-` and `_` (e.g. `dev-us`). To produce a
/// name the shell can actually export, every non-alphanumeric
/// character in the env name is mapped to `_` and the whole thing
/// uppercased.
///
/// | env name   | suffix  | env-var               |
/// |------------|---------|-----------------------|
/// | `dev`      | `TOKEN` | `RDC_TOKEN_DEV`       |
/// | `dev-us`   | `USER`  | `RDC_USER_DEV_US`     |
/// | `prod_eu`  | `PASS`  | `RDC_PASS_PROD_EU`    |
///
/// The hyphen-vs-underscore collision documented for `env_token_var`
/// still applies (e.g. `dev-us` and `dev_us` normalize to the same
/// suffix). The `rdc init` wizard prevents this collision at project
/// creation time.
pub fn env_var_for(env: &str, suffix: &str) -> String {
    format!("RDC_{suffix}_{}", env_var_suffix(env))
}

/// Outcome of synchronously inspecting the per-env credential
/// configuration (env vars + on-disk secrets file). The async
/// [`resolve_token`] consumes this enum and performs I/O (HTTP login,
/// cache write) when needed.
#[derive(PartialEq, Eq)]
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

impl std::fmt::Debug for TokenLookup {
    /// Custom Debug that redacts token/password values so a stray
    /// `tracing::debug!("{lookup:?}")` (or test-panic message) doesn't
    /// leak secrets into logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cached { token: _, expires_at } => f
                .debug_struct("Cached")
                .field("token", &"<redacted>")
                .field("expires_at", expires_at)
                .finish(),
            Self::NeedsLogin { username, password: _ } => f
                .debug_struct("NeedsLogin")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Missing { message } => f
                .debug_struct("Missing")
                .field("message", message)
                .finish(),
        }
    }
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
    if let Some(t) = get_env(&token_var)
        && !t.is_empty() {
            return Ok(TokenLookup::Cached { token: t, expires_at: None });
        }

    // 2. Cached token in secrets/<env>.secrets.json, if still valid.
    let file = read_secrets_file(project_root, env)?;
    if let Some(ref token) = file.api_token
        && !token.is_empty() {
            let is_valid = match file.expires_at {
                None => true, // no expiry tracking; treat as valid
                Some(exp) => exp > now_unix_secs.saturating_add(TOKEN_EXPIRY_SKEW_SECS),
            };
            if is_valid {
                return Ok(TokenLookup::Cached {
                    token: token.clone(),
                    expires_at: file.expires_at,
                });
            }
        }

    // 3a. Username + password persisted in the secrets file (desktop app
    // password-mode). Same NeedsLogin contract as the env-var path below.
    if let (Some(u), Some(p)) = (file.username.as_deref(), file.password.as_deref())
        && !u.is_empty() && !p.is_empty() {
            return Ok(TokenLookup::NeedsLogin {
                username: u.to_string(),
                password: p.to_string(),
            });
        }

    // 3b. RDC_USER_<ENV> + RDC_PASS_<ENV> creds for a fresh login.
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

pub(crate) fn now_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the API token for an environment.
///
/// Resolution order (managed by [`resolve_token_lookup`]):
/// 1. `RDC_TOKEN_<ENV>` env var (always wins; opaque, no expiry tracking).
/// 2. Non-expired cached token in `secrets/<env>.secrets.json`.
/// 3. `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` -> exchange via
///    [`crate::api::login`], write the resulting token + computed
///    `expires_at` back to the secrets file, return the fresh token.
/// 4. Otherwise, return an actionable error.
///
/// `api_base` is needed for the login call; callers pass the env's
/// configured `api_base` (e.g. from `EnvConfig`).
pub async fn resolve_token(project_root: &Path, env: &str, api_base: &str) -> Result<String> {
    match resolve_token_lookup(project_root, env)? {
        TokenLookup::Cached { token, .. } => Ok(token),
        TokenLookup::NeedsLogin { username, password } => {
            let token = crate::api::login(api_base, &username, &password)
                .await
                .with_context(|| {
                    format!(
                        "logging in to env '{env}' with ${} / ${}",
                        env_var_for(env, "USER"),
                        env_var_for(env, "PASS"),
                    )
                })?;
            let expires_at = now_unix_secs().saturating_add(LOGIN_TOKEN_LIFETIME_SECS);
            write_secrets_file(project_root, env, &token, Some(expires_at))?;
            Ok(token)
        }
        TokenLookup::Missing { message } => Err(anyhow!(message)),
    }
}

/// Re-authenticate an env whose cached token was rejected (401), ignoring
/// the cache entirely.
///
/// [`resolve_token`] cannot do this: it returns the cached token whenever
/// `expires_at` is absent or still in the future, which is exactly the
/// state a revoked-but-unexpired token is in. And
/// `cli::auth::refresh_token_for_401` reads only `RDC_USER_<ENV>` /
/// `RDC_PASS_<ENV>`, never the credentials the desktop app persists in
/// `secrets/<env>.secrets.json`.
///
/// Token-auth projects have no credentials to re-login with, so this fails
/// with a message that tells the user what to do about it.
pub async fn force_relogin(project_root: &Path, env: &str, api_base: &str) -> Result<String> {
    let file = read_secrets_file(project_root, env)?;
    let (Some(username), Some(password)) = (file.username.as_deref(), file.password.as_deref())
    else {
        return Err(anyhow!(
            "the API token for env '{env}' was rejected (401), and this env has no saved \
             username/password to sign in with again. Update its credentials in the app's \
             Edit dialog, or run `rdc auth {env} --token <new-token>`."
        ));
    };
    if username.is_empty() || password.is_empty() {
        return Err(anyhow!(
            "the API token for env '{env}' was rejected (401), and this env's saved \
             credentials are incomplete. Update them and retry."
        ));
    }
    let token = crate::api::login(api_base, username, password)
        .await
        .with_context(|| format!("re-signing in to env '{env}' after a 401"))?;
    let expires_at = now_unix_secs().saturating_add(LOGIN_TOKEN_LIFETIME_SECS);
    write_secrets_file(project_root, env, &token, Some(expires_at))?;
    Ok(token)
}

/// Write a token (and optional expiry) to `secrets/<env>.secrets.json`
/// atomically, mode 0600 on Unix. Preserves any `username` / `password`
/// fields already in the file — the desktop app persists those for
/// password-mode silent re-login, and a token refresh must not clobber
/// them.
///
/// Used by [`resolve_token`] when caching a login-derived token, and
/// by `cli::auth::validate_and_save_token` when the user runs `rdc auth`.
pub fn write_secrets_file(
    project_root: &Path,
    env: &str,
    token: &str,
    expires_at: Option<u64>,
) -> Result<PathBuf> {
    let mut current = read_secrets_file(project_root, env).unwrap_or_default();
    current.api_token = Some(token.to_string());
    current.expires_at = expires_at;
    write_secrets_file_full(project_root, env, &current)
}

/// Literal string rdc writes in `secrets/<env>.hook-secrets.json` for
/// every required key the user hasn't filled in yet. The push injection
/// sites treat any value equal to this constant as "missing" — so a re-run
/// of `rdc sync <env>` with the placeholders unchanged still refuses to
/// proceed and re-prompts the user.
///
/// Angle brackets are deliberate: they're not valid in any sane real
/// secret value (API keys, passwords, JWTs), so the chance of a real
/// secret accidentally colliding with the sentinel is effectively
/// zero. `null` is reserved for the Rossum API semantic of "unset the
/// secret key entirely" and must NOT be hijacked for this purpose.
pub const UNFILLED_SENTINEL: &str = "<unfilled>";

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
///     "notify-slack":    { "signing_secret": "<unfilled>" }
///   }
/// }
/// ```
///
/// A value equal to [`UNFILLED_SENTINEL`] (`"<unfilled>"`) means rdc
/// pre-populated the key and the user hasn't typed a real value yet — the
/// push pipeline treats the key as still missing and never sends it to the
/// API.
///
/// Values are never read back from the server (`GET /hooks/<id>` does
/// not return `secrets`; `GET /hooks/<id>/secrets_keys` exposes the
/// list of key names only). This struct is the canonical local source.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HookSecrets {
    /// slug → (key → value).
    by_slug: BTreeMap<String, BTreeMap<String, String>>,
}

impl HookSecrets {
    /// Owned K/V map containing only the keys the user has actually
    /// filled in (i.e. the value is not the unfilled sentinel). Used
    /// by the push injection sites that must never leak the sentinel
    /// string to the Rossum API.
    pub fn filled_kv_for_slug(&self, slug: &str) -> BTreeMap<String, String> {
        self.by_slug
            .get(slug)
            .map(|kv| {
                kv.iter()
                    .filter(|(_, v)| v.as_str() != UNFILLED_SENTINEL)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// All slugs present in the local secrets file. Used to detect
    /// typo slugs that don't match any hook on push.
    pub fn slugs(&self) -> impl Iterator<Item = &String> {
        self.by_slug.keys()
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
        return Ok(HookSecrets::default());
    }
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        hooks: BTreeMap<String, BTreeMap<String, String>>,
    }
    let f: File = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(HookSecrets { by_slug: f.hooks })
}

/// The stub `rdc init` (and the first `rdc sync`) drops at
/// `secrets/<env>.hook-secrets.json` so the file is there to be found
/// instead of having to be guessed at. JSON has no comments, so the
/// instructions ride in a `"//"` key: the loader models only `hooks`, so
/// anything else in the object is ignored.
///
/// The key deliberately sits at the TOP level and not inside `hooks` —
/// a `hooks` value must deserialize as a key/value map, and even an empty
/// one would read as a hook slug, which push then warns about on every run
/// ("no lockfile entry for slug `//`").
pub const HOOK_SECRETS_STUB: &str = r#"{
  "//": "Hook secret values for this env, one entry per hook slug: \"my-hook\": { \"api_key\": \"...\" }. Rossum never returns these, so this file is their only copy; it is gitignored and never copied between envs. A value of \"<unfilled>\" is skipped on push.",
  "hooks": {}
}
"#;

/// Write [`HOOK_SECRETS_STUB`] at `secrets/<env>.hook-secrets.json` when that
/// file does not exist yet, and return whether it was created.
///
/// Create-if-absent with no `force` escape hatch, unlike every other scaffold:
/// this file holds real secret values that exist nowhere else — not on the
/// server, not in git — so there is no version of "regenerate it" that is not
/// data loss. Mode 0600 on Unix, matching `secrets/<env>.secrets.json`.
pub fn write_hook_secrets_stub(project_root: &Path, env: &str) -> Result<bool> {
    let path = hook_secrets_path(project_root, env);
    if path.exists() {
        return Ok(false);
    }
    crate::snapshot::writer::write_atomic(&path, HOOK_SECRETS_STUB.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(true)
}

/// Move a hook's entry in `secrets/<env>.hook-secrets.json` from slug `old`
/// to slug `new`, so a renamed hook keeps its secrets. Returns whether it
/// moved one. Leaves the file alone when it has no entry for `old`, and
/// refuses to overwrite an existing entry for `new`. Every other key in the
/// file, `"//"` included, is kept as it was.
pub fn rename_hook_secret(project_root: &Path, env: &str, old: &str, new: &str) -> Result<bool> {
    let path = hook_secrets_path(project_root, env);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let mut file: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let Some(hooks) = file.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(false);
    };
    if hooks.contains_key(new) {
        anyhow::bail!(
            "{} already holds secrets for hook '{new}'; move the ones for '{old}' by hand",
            path.display()
        );
    }
    let Some(entry) = hooks.remove(old) else {
        return Ok(false);
    };
    hooks.insert(new.to_string(), entry);
    let mut bytes = serde_json::to_vec_pretty(&file)?;
    bytes.push(b'\n');
    crate::snapshot::writer::write_atomic(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(true)
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
        // The motivating case: real env names like `dev-us` need to
        // produce a valid POSIX env-var identifier.
        assert_eq!(env_var_for("dev-us", "TOKEN"), "RDC_TOKEN_DEV_US");
        assert_eq!(env_var_for("prod-eu-west-1", "TOKEN"), "RDC_TOKEN_PROD_EU_WEST_1");
    }

    #[test]
    fn env_token_var_preserves_existing_underscores() {
        assert_eq!(env_var_for("dev_us", "TOKEN"), "RDC_TOKEN_DEV_US");
    }

    #[test]
    fn env_token_var_collision_between_hyphen_and_underscore_is_documented() {
        // This is the known footgun; the init wizard refuses the
        // second one of these pairs to prevent it inside a project.
        // Documented here so a future change can't silently break it.
        assert_eq!(env_var_for("dev-us", "TOKEN"), env_var_for("dev_us", "TOKEN"));
    }

    #[test]
    fn env_var_for_supports_arbitrary_suffix() {
        assert_eq!(env_var_for("dev", "TOKEN"), "RDC_TOKEN_DEV");
        assert_eq!(env_var_for("dev", "USER"), "RDC_USER_DEV");
        assert_eq!(env_var_for("dev", "PASS"), "RDC_PASS_DEV");
        assert_eq!(env_var_for("dev-us", "USER"), "RDC_USER_DEV_US");
        assert_eq!(env_var_for("prod-eu-west-1", "PASS"), "RDC_PASS_PROD_EU_WEST_1");
    }

    #[test]
    fn resolve_token_uses_normalized_env_var_for_hyphenated_env() {
        // `dev-us` env must resolve via `$RDC_TOKEN_DEV_US`, not the
        // invalid `$RDC_TOKEN_DEV-US` (which no shell can export).
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "dev-us", |k| {
            (k == "RDC_TOKEN_DEV_US").then(|| "from-env".to_string())
        })
        .unwrap();
        assert!(matches!(lookup, TokenLookup::Cached { ref token, .. } if token == "from-env"));
    }

    #[test]
    fn resolve_token_missing_message_quotes_normalized_var_name() {
        let dir = TempDir::new().unwrap();
        let lookup = resolve_token_lookup_from(dir.path(), "dev-us", |_| None).unwrap();
        match lookup {
            TokenLookup::Missing { message } => {
                assert!(message.contains("RDC_TOKEN_DEV_US"), "must point at actual env-var name: {message}");
                assert!(!message.contains("RDC_TOKEN_DEV-US"), "must not mention hyphenated form: {message}");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn hook_secrets_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let s = load_hook_secrets(dir.path(), "dev").unwrap();
        assert!(!s.by_slug.contains_key("anything"));
        assert_eq!(s.slugs().count(), 0);
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
        let mdh = s.by_slug.get("master-data-hub").expect("mdh entry");
        assert_eq!(mdh.get("mdh_api_token").map(String::as_str), Some("abc"));
        assert_eq!(mdh.get("mdh_endpoint").map(String::as_str), Some("https://x"));
        let slack = s.by_slug.get("notify-slack").expect("slack entry");
        assert_eq!(slack.get("signing_secret").map(String::as_str), Some("xyz"));
        assert!(!s.by_slug.contains_key("unrelated"));
        let slugs: Vec<&String> = s.slugs().collect();
        assert_eq!(slugs.len(), 2, "should report both slugs (sorted)");
    }

    #[test]
    fn hook_secrets_empty_file_is_loaded_but_empty() {
        // An empty file is a valid "I have a project-level secrets
        // file but no values yet" state, not a parse error.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/dev.hook-secrets.json"), "").unwrap();
        let s = load_hook_secrets(dir.path(), "dev").unwrap();
        assert_eq!(s.slugs().count(), 0);
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

    /// The stub must load as a hook-secrets file with NO slugs in it. A `"//"`
    /// that ended up inside `hooks` would read as a slug, and push warns once
    /// per run about a slug with no lockfile entry.
    #[test]
    fn hook_secrets_stub_loads_as_an_empty_file_with_no_slugs() {
        let dir = TempDir::new().unwrap();
        write_hook_secrets_stub(dir.path(), "test-eu").unwrap();
        let s = load_hook_secrets(dir.path(), "test-eu").unwrap();
        assert_eq!(s.slugs().count(), 0, "the \"//\" key must not read as a slug");
    }

    /// The stub tells the reader the shape and names the sentinel, because the
    /// whole point of writing it is that nobody has to go looking for either.
    #[test]
    fn hook_secrets_stub_documents_the_shape_and_the_sentinel() {
        let v: serde_json::Value = serde_json::from_str(HOOK_SECRETS_STUB).unwrap();
        let note = v["//"].as_str().expect("a top-level \"//\" note");
        assert!(note.contains("my-hook"), "{note}");
        assert!(note.contains(UNFILLED_SENTINEL), "{note}");
        assert!(v["hooks"].as_object().unwrap().is_empty());
    }

    /// Create-if-absent, forever: this file is the only copy of its values.
    #[test]
    fn write_hook_secrets_stub_never_overwrites() {
        let dir = TempDir::new().unwrap();
        assert!(write_hook_secrets_stub(dir.path(), "test-eu").unwrap());

        let mine = r#"{ "hooks": { "h": { "k": "v" } } }"#;
        std::fs::write(hook_secrets_path(dir.path(), "test-eu"), mine).unwrap();
        assert!(!write_hook_secrets_stub(dir.path(), "test-eu").unwrap());
        assert_eq!(
            std::fs::read_to_string(hook_secrets_path(dir.path(), "test-eu")).unwrap(),
            mine
        );
    }

    /// Fresh `rdc init` projects have `secrets/`, but `rdc sync` writes the
    /// stub too and may be the first thing a clone runs.
    #[test]
    fn write_hook_secrets_stub_creates_the_secrets_dir() {
        let dir = TempDir::new().unwrap();
        write_hook_secrets_stub(dir.path(), "test-eu").unwrap();
        assert!(hook_secrets_path(dir.path(), "test-eu").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_hook_secrets_stub_chmods_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        write_hook_secrets_stub(dir.path(), "test-eu").unwrap();
        let path = hook_secrets_path(dir.path(), "test-eu");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the hook-secrets stub must be owner-only");
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
    fn filled_kv_for_slug_strips_sentinel_values() {
        // The injection-side helper must never leak the sentinel string
        // to the API. A half-edited template (one key filled, one not)
        // must result in just the filled key in the outbound map.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/dev.hook-secrets.json"),
            format!(
                r#"{{ "hooks": {{ "h": {{ "filled": "real-value", "still_unfilled": "{UNFILLED_SENTINEL}" }} }} }}"#,
            ),
        )
        .unwrap();
        let s = load_hook_secrets(dir.path(), "dev").unwrap();
        let kv = s.filled_kv_for_slug("h");
        assert_eq!(kv.len(), 1, "sentinel-valued keys must be excluded: {kv:?}");
        assert_eq!(kv.get("filled").map(String::as_str), Some("real-value"));
        assert!(!kv.contains_key("still_unfilled"));
    }

    #[test]
    fn debug_redacts_token_and_password() {
        let cached = TokenLookup::Cached {
            token: "secret-token-abc".to_string(),
            expires_at: Some(123),
        };
        let s = format!("{cached:?}");
        assert!(
            !s.contains("secret-token-abc"),
            "Debug must not leak token: {s}"
        );
        assert!(s.contains("<redacted>"), "Debug should mark redaction: {s}");
        assert!(s.contains("123"), "Debug should keep expires_at: {s}");

        let needs = TokenLookup::NeedsLogin {
            username: "alice".to_string(),
            password: "hunter2".to_string(),
        };
        let s = format!("{needs:?}");
        assert!(
            !s.contains("hunter2"),
            "Debug must not leak password: {s}"
        );
        assert!(
            s.contains("alice"),
            "username is not a secret, may appear: {s}"
        );
        assert!(s.contains("<redacted>"), "Debug should mark redaction: {s}");

        let missing = TokenLookup::Missing {
            message: "no token set".to_string(),
        };
        let s = format!("{missing:?}");
        assert!(
            s.contains("no token set"),
            "Debug should keep error message: {s}"
        );
    }

    #[test]
    fn force_relogin_refuses_without_persisted_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        write_secrets_file(tmp.path(), "dev", "a-token", None).unwrap();
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(force_relogin(tmp.path(), "dev", "https://acme.test/api/v1"))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("token") && msg.contains("dev"),
            "error must name the env and say the token was rejected: {msg}"
        );
        assert!(
            msg.contains("Edit dialog") && msg.contains("rdc auth dev --token"),
            "error must name a concrete desktop action (Edit dialog) and the CLI equivalent: {msg}"
        );
    }
}
