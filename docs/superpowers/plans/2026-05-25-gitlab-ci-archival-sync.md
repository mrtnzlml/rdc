# GitLab CI archival sync + native user/pass auth — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add native username/password authentication to rdc (exchanges creds for a token via `POST /v1/auth/login`, caches the token + expiry in the existing secrets file, auto-refreshes when expired or on mid-run 401), and ship a copy-paste GitLab CI template that uses it for daily archival sync.

**Architecture:** Token resolution becomes a two-stage process. A sync helper (`secrets::resolve_token_lookup`) returns a `TokenLookup` enum describing the situation (`Cached`, `NeedsLogin`, `Missing`). An async wrapper (`secrets::resolve_token`) consumes the enum: on `NeedsLogin`, calls `api::login` and persists the new token + expiry to the same `secrets/<env>.secrets.json` the manual `--token` flow already writes; on `Cached` (non-expired), returns directly. All existing call sites of `resolve_token` migrate to this async signature. The existing `with_401_retry`/`refresh_token_interactively` machinery is extended to attempt a silent re-login on non-TTY contexts when `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are set, falling back to today's TTY prompt / actionable error otherwise. A new `rdc auth <env> --username <u>` CLI subcommand mirrors today's `--token` flow.

**Tech Stack:** Rust 2024, `reqwest` (HTTP), `serde`/`serde_json` (JSON), `inquire` (TTY password prompt — already a dep), `wiremock` (integration tests — already a dev-dep), `assert_cmd` + `predicates` (CLI tests — already dev-deps).

**Spec reference:** `docs/superpowers/specs/2026-05-25-gitlab-ci-archival-sync-design.md`.

---

## File map

| Path | Action | Responsibility |
|---|---|---|
| `src/secrets.rs` | Modify | Add `env_var_for(env, suffix)` helper. Add `TokenLookup` enum. Add `expires_at` to the on-disk schema. Make `resolve_token` async; integrate login + cache write. |
| `src/api/mod.rs` | Modify | Add `pub async fn login(api_base, username, password) -> Result<String>` free function (doesn't need a token). |
| `src/cli/auth.rs` | Modify | Add `--username` path to `run()`; extend `refresh_token_interactively` to attempt silent re-login from env vars on non-TTY. |
| `src/cli/mod.rs` | Modify | Add `username: Option<String>` field to `Command::Auth` with `conflicts_with = "token"`. Wire it through to `cli::auth::run`. |
| `src/cli/sync/mod.rs` | Modify | Migrate `resolve_token(&cwd, env)?` → `resolve_token(&cwd, env, &env_cfg.api_base).await?`. |
| `src/cli/deploy/run.rs` | Modify | Same migration (two call sites). |
| `src/cli/deploy/apply.rs` | Modify | Same migration. |
| `src/cli/diff.rs` | Modify | Same migration. |
| `src/cli/repair/store_anomaly.rs` | Modify | Same migration. |
| `tests/cli_auth.rs` | Modify | New test cases for `--username`, mutual exclusion, expiry, silent re-login. |
| `tests/api.rs` | Modify | New test cases for `api::login` against wiremock. |
| `templates/gitlab-ci-archival.yml` | Create | The CI template. |
| `README.md` | Modify | Authentication section gets an env-var table; new "Daily archive in GitLab CI" subsection. |

---

### Task 1: Generalize `env_token_var` → `env_var_for(env, suffix)`

**Files:**
- Modify: `src/secrets.rs`

Pure refactor — no behavior change. Adds a generic helper that will be used for `USER` and `PASS` suffixes in later tasks. Inlines the single use of the old name; removes `env_token_var`. The dead-code lint (`dead_code = "deny"` in `Cargo.toml`) means removing the helper is mandatory if no caller exists.

- [ ] **Step 1.1: Write the failing test** in `src/secrets.rs` (test module at the bottom of the file).

Append to the `#[cfg(test)] mod tests { ... }` block, near the other `env_*` tests:

```rust
#[test]
fn env_var_for_supports_arbitrary_suffix() {
    assert_eq!(env_var_for("dev", "TOKEN"), "RDC_TOKEN_DEV");
    assert_eq!(env_var_for("dev", "USER"), "RDC_USER_DEV");
    assert_eq!(env_var_for("dev", "PASS"), "RDC_PASS_DEV");
    assert_eq!(env_var_for("dev-ap", "USER"), "RDC_USER_DEV_AP");
    assert_eq!(env_var_for("prod-eu-west-1", "PASS"), "RDC_PASS_PROD_EU_WEST_1");
}
```

- [ ] **Step 1.2: Run the test; verify it fails**

Run: `cargo test -p rdc --lib secrets::tests::env_var_for_supports_arbitrary_suffix`
Expected: `error[E0425]: cannot find function `env_var_for` in this scope` (or similar — function doesn't exist yet).

- [ ] **Step 1.3: Implement `env_var_for`; remove `env_token_var`**

Replace this block in `src/secrets.rs`:

```rust
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
```

with:

```rust
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
```

In the same file, in `resolve_token_from`, change:

```rust
    let env_var = env_token_var(env);
```

to:

```rust
    let env_var = env_var_for(env, "TOKEN");
```

Update the four existing tests that reference `env_token_var`:

```rust
#[test]
fn env_token_var_uppercases_and_keeps_alphanumerics() {
    assert_eq!(env_var_for("dev", "TOKEN"), "RDC_TOKEN_DEV");
    assert_eq!(env_var_for("PROD", "TOKEN"), "RDC_TOKEN_PROD");
    assert_eq!(env_var_for("staging42", "TOKEN"), "RDC_TOKEN_STAGING42");
}

#[test]
fn env_token_var_maps_hyphen_to_underscore() {
    assert_eq!(env_var_for("dev-ap", "TOKEN"), "RDC_TOKEN_DEV_AP");
    assert_eq!(env_var_for("prod-eu-west-1", "TOKEN"), "RDC_TOKEN_PROD_EU_WEST_1");
}

#[test]
fn env_token_var_preserves_existing_underscores() {
    assert_eq!(env_var_for("dev_ap", "TOKEN"), "RDC_TOKEN_DEV_AP");
}

#[test]
fn env_token_var_collision_between_hyphen_and_underscore_is_documented() {
    assert_eq!(env_var_for("dev-ap", "TOKEN"), env_var_for("dev_ap", "TOKEN"));
}
```

- [ ] **Step 1.4: Run all secrets tests; verify they pass**

Run: `cargo test -p rdc --lib secrets::`
Expected: all secrets tests pass, including the new `env_var_for_supports_arbitrary_suffix`.

- [ ] **Step 1.5: Run the full build (catch any other call sites)**

Run: `cargo build --all-targets`
Expected: clean build. If any external caller of `env_token_var` exists (none do as of this writing — verified with `grep -rn env_token_var src/`), update it.

- [ ] **Step 1.6: Commit**

```bash
git add src/secrets.rs
git commit -m "refactor(secrets): generalize env_token_var to env_var_for(suffix)"
```

---

### Task 2: Introduce `TokenLookup` enum + parse `expires_at`

**Files:**
- Modify: `src/secrets.rs`

Introduce the sync-side resolution shape that the async wrapper (Task 5) will consume. Refactor `resolve_token_from` to return `TokenLookup` instead of `Result<String>`. Add `expires_at` parsing to the on-disk schema (no expiry check yet — that lands in Task 3). Keep the public `resolve_token` returning `Result<String>` for now by wrapping the lookup; this lets all existing callers compile unchanged at this checkpoint.

- [ ] **Step 2.1: Write the failing test** for `TokenLookup`

Append to `#[cfg(test)] mod tests` in `src/secrets.rs`:

```rust
#[test]
fn lookup_returns_cached_with_expires_at_from_file() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    std::fs::write(
        dir.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"abc","expires_at":1700000000}"#,
    )
    .unwrap();
    let lookup = resolve_token_lookup_from(dir.path(), "dev", |_| None).unwrap();
    match lookup {
        TokenLookup::Cached { token, expires_at } => {
            assert_eq!(token, "abc");
            assert_eq!(expires_at, Some(1700000000));
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
            assert!(message.contains("secrets/dev.secrets.json"), "missing message: {message}");
        }
        other => panic!("expected Missing, got {other:?}"),
    }
}
```

- [ ] **Step 2.2: Run the tests; verify they fail to compile**

Run: `cargo test -p rdc --lib secrets::tests::lookup_returns_cached_with_expires_at_from_file`
Expected: compile error — `TokenLookup` does not exist; `resolve_token_lookup_from` does not exist.

- [ ] **Step 2.3: Implement `TokenLookup` and `resolve_token_lookup_from`**

In `src/secrets.rs`, add near the top of the file (after the `env_var_for` function):

```rust
/// Outcome of synchronously inspecting the per-env credential
/// configuration (env vars + on-disk secrets file). The async
/// [`resolve_token`] consumes this enum and performs I/O (HTTP login,
/// cache write) when needed.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenLookup {
    /// A token is ready to use. `expires_at` is `Some(unix_secs)` when
    /// the file recorded one (token came from `/v1/auth/login` and rdc
    /// computed the expiry at issue time); `None` when the token came
    /// from `RDC_TOKEN_<ENV>` or from a manual `rdc auth --token`
    /// (opaque tokens; no expiry tracking).
    Cached {
        token: String,
        expires_at: Option<u64>,
    },
    /// Nothing is configured. `message` is the actionable error to
    /// surface to the user, naming all three options
    /// (`$RDC_TOKEN_<ENV>`, `$RDC_USER_<ENV>+$RDC_PASS_<ENV>`,
    /// `rdc auth <env>`).
    Missing { message: String },
}
```

Replace the existing `resolve_token` + `resolve_token_from` block with this. (Note: `resolve_token` becomes a thin wrapper that's still sync at this checkpoint — we make it async in Task 5.)

```rust
/// Inspect the per-env credential state and report a [`TokenLookup`].
///
/// Resolution order:
/// 1. `RDC_TOKEN_<ENV>` env var — used as-is, opaque (no expiry tracking).
/// 2. `secrets/<env>.secrets.json` (`{api_token, expires_at?}`) — used as-is.
///
/// Returns `TokenLookup::Missing` if neither is configured.
///
/// **Note:** This is the sync inspection step. Expiry checks and the
/// `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` resolution branch land in Task 3.
pub fn resolve_token_lookup(project_root: &Path, env: &str) -> Result<TokenLookup> {
    resolve_token_lookup_from(project_root, env, |k| std::env::var(k).ok())
}

/// Inner form with an injectable env-getter. Lets tests cover the
/// env-var branch without mutating the process-wide environment.
fn resolve_token_lookup_from<F: Fn(&str) -> Option<String>>(
    project_root: &Path,
    env: &str,
    get_env: F,
) -> Result<TokenLookup> {
    let token_var = env_var_for(env, "TOKEN");
    if let Some(t) = get_env(&token_var) {
        if !t.is_empty() {
            return Ok(TokenLookup::Cached { token: t, expires_at: None });
        }
    }

    let path = project_root.join("secrets").join(format!("{env}.secrets.json"));
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
        if f.api_token.is_empty() {
            return Ok(TokenLookup::Missing {
                message: format!(
                    "{} has empty api_token; set ${token_var} or fill in the file",
                    path.display()
                ),
            });
        }
        return Ok(TokenLookup::Cached { token: f.api_token, expires_at: f.expires_at });
    }

    Ok(TokenLookup::Missing {
        message: format!(
            "no token for env '{env}': set ${token_var} or write {}",
            path.display()
        ),
    })
}

/// Convenience wrapper that converts a [`TokenLookup`] back into the
/// `Result<String>` shape the existing callers expect. **Sync at this
/// checkpoint — becomes async in Task 5 when the login flow is wired up.**
pub fn resolve_token(project_root: &Path, env: &str) -> Result<String> {
    match resolve_token_lookup(project_root, env)? {
        TokenLookup::Cached { token, .. } => Ok(token),
        TokenLookup::Missing { message } => Err(anyhow!(message)),
    }
}
```

Update the existing tests that called `resolve_token_from` (they now need to call `resolve_token_lookup_from` and destructure):

```rust
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
            assert!(message.contains("secrets/unittest_c.secrets.json"), "should mention file path: {message}");
        }
        other => panic!("expected Missing, got {other:?}"),
    }
}

#[test]
fn resolve_token_uses_normalized_env_var_for_hyphenated_env() {
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
```

- [ ] **Step 2.4: Run all secrets tests; verify they pass**

Run: `cargo test -p rdc --lib secrets::`
Expected: all secrets tests pass.

- [ ] **Step 2.5: Run the full build to confirm no caller breaks**

Run: `cargo build --all-targets`
Expected: clean build. Existing `resolve_token` callers still compile because the wrapper returns `Result<String>` as before.

- [ ] **Step 2.6: Commit**

```bash
git add src/secrets.rs
git commit -m "refactor(secrets): introduce TokenLookup enum; parse expires_at"
```

---

### Task 3: Expiry check + `RDC_USER` / `RDC_PASS` resolution branch

**Files:**
- Modify: `src/secrets.rs`

Extend `TokenLookup` with a `NeedsLogin` variant. `resolve_token_lookup_from` now:
- Returns `Cached` only when the cache is present AND `expires_at` is either absent or `> now + 60s`.
- Falls through to checking `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` when there's no usable cache.
- Returns `NeedsLogin` when both creds env vars are set.
- Returns `Missing` (with an actionable message naming all three options) when nothing is configured.

The `resolve_token` wrapper at this checkpoint errors on `NeedsLogin` (still sync; we plumb the login in Task 5).

- [ ] **Step 3.1: Write the failing tests**

Append to `#[cfg(test)] mod tests` in `src/secrets.rs`. Note the new `now` parameter on `resolve_token_lookup_from` — it's an injected clock for deterministic tests.

```rust
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
```

- [ ] **Step 3.2: Run; verify failure (variant doesn't exist)**

Run: `cargo test -p rdc --lib secrets::tests::lookup_creds_only_no_cache_returns_needs_login`
Expected: compile error — `TokenLookup::NeedsLogin` does not exist; `resolve_token_lookup_from_at` does not exist.

- [ ] **Step 3.3: Extend the enum and the inner resolver**

In `src/secrets.rs`, replace the `TokenLookup` enum definition with:

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum TokenLookup {
    /// A token is ready to use.
    Cached { token: String, expires_at: Option<u64> },
    /// `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are both set and the cache
    /// is missing/expired. Caller (async `resolve_token`) should call
    /// `api::login` and persist the result.
    NeedsLogin { username: String, password: String },
    /// Nothing is configured. `message` is the actionable error to
    /// surface, naming all three options.
    Missing { message: String },
}
```

Add a public constant for the skew window near the enum:

```rust
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
```

Replace `resolve_token_lookup_from` with:

```rust
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
```

Keep the public `resolve_token` wrapper as-is for now (sync; errors on `NeedsLogin`). Replace its body with:

```rust
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
```

(This wrapper is what callers still use at this checkpoint; the `NeedsLogin` branch becomes unreachable in Task 5 when we make `resolve_token` async and route the login there. For now it acts as a guard so the build stays green even if a caller's env vars accidentally trigger that path.)

- [ ] **Step 3.4: Run all secrets tests**

Run: `cargo test -p rdc --lib secrets::`
Expected: all new + existing tests pass.

- [ ] **Step 3.5: Build full crate**

Run: `cargo build --all-targets`
Expected: clean build.

- [ ] **Step 3.6: Commit**

```bash
git add src/secrets.rs
git commit -m "feat(secrets): expiry check + RDC_USER/RDC_PASS resolution branch"
```

---

### Task 4: Add `api::login()` function

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`

A free async function (not a method on `RossumClient`, because login doesn't need a token). Sends `POST /v1/auth/login` with `{username, password}` and returns the `key` from the response. Routes through the existing `retry::send_with_retry` for 429/502/503/504 resilience.

- [ ] **Step 4.1: Write the failing integration test**

Open `tests/api.rs` and check its current shape. If it already uses `wiremock`, add a new module; if not, add the wiremock setup. (Pattern: see existing tests like `tests/cli_sync.rs` for wiremock usage.)

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn login_posts_credentials_and_returns_key() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .and(matchers::body_json(serde_json::json!({
            "username": "alice@example.com",
            "password": "hunter2",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh-token-abc",
            "domain": "example",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let api_base = format!("{}/v1", server.uri());
    let token = rdc::api::login(&api_base, "alice@example.com", "hunter2")
        .await
        .expect("login should succeed");
    assert_eq!(token, "fresh-token-abc");
}

#[tokio::test]
async fn login_propagates_401_on_bad_credentials() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "detail": "Invalid username/password.",
        })))
        .mount(&server)
        .await;

    let api_base = format!("{}/v1", server.uri());
    let err = rdc::api::login(&api_base, "alice@example.com", "wrong")
        .await
        .expect_err("login should fail on 401");
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "error should mention status: {msg}");
}
```

- [ ] **Step 4.2: Run; verify failure (function not found)**

Run: `cargo test --test api login_posts_credentials_and_returns_key -- --nocapture`
Expected: compile error — `rdc::api::login` not found.

- [ ] **Step 4.3: Implement `api::login`**

Append to `src/api/mod.rs` (after the `impl RossumClient { ... }` block):

```rust
/// Exchange username/password for an API token via
/// `POST /v1/auth/login`. Returns the issued `key`.
///
/// This is a free function rather than a method on [`RossumClient`]
/// because login doesn't take a token (it produces one). Used by
/// `secrets::resolve_token` to obtain a fresh token when the cache is
/// missing/expired and `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are set,
/// and by `cli::auth::run` to handle `rdc auth <env> --username <u>`.
///
/// Retries on transient 429/502/503/504 via [`retry::send_with_retry`].
/// 401 is **not** retried (a bad password isn't going to fix itself).
pub async fn login(api_base: &str, username: &str, password: &str) -> Result<String> {
    let http = Client::builder()
        .tcp_nodelay(true)
        .build()
        .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))?;
    let url = format!("{api_base}/auth/login");
    let body = serde_json::json!({
        "username": username,
        "password": password,
    });
    let progress = ProgressHandle::silent();
    let resp = retry::send_with_retry(
        || http.post(&url).json(&body),
        &format!("POST {url}"),
        progress.clone(),
        None, // no rate limiter — login is rare
    )
    .await?;
    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return Err(ApiError::Status {
            status: status.as_u16(),
            body: body_text,
            env: None,
        }
        .into());
    }
    #[derive(Deserialize)]
    struct LoginResponse {
        key: String,
    }
    let parsed: LoginResponse = resp
        .json()
        .await
        .with_context(|| format!("decoding login response from {url}"))?;
    Ok(parsed.key)
}
```

If `ProgressHandle::silent()` doesn't exist, replace with whatever constructor `retry::ProgressHandle` already exposes for non-progress callers — check `src/api/retry.rs` and use the existing pattern.

- [ ] **Step 4.4: Run the integration tests; verify pass**

Run: `cargo test --test api login_`
Expected: both `login_posts_credentials_and_returns_key` and `login_propagates_401_on_bad_credentials` pass.

- [ ] **Step 4.5: Full build**

Run: `cargo build --all-targets`
Expected: clean.

- [ ] **Step 4.6: Commit**

```bash
git add src/api/mod.rs tests/api.rs
git commit -m "feat(api): add login() for username/password -> token exchange"
```

---

### Task 5: Make `resolve_token` async; integrate login + cache write; migrate callers

**Files:**
- Modify: `src/secrets.rs`
- Modify: `src/cli/sync/mod.rs`
- Modify: `src/cli/deploy/run.rs`
- Modify: `src/cli/deploy/apply.rs`
- Modify: `src/cli/diff.rs`
- Modify: `src/cli/repair/store_anomaly.rs`

The atomic "switch the rails" task. After this commit, `resolve_token` is async, takes `api_base`, and silently logs in via `api::login` + caches the result when `TokenLookup::NeedsLogin` is the resolution outcome. Every caller is updated in the same commit so the build stays green.

- [ ] **Step 5.1: Write the failing integration test**

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn resolve_token_logs_in_when_creds_set_and_no_cache() {
    use tempfile::TempDir;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh-from-login",
            "domain": "example",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    // No secrets file, no RDC_TOKEN_*. Set RDC_USER_DEV / RDC_PASS_DEV.
    // Use a unique env name per test to avoid env-var collisions across
    // the test runner's parallel jobs.
    let env_name = format!("login_e2e_{}", uuid_like_suffix());
    std::env::set_var(&format!("RDC_USER_{}", env_name.to_uppercase()), "alice");
    std::env::set_var(&format!("RDC_PASS_{}", env_name.to_uppercase()), "hunter2");

    let api_base = format!("{}/v1", server.uri());
    let token = rdc::secrets::resolve_token(dir.path(), &env_name, &api_base)
        .await
        .expect("resolve_token should login and return a fresh token");
    assert_eq!(token, "fresh-from-login");

    // The login result must be persisted with an expires_at.
    let raw = std::fs::read_to_string(
        dir.path().join("secrets").join(format!("{env_name}.secrets.json")),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["api_token"], "fresh-from-login");
    assert!(
        parsed["expires_at"].is_number(),
        "expires_at must be persisted, got {parsed}"
    );

    std::env::remove_var(&format!("RDC_USER_{}", env_name.to_uppercase()));
    std::env::remove_var(&format!("RDC_PASS_{}", env_name.to_uppercase()));
}

#[tokio::test]
async fn resolve_token_uses_cache_when_valid_no_login_call() {
    use tempfile::TempDir;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    // Far-future expires_at -> cache is valid.
    std::fs::write(
        dir.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"cached-token","expires_at":99999999999}"#,
    )
    .unwrap();

    let api_base = format!("{}/v1", server.uri());
    let token = rdc::secrets::resolve_token(dir.path(), "dev", &api_base)
        .await
        .expect("resolve_token should use the cache");
    assert_eq!(token, "cached-token");
    // The wiremock expect(0) above asserts no POST was made.
}

// Small helper to produce a unique-ish env name per test invocation.
// Doesn't need to be cryptographically unique — just enough to keep
// parallel test jobs from stomping on each other's env vars.
fn uuid_like_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}
```

- [ ] **Step 5.2: Run; verify failure**

Run: `cargo test --test api resolve_token_logs_in_when_creds_set_and_no_cache`
Expected: compile error — `resolve_token` is sync, doesn't take `api_base`.

- [ ] **Step 5.3: Make `resolve_token` async; route `NeedsLogin` through `api::login`; write cache**

In `src/secrets.rs`, replace the existing `pub fn resolve_token` with:

```rust
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
                .with_context(|| format!("logging in to env '{env}' with RDC_USER_*/RDC_PASS_*"))?;
            let expires_at = now_unix_secs().saturating_add(LOGIN_TOKEN_LIFETIME_SECS);
            write_secrets_file(project_root, env, &token, Some(expires_at))?;
            Ok(token)
        }
        TokenLookup::Missing { message } => Err(anyhow!(message)),
    }
}

/// Write `secrets/<env>.secrets.json` atomically with mode 0600 on
/// Unix. Used by [`resolve_token`] when caching a login-derived
/// token, and by `cli::auth::validate_and_save_token` when the user
/// runs `rdc auth`.
pub fn write_secrets_file(
    project_root: &Path,
    env: &str,
    token: &str,
    expires_at: Option<u64>,
) -> Result<PathBuf> {
    let path = project_root.join("secrets").join(format!("{env}.secrets.json"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = match expires_at {
        Some(exp) => serde_json::json!({ "api_token": token, "expires_at": exp }),
        None => serde_json::json!({ "api_token": token }),
    };
    let mut bytes = serde_json::to_vec_pretty(&body).context("serializing token JSON")?;
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
```

Add the `PathBuf` import at the top of `src/secrets.rs` if it isn't there:

```rust
use std::path::{Path, PathBuf};
```

- [ ] **Step 5.4: Migrate the six call sites**

Replace `let token = resolve_token(&cwd, env)?;` (or the equivalent) at each of these locations with the async, three-arg form. The exact env-config variable name at each site is named in the snippet; if it differs, use the local one that holds the `EnvConfig` for the relevant env.

`src/cli/sync/mod.rs` line ~209 (single env):

```rust
let token = resolve_token(&cwd, env, &env_cfg.api_base).await?;
```

`src/cli/deploy/run.rs` lines ~103, ~108 (src + tgt envs):

```rust
let src_token = resolve_token(&cwd, src, &src_cfg.api_base).await?;
// ...
let tgt_token = resolve_token(&cwd, tgt, &tgt_cfg.api_base).await?;
```

`src/cli/deploy/apply.rs` line ~238 (tgt env):

```rust
let token = resolve_token(&cwd, tgt, &tgt_cfg.api_base).await?;
```

`src/cli/diff.rs` line ~69 (one env):

```rust
let token = resolve_token(cwd, env, &env_cfg.api_base).await?;
```

`src/cli/repair/store_anomaly.rs` line ~80:

```rust
let token = resolve_token(&cwd, env, &env_cfg.api_base).await?;
```

If any call site doesn't have `env_cfg` in scope, load it locally:

```rust
let cfg = crate::config::ProjectConfig::load(&cwd.join("rdc.toml"))?;
let env_cfg = cfg.envs.get(env).ok_or_else(|| anyhow::anyhow!("env '{env}' is not defined in rdc.toml"))?;
let token = resolve_token(&cwd, env, &env_cfg.api_base).await?;
```

(Most call sites already construct `env_cfg` or `cfg` for the API client below; reuse that.)

- [ ] **Step 5.5: Run cargo build to catch any missed call site**

Run: `cargo build --all-targets`
Expected: clean build. If a compile error names a missing `.await` or a wrong arity, that's a missed migration — fix it.

- [ ] **Step 5.6: Run the integration tests**

Run: `cargo test --test api resolve_token_`
Expected: both new tests pass.

- [ ] **Step 5.7: Run the full test suite to confirm no regression**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 5.8: Commit**

```bash
git add src/secrets.rs src/cli/sync/mod.rs src/cli/deploy/run.rs src/cli/deploy/apply.rs src/cli/diff.rs src/cli/repair/store_anomaly.rs tests/api.rs
git commit -m "feat(secrets): async resolve_token with auto-login on RDC_USER/RDC_PASS"
```

---

### Task 6: `rdc auth <env> --username <u>` CLI surface

**Files:**
- Modify: `src/cli/mod.rs`
- Modify: `src/cli/auth.rs`
- Modify: `tests/cli_auth.rs`

Add the `--username` flag, mutually exclusive with `--token`. On TTY, prompt for the password via `inquire::Password`. On non-TTY, read from stdin. Call `api::login`, validate via `GET /organizations/{org_id}`, write `{api_token, expires_at}` via the new `write_secrets_file`.

- [ ] **Step 6.1: Write the failing tests**

Append to `tests/cli_auth.rs`:

```rust
#[test]
fn auth_username_and_token_are_mutually_exclusive() {
    use assert_cmd::Command;
    use predicates::str::contains;

    let dir = tempfile::tempdir().unwrap();
    // Project with one env.
    std::fs::write(
        dir.path().join("rdc.toml"),
        r#"
name = "fixture"
[envs.dev]
api_base = "https://example.rossum.app/api/v1"
org_id = 1
"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["auth", "dev", "--username", "alice", "--token", "T"])
        .assert()
        .failure()
        .stderr(contains("--username").and(contains("--token")));
}

#[tokio::test]
async fn auth_username_logs_in_and_writes_token_with_expires_at() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "minted-by-login",
            "domain": "example",
        })))
        .mount(&server)
        .await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "name": "Example Org",
            "url": format!("{}/v1/organizations/1", server.uri()),
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rdc.toml"),
        format!(
            r#"
name = "fixture"
[envs.dev]
api_base = "{}/v1"
org_id = 1
"#,
            server.uri()
        ),
    )
    .unwrap();

    use assert_cmd::Command;
    // Run blocking from inside a tokio test: use spawn_blocking or
    // assert_cmd directly (it's blocking). Channel the password via stdin.
    let output = tokio::task::spawn_blocking({
        let dir_path = dir.path().to_path_buf();
        move || {
            Command::cargo_bin("rdc")
                .unwrap()
                .current_dir(&dir_path)
                .args(["auth", "dev", "--username", "alice"])
                .write_stdin("hunter2\n")
                .assert()
                .success()
        }
    })
    .await
    .unwrap();
    let _ = output;

    let raw = std::fs::read_to_string(dir.path().join("secrets/dev.secrets.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["api_token"], "minted-by-login");
    assert!(parsed["expires_at"].is_number(), "expires_at must be present: {parsed}");
}
```

- [ ] **Step 6.2: Run; verify failure**

Run: `cargo test --test cli_auth auth_username_and_token_are_mutually_exclusive`
Expected: failure (the flag doesn't exist or isn't wired up).

- [ ] **Step 6.3: Update `Command::Auth` in `src/cli/mod.rs`**

Find `Command::Auth { env, token, ... }` (around line 188) and update to:

```rust
/// Set or refresh an env's API token. Validates the token before
/// writing to `secrets/<env>.secrets.json` (mode 0600 on Unix).
///
/// Provide credentials via one of:
/// * `--token <T>` — explicit token (CI-friendly).
/// * `--username <U>` — exchanges <U> + password (stdin or TTY
///   prompt) for a token via POST /v1/auth/login; the token and
///   computed expiry (162h from now) are written to the secrets file.
/// * Neither — read a token from stdin (back-compat with today).
///
/// Without `<env>`, picks interactively from envs defined in `rdc.toml`.
Auth {
    #[arg(add = ArgValueCandidates::new(env_name_candidates))]
    env: Option<String>,
    #[arg(long, conflicts_with = "username")]
    token: Option<String>,
    #[arg(long, conflicts_with = "token")]
    username: Option<String>,
},
```

In the `match cli.command` block, update the Auth arm (around line 320):

```rust
Some(Command::Auth { env, token, username }) => {
    let env = crate::cli::env_picker::pick_env("Set token for which env?", env)?;
    crate::cli::auth::run(&env, token, username).await
}
```

- [ ] **Step 6.4: Update `cli::auth::run` to handle `--username`**

In `src/cli/auth.rs`, replace the entire `pub async fn run(...)` with:

```rust
pub async fn run(env: &str, token_arg: Option<String>, username_arg: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let paths = Paths::for_env(&cwd, env);

    let outcome = if let Some(username) = username_arg {
        // --username flow: prompt or read password, login, persist.
        let password = read_password_for_login()?;
        let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
        log.event(
            Action::Auth,
            &format!("logging in as '{username}' to {}", env_cfg.api_base),
        );
        let token = crate::api::login(&env_cfg.api_base, &username, &password)
            .await
            .with_context(|| format!("logging in to env '{env}'"))?;
        // Validate against the org endpoint, same as the --token flow.
        let org_name = validate_token(env_cfg, &token).await?;
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_add(crate::secrets::LOGIN_TOKEN_LIFETIME_SECS);
        crate::secrets::write_secrets_file(&cwd, env, &token, Some(expires_at))?;
        format!(
            "saved token to {} (org '{org_name}', expires in ~162h)",
            paths.secrets_file().display()
        )
    } else {
        // Existing --token (or stdin) flow.
        let new_token = match token_arg {
            Some(t) => t,
            None => {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)
                    .context("reading token from stdin")?;
                let trimmed = buf.trim().to_string();
                if trimmed.is_empty() {
                    return Err(anyhow!(
                        "no token provided; pass `--token <value>`, pipe a token via stdin, \
                         or use `--username <U>` to log in with credentials"
                    ));
                }
                trimmed
            }
        };
        let org_name = validate_and_save_token(env_cfg, &paths.secrets_file(), &new_token).await?;
        format!(
            "saved token to {} (org '{org_name}')",
            paths.secrets_file().display()
        )
    };

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    log.event(Action::Auth, &outcome);
    Ok(())
}

/// Read the password for `--username` login.
/// - Stdin piped: read line, trim.
/// - TTY: prompt via inquire::Password (masked).
fn read_password_for_login() -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        use inquire::{Password, PasswordDisplayMode};
        let pw = Password::new("Password")
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .with_help_message("Ctrl+C to cancel")
            .prompt()
            .map_err(|e| anyhow!("password prompt failed: {e}"))?;
        if pw.is_empty() {
            return Err(anyhow!("empty password"));
        }
        Ok(pw)
    } else {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading password from stdin")?;
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            return Err(anyhow!(
                "no password provided; pipe one on stdin (`echo $PASS | rdc auth ...`) or run on a TTY"
            ));
        }
        Ok(trimmed)
    }
}

/// Validate a token by hitting `GET /organizations/<id>`. Returns the
/// organization's name on success.
///
/// Extracted from `validate_and_save_token` so the `--username` flow can
/// validate the freshly-issued token without re-using the save logic
/// (the save goes through `secrets::write_secrets_file` to include
/// `expires_at`).
async fn validate_token(env_cfg: &EnvConfig, token: &str) -> Result<String> {
    let client = RossumClient::new(env_cfg.api_base.clone(), token.to_string())
        .context("constructing Rossum API client")?;
    let progress = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    progress.event(Action::Auth, &format!("validating token (GET /organizations/{})", env_cfg.org_id));
    let org = client
        .get_organization(env_cfg.org_id, Some(progress.clone()))
        .await
        .with_context(|| {
            format!(
                "validating token against {}/organizations/{}",
                env_cfg.api_base, env_cfg.org_id
            )
        })?;
    progress.event(Action::Auth, &format!("done validated against org '{}'", org.name));
    Ok(org.name)
}
```

Add the `RossumClient` import at the top of `cli/auth.rs` if not already present:

```rust
use crate::api::RossumClient;
```

- [ ] **Step 6.5: Run the new tests**

Run: `cargo test --test cli_auth auth_username_and_token_are_mutually_exclusive`
Expected: pass.

Run: `cargo test --test cli_auth auth_username_logs_in_and_writes_token_with_expires_at`
Expected: pass.

- [ ] **Step 6.6: Run the full test suite**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 6.7: Commit**

```bash
git add src/cli/mod.rs src/cli/auth.rs tests/cli_auth.rs
git commit -m "feat(auth): rdc auth <env> --username <u> with login + cache"
```

---

### Task 7: Silent re-login on non-TTY mid-run 401

**Files:**
- Modify: `src/cli/auth.rs`
- Modify: `tests/cli_auth.rs` (or `tests/api.rs`)

Extend `refresh_token_interactively` so that, on a non-TTY context, it first checks whether `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are set. If yes, it silently calls `api::login`, writes the cache via `write_secrets_file`, and returns Ok — the caller's `with_401_retry` will then re-execute the failed operation. If no, it falls through to the existing error.

- [ ] **Step 7.1: Write the failing integration test**

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn refresh_token_silent_relogin_on_non_tty_with_creds() {
    use tempfile::TempDir;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "relogin-fresh",
            "domain": "example",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "name": "Example Org",
            "url": format!("{}/v1/organizations/1", server.uri()),
        })))
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("rdc.toml"),
        format!(
            r#"
name = "fixture"
[envs.dev]
api_base = "{}/v1"
org_id = 1
"#,
            server.uri()
        ),
    )
    .unwrap();

    let env_suffix = uuid_like_suffix();
    let env_name = format!("relogin_{env_suffix}");
    // Re-write rdc.toml with the unique env name so set_var collisions
    // across parallel tests don't matter.
    std::fs::write(
        dir.path().join("rdc.toml"),
        format!(
            r#"
name = "fixture"
[envs.{env_name}]
api_base = "{}/v1"
org_id = 1
"#,
            server.uri()
        ),
    )
    .unwrap();
    let user_var = format!("RDC_USER_{}", env_name.to_uppercase());
    let pass_var = format!("RDC_PASS_{}", env_name.to_uppercase());
    std::env::set_var(&user_var, "alice");
    std::env::set_var(&pass_var, "hunter2");

    // Force "non-TTY" by chdir-ing into the temp dir and calling the
    // function in a way that doesn't simulate a terminal. Since the
    // current test process's stdin is typically piped (not a TTY) when
    // running under cargo test, refresh_token_interactively's
    // IsTerminal check will return false naturally.
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let result = rdc::cli::auth::refresh_token_interactively(&env_name).await;
    std::env::set_current_dir(&cwd).unwrap();
    std::env::remove_var(&user_var);
    std::env::remove_var(&pass_var);

    result.expect("silent relogin should succeed");
    let raw = std::fs::read_to_string(
        dir.path().join("secrets").join(format!("{env_name}.secrets.json")),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["api_token"], "relogin-fresh");
    assert!(parsed["expires_at"].is_number());
}
```

- [ ] **Step 7.2: Run; verify failure (current code errors on non-TTY)**

Run: `cargo test --test api refresh_token_silent_relogin_on_non_tty_with_creds`
Expected: test fails — current `refresh_token_interactively` returns an error on non-TTY immediately, before considering creds.

- [ ] **Step 7.3: Extend `refresh_token_interactively`**

In `src/cli/auth.rs`, replace the function body's early non-TTY return with a creds check first.

Find:

```rust
pub async fn refresh_token_interactively(env: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "token for env '{env}' was rejected (401). \
             Re-run on a TTY to refresh interactively, or run \
             `rdc auth {env} --token <new-token>`."
        ));
    }
    // ... existing TTY prompt loop ...
}
```

Replace the non-TTY block with:

```rust
pub async fn refresh_token_interactively(env: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        // Non-TTY: try silent re-login from RDC_USER_<ENV> + RDC_PASS_<ENV>
        // before erroring. This is the CI / cron path.
        let user_var = crate::secrets::env_var_for(env, "USER");
        let pass_var = crate::secrets::env_var_for(env, "PASS");
        let user_opt = std::env::var(&user_var).ok().filter(|s| !s.is_empty());
        let pass_opt = std::env::var(&pass_var).ok().filter(|s| !s.is_empty());
        if let (Some(username), Some(password)) = (user_opt, pass_opt) {
            let cwd = std::env::current_dir().context("getting current directory")?;
            let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
            let env_cfg = cfg
                .envs
                .get(env)
                .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
            let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
            log.event(
                Action::Auth,
                &format!("token for env '{env}' rejected (401); silent re-login from ${user_var}"),
            );
            let token = crate::api::login(&env_cfg.api_base, &username, &password)
                .await
                .with_context(|| format!("silent re-login for env '{env}'"))?;
            let expires_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_add(crate::secrets::LOGIN_TOKEN_LIFETIME_SECS);
            crate::secrets::write_secrets_file(&cwd, env, &token, Some(expires_at))?;
            log.event(Action::Auth, &format!("silent re-login OK for env '{env}'"));
            return Ok(());
        }
        return Err(anyhow!(
            "token for env '{env}' was rejected (401). \
             Re-run on a TTY to refresh interactively, set ${user_var} + ${pass_var} \
             for silent re-login, or run `rdc auth {env} --token <new-token>`."
        ));
    }
    // ... existing TTY prompt loop (unchanged) ...
```

Keep the rest of the function (the TTY prompt loop) unchanged.

- [ ] **Step 7.4: Run the new integration test**

Run: `cargo test --test api refresh_token_silent_relogin_on_non_tty_with_creds`
Expected: pass.

- [ ] **Step 7.5: Run the full test suite**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 7.6: Commit**

```bash
git add src/cli/auth.rs tests/api.rs
git commit -m "feat(auth): silent re-login on non-TTY 401 when RDC_USER/RDC_PASS set"
```

---

### Task 8: GitLab CI template

**Files:**
- Create: `templates/gitlab-ci-archival.yml`

A copy-paste artifact. No code; no automated tests. The README section in Task 9 covers the smoke-test recipe.

- [ ] **Step 8.1: Verify the `templates/` directory exists or create it**

Run: `ls templates/ 2>/dev/null || mkdir templates`
Expected: directory exists after this command.

- [ ] **Step 8.2: Create the template file**

Create `templates/gitlab-ci-archival.yml` with this content:

```yaml
# Daily archival sync for a Rossum environment managed with rdc.
#
# Copy this file to .gitlab-ci.yml at the root of your Rossum project
# repo (the one that holds rdc.toml + envs/). Then:
#   1. Set CI variables in Settings -> CI/CD -> Variables (masked, protected):
#        RDC_USER_<ENV>  - system username for the env
#        RDC_PASS_<ENV>  - system password for the env
#      Variable name suffix follows rdc's env-name normalization:
#      uppercase, non-alphanumerics -> '_'. E.g. env 'prod' -> RDC_USER_PROD.
#   2. Add a Schedule in Settings -> CI/CD -> Schedules (e.g. '0 2 * * *' UTC).
#   3. Adjust RDC_ENV and RDC_VERSION below.
#   4. In Settings -> CI/CD -> Token Permissions, allow the project's
#      CI_JOB_TOKEN to push to the repo (or swap the push line for a PAT).

variables:
  RDC_ENV: "prod"
  RDC_VERSION: "v0.1.0"  # pin to a release from
                          # https://github.com/mrtnzlml/rdc/releases

stages:
  - archive

archive:
  stage: archive
  image: alpine:3.20
  rules:
    - if: $CI_PIPELINE_SOURCE == "schedule"
    - if: $CI_PIPELINE_SOURCE == "web"  # allow manual trigger
  before_script:
    - apk add --no-cache curl git
    - curl -fsSL "https://github.com/mrtnzlml/rdc/releases/download/${RDC_VERSION}/rdc-x86_64-unknown-linux-gnu.tar.gz" | tar xz -C /usr/local/bin
    - rdc --version
    - git config user.name  "rdc-archive[bot]"
    - git config user.email "rdc-archive@noreply"
  script:
    - rdc sync "$RDC_ENV" --no-push --yes --force-overwrite-drift
    - |
      git add -A "envs/$RDC_ENV" ".rdc/state/$RDC_ENV.lock.json"
      if git diff --staged --quiet; then
        echo "No changes in $RDC_ENV; nothing to archive."
        exit 0
      fi
      git commit -m "chore(archive): daily sync of $RDC_ENV ($(date -u +%Y-%m-%d))"
      git push "https://oauth2:${CI_JOB_TOKEN}@${CI_SERVER_HOST}/${CI_PROJECT_PATH}.git" "HEAD:${CI_DEFAULT_BRANCH}"

# Multi-env extension: replace the single job above with parallel:matrix:.
# Set RDC_USER_<ENV> + RDC_PASS_<ENV> CI variables for each env name.
#
# archive:
#   parallel:
#     matrix:
#       - RDC_ENV: [test, prod]
```

- [ ] **Step 8.3: Lint the YAML**

Run: `python3 -c "import yaml; yaml.safe_load(open('templates/gitlab-ci-archival.yml'))"`
Expected: no output (parses cleanly).

- [ ] **Step 8.4: Commit**

```bash
git add templates/gitlab-ci-archival.yml
git commit -m "feat(templates): GitLab CI for daily archival sync"
```

---

### Task 9: README documentation — env-var table + CI section

**Files:**
- Modify: `README.md`

Update the Authentication section to use a table that includes all the rdc-relevant env vars. Add a new "Daily archive in GitLab CI" subsection that links to `templates/gitlab-ci-archival.yml`.

- [ ] **Step 9.1: Update the Authentication section**

Find this block in `README.md` (around line 580–600):

```markdown
## Authentication

Token resolution per env, in priority order:

1. `RDC_TOKEN_<ENV_UPPER>` environment variable. Recommended for CI. The env name is uppercased and any non-alphanumeric character is normalized to `_` so the shell can export it: `test` → `RDC_TOKEN_TEST`, `dev-ap` → `RDC_TOKEN_DEV_AP`.
2. `secrets/<env>.secrets.json` — `{"api_token": "..."}`. Recommended locally. `rdc init` adds `secrets/` to `.gitignore`.

Set or rotate:

```sh
rdc auth test --token <new-token>
```

Validates against `GET /organizations/{org_id}` before writing the file (mode 0600 on Unix). For pipe input that keeps the token out of shell history:

```sh
read -s T && echo "$T" | rdc auth test
```
```

Replace it with:

```markdown
## Authentication

Per-env credential resolution, in priority order:

1. `RDC_TOKEN_<ENV>` env var — used as-is (opaque, no expiry tracking).
2. `secrets/<env>.secrets.json` — cached token from a previous `rdc auth` or auto-login; reused until `expires_at` (if recorded).
3. `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` env vars — rdc calls `POST /v1/auth/login` and caches the result in (2).

`<ENV>` is the env name from `rdc.toml`, uppercased, with non-alphanumerics replaced by `_`. So `test` → `RDC_TOKEN_TEST`, `dev-ap` → `RDC_USER_DEV_AP`.

### Env vars rdc reads

| Var | Purpose |
|---|---|
| `RDC_TOKEN_<ENV>` | API token override. Highest priority. Use for CI when a long-lived token exists. |
| `RDC_USER_<ENV>` | System username. Paired with `RDC_PASS_<ENV>`. Used when tokens are too short-lived (e.g. support-team accounts) — rdc calls `POST /v1/auth/login` to exchange for a fresh token, cached in `secrets/<env>.secrets.json` with a computed expiry. |
| `RDC_PASS_<ENV>` | System password. Paired with `RDC_USER_<ENV>`. Both must be set together. |
| `RDC_REPAIR_CURE` | Non-TTY cure selection for `rdc repair --fix-store-anomaly`: `reinstall`, `skip`, or default `convert`. |
| `EDITOR` | Editor opened for `[e]` in the conflict resolver. Defaults to `vi`. Standard OS env var. |
| `NO_COLOR` | When non-empty, disables ANSI color output. Honors [no-color.org](https://no-color.org). |

### Set or rotate

By token (CI-friendly, validates against `GET /organizations/{org_id}` before writing):

```sh
rdc auth test --token <new-token>
```

By username + password (calls `POST /v1/auth/login`, validates, caches the result with computed expiry):

```sh
rdc auth test --username alice@example.com
# password is prompted on TTY (masked), or piped on stdin:
echo "$RDC_PASS_TEST" | rdc auth test --username alice@example.com
```

For pipe input that keeps a token out of shell history:

```sh
read -s T && echo "$T" | rdc auth test
```

When `secrets/<env>.secrets.json` contains an `expires_at` and the cache is past expiry, rdc auto-refreshes by calling `/v1/auth/login` again — provided `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are set. Otherwise the next command surfaces an actionable error pointing at the three options.
```

- [ ] **Step 9.2: Add a "Daily archive in GitLab CI" subsection**

Append this section to the README, placing it after the existing "Resilience" section (or wherever CI-adjacent docs naturally live):

```markdown
## Daily archive in GitLab CI

For an automated daily snapshot of a Rossum env (pull-only, committed to the repo so the git history *is* the archive), copy `templates/gitlab-ci-archival.yml` into a Rossum project repo as `.gitlab-ci.yml`.

Setup:

1. Set CI variables (Settings → CI/CD → Variables, masked + protected):
   - `RDC_USER_<ENV>` — system username for the env.
   - `RDC_PASS_<ENV>` — system password for the env.
2. Add a schedule (Settings → CI/CD → Schedules), e.g. `0 2 * * *` UTC.
3. In Settings → CI/CD → Token Permissions, allow the project's `CI_JOB_TOKEN` to push to the repo (or replace the push command in the template with a personal access token).

The job:

- Downloads a pinned `rdc` release tarball from GitHub (`RDC_VERSION` variable in the template).
- Runs `rdc sync "$RDC_ENV" --no-push --yes --force-overwrite-drift`. `--no-push` keeps the sync pull-only; `--force-overwrite-drift` makes the snapshot reflect any out-of-band remote edits silently (archival semantics — the remote is canonical).
- Commits `envs/<env>/` + `.rdc/state/<env>.lock.json` to the default branch when there's a diff. The `rules:` block restricts the job to `schedule` and `web` triggers, so the self-commit doesn't recursively trigger the archive.

Smoke test after setup:

1. Trigger via Settings → CI/CD → Schedules → "Play".
2. Check the job log for `rdc sync`'s summary line.
3. Confirm a commit appears on the default branch (or "No changes in `<env>`; nothing to archive." if the remote hasn't drifted).

Deliberately out of scope: tagging, per-day branches, artifact uploads, retry-on-transient-login-failure beyond rdc's built-in 429/502/503/504 retry. Layer those on as needed.
```

- [ ] **Step 9.3: Verify the README renders cleanly**

Run (if `pandoc` is installed): `pandoc -f markdown -t html README.md > /dev/null`
Expected: exits 0.

Alternatively, eyeball the diff with `git diff README.md` and confirm headings, table syntax, and code fences look right.

- [ ] **Step 9.4: Run the full test suite once more to confirm nothing regressed**

Run: `cargo test && cargo build --all-targets`
Expected: all tests pass; clean build.

- [ ] **Step 9.5: Commit**

```bash
git add README.md
git commit -m "docs(readme): env-var table + GitLab CI daily-archive section"
```

---

## Self-review

**Spec coverage (against `docs/superpowers/specs/2026-05-25-gitlab-ci-archival-sync-design.md`):**

| Spec section | Implementing task |
|---|---|
| Native user/pass auth — secrets file schema (`expires_at` field) | Task 2 |
| CLI surface (`rdc auth <env> --username <u>`, mutual exclusion, stdin/TTY) | Task 6 |
| Env vars (`RDC_USER_<ENV>`, `RDC_PASS_<ENV>`, "both required") | Task 3 |
| Resolution flow (steps 1–4) | Task 3 (sync inspection) + Task 5 (async wrapper + login) |
| Mid-run 401 silent re-login + retry-once | Task 7 (silent re-login on non-TTY); the retry-once is inherited from existing `with_401_retry` in `cli/mod.rs` |
| `api::login` free function | Task 4 |
| `env_var_for(env, suffix)` generalization | Task 1 |
| GitLab CI template | Task 8 |
| README env-var table + CI subsection | Task 9 |
| Testing (unit, integration, CLI, manual smoke) | Tasks 1–9 each ship their own tests; manual smoke documented in Task 9 |
| Backwards compatibility (no expires_at = no-expiry) | Tested in Task 2 (`lookup_returns_cached_without_expires_at_when_field_absent`) |

**Placeholder scan:** No TBD / TODO / "implement later" / "similar to Task N" patterns remain. Every step has either the test code, the implementation code, or the exact command.

**Type consistency:** `TokenLookup` variants (`Cached { token, expires_at }`, `NeedsLogin { username, password }`, `Missing { message }`) are used consistently across Tasks 2–5. `LOGIN_TOKEN_LIFETIME_SECS` is referenced in Tasks 5–7 from `crate::secrets`. `write_secrets_file(project_root, env, token, expires_at: Option<u64>)` signature is consistent across Tasks 5, 6, 7. The new `cli::auth::run` signature `(env, token, username)` is consistent between Task 6 and the `Command::Auth` arm.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-25-gitlab-ci-archival-sync.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
