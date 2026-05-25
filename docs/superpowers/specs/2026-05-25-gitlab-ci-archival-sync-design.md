# GitLab CI archival sync + native username/password auth

Status: draft
Date: 2026-05-25

## Overview

Add a daily archival workflow for rdc-managed Rossum environments, delivered as two coupled pieces:

1. **Native username/password auth in rdc.** A third token-resolution branch alongside the existing `RDC_TOKEN_<ENV>` env var and `secrets/<env>.secrets.json` token: when `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are present, rdc exchanges them via `POST /v1/auth/login`, caches the issued token plus its computed expiry in the same `secrets/<env>.secrets.json` it already uses, and reuses the cached token until it expires. A new `rdc auth <env> --username <u>` subcommand mirrors today's `--token` flow for local use.
2. **GitLab CI template.** A copy-paste `templates/gitlab-ci-archival.yml` (referenced from README) that runs daily, downloads a pinned rdc release tarball, runs `rdc sync <env> --no-push --yes --force-overwrite-drift`, and commits the result to the default branch. Uses the new env vars; no token storage on the CI side.

Motivation: Rossum support-team users get short-lived tokens by policy, so a long-lived `RDC_TOKEN_<ENV>` baked into CI variables is not viable. Username + password are durable system credentials; storing those in the CI variable store and re-deriving a token at runtime sidesteps the lifetime constraint. The CI template needs the rdc feature; the rdc feature stands on its own (also useful for local support workflows where the operator types their password rather than copying a transient token from the UI).

## Success criteria

1. `rdc auth <env> --username <u>` reads the password from stdin (or prompts via `inquire::Password` on a TTY), calls `POST /v1/auth/login`, validates the issued token with `GET /organizations/{org_id}`, and writes `{api_token, expires_at}` to `secrets/<env>.secrets.json` (mode 0600).
2. Any `rdc <cmd> <env>` invocation with `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` set, no `RDC_TOKEN_<ENV>` override, and no usable cached token (missing or expired) transparently logs in once, caches the result, and proceeds.
3. A cached token past `expires_at`, or a mid-run 401 against a cached token, triggers a silent re-login when creds are available; otherwise surfaces an actionable error naming all three options.
4. The GitLab CI template runs end-to-end in a fresh project repo after the user (a) sets two CI variables and (b) adds a cron schedule.

## Native username/password auth in rdc

### Secrets file schema (additive)

```json
{ "api_token": "abc...", "expires_at": 1762344000 }
```

`expires_at` is an optional integer: Unix epoch seconds in UTC. Format chosen to match the project's existing approach of using `std::time::SystemTime` arithmetic without adding `chrono`/`time` deps (see `src/log.rs:106` for the comment that explicitly documents this stance). It is set only when the token was obtained via `/v1/auth/login` (when rdc has computed an expiry). Manually-set tokens (`rdc auth <env> --token …`) continue to write the same body as today — just `{api_token}` with no `expires_at` — so existing files keep working without migration. The reader treats absence of `expires_at` as "no expiry tracking, use as-is".

### CLI surface

`rdc auth <env> --username <u>` mirrors today's `--token`:

- Mutually exclusive with `--token` (clap `conflicts_with`).
- Password is read from stdin when piped (`echo $PASS | rdc auth <env> --username <u>`), or prompted via `inquire::Password` on a TTY. The inquire dep is already wired up in `cli/auth.rs::refresh_token_interactively`.
- Calls `POST /v1/auth/login` with `{username, password}` (no `max_token_lifetime_s` — let the server use its 162h default).
- Validates the returned token via `GET /organizations/{org_id}` (reuses the existing validation block in `cli/auth.rs::validate_and_save_token`).
- Writes `{api_token: <key from login>, expires_at: now_utc() + 162h}` atomically through `snapshot::writer::write_atomic`, mode 0600 on Unix.

### Env vars

`RDC_USER_<ENV>` and `RDC_PASS_<ENV>`. Both must be present together; one alone is a resolution error ("only RDC_USER_PROD is set; also set RDC_PASS_PROD"). The name suffix follows the existing normalization (uppercase, non-alphanumerics → `_`). The current `env_token_var(env)` helper in `secrets.rs` becomes a more general `env_var_for(env, suffix)` and the three call sites (`TOKEN`, `USER`, `PASS`) use it.

### Resolution flow

Per `rdc` invocation, `resolve_token` returns the token to use:

1. **`RDC_TOKEN_<ENV>` set and non-empty** → return it. (Unchanged from today; no expiry tracking. Admin-supplied opaque tokens stay opaque.)
2. **`secrets/<env>.secrets.json` exists with `api_token`**:
   - No `expires_at`, or `expires_at > now + 60s` skew → return the cached token.
   - Else (expired or expiring within 60s) → treat as "no cached token", fall through.
3. **Both `RDC_USER_<ENV>` and `RDC_PASS_<ENV>` set** → call `POST /v1/auth/login`, compute `expires_at = now + 162h`, atomically write `{api_token, expires_at}` back to `secrets/<env>.secrets.json` (mode 0600), return the fresh token.
4. **Otherwise** → actionable error naming all three options:
   > no token for env '<env>': set $RDC_TOKEN_<ENV>, set $RDC_USER_<ENV> + $RDC_PASS_<ENV>, or run `rdc auth <env>`

### Mid-run 401

Today's behavior on 401: TTY → interactive token prompt; non-TTY → error pointing at `rdc auth`. Extended:

- Before falling into either path, if `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are set, silently re-login, update the cache, retry the failed call **once**.
- Retry-once, not loop — a second 401 after a fresh login is a real auth failure (revoked user, wrong password) and must surface immediately.

### Files touched

| File | Change |
|---|---|
| `src/secrets.rs` | Schema extension (`expires_at` field), parse + skew check, `env_var_for` generalization, new test cases. |
| `src/cli/auth.rs` | `--username` flag, login path, mutual exclusion with `--token`, stdin-vs-TTY password handling. |
| `src/api/mod.rs` | New `login(api_base, username, password)` free function (doesn't need a token, so doesn't fit on `RossumClient`). Returns the issued key. |
| `src/cli/resolve.rs` | New resolution branch (step 3 above) + the 401 auto-refresh hook. |
| `tests/cli_auth.rs` | New cases: `--username`, mutual exclusion, expiry, auto-refresh. |
| `README.md` | New env-var table in the Authentication section; new "Daily archive in GitLab CI" subsection pointing at the template. |

### Why this is safe

- **Back-compat.** Existing secrets files without `expires_at` keep working (treated as no-expiry). No migration step.
- **No new files or directories.** Cache lives in the existing secrets file.
- **rdc never extends a token's lifetime.** `expires_at` is computed at issue time from the Rossum-documented default lifetime (162h). The login response (`{key, domain}`) doesn't return the actual expiry, so this is an assumption: if the server's policy caps the lifetime shorter (e.g. an org-level restriction on support-team accounts), the cache will think the token is still valid past its real expiry. The mid-run 401 path catches that — one wasted API call per affected `rdc` invocation, then a silent re-login. Acceptable for an archival cron.
- **CI is a no-op for caching.** In a fresh CI workspace (no secrets dir), the first command logs in once, caches in-process, and the workspace is discarded on cleanup. Same outcome as the simplest design; the caching only matters for repeated local invocations.

## GitLab CI template

### Location

`templates/gitlab-ci-archival.yml`. A documented copy-paste artifact, not executed against this repo. Linked from a new README subsection.

### Template content

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

### Flag justifications

| Flag | Why |
|---|---|
| `--no-push` | Audit mode. Pull-only — never sends local edits to the remote. The README already documents this as the CI audit pattern. |
| `--yes` | Non-interactive: skip the destructive-section confirm and any other TTY prompt. |

**Note on `--force-overwrite-drift`:** Originally specified for `sync` to achieve archival semantics (remote is canonical, out-of-band edits silently adopted), but that flag is currently `deploy`-only. Spec relaxed to accept skip-with-warning shadow files in the rare case of human-edited archive branch. If the archive branch is write-protected and only the CI job commits to it, skips are unreachable.

### Auth at runtime

No `rdc auth` step in the CI job. The CI variables `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are picked up directly by `rdc sync` via the new resolution branch. The login + token cache happens transparently inside the first API call rdc makes during the sync.

### Push-back mechanism

`https://oauth2:${CI_JOB_TOKEN}@${CI_SERVER_HOST}/${CI_PROJECT_PATH}.git`. `CI_JOB_TOKEN` is GitLab's built-in token for the running job. The project needs `Settings → CI/CD → Token Permissions → Job token` configured so the project's own token can push to itself. No long-lived PAT.

### Avoiding push loops

The `rules:` block restricts the job to `schedule` and `web` triggers. The commit the job itself produces is a `push` event, which would normally re-trigger the pipeline — but since this job only runs on `schedule` or `web`, the re-trigger fires no archive job. Other jobs in the same pipeline (lint, test, deploy on push) are unaffected.

### Out of scope

- No tagging.
- No per-day archive branches; single linear history on `${CI_DEFAULT_BRANCH}`.
- No artifact upload (the repo IS the archive).
- No retry on transient login failures beyond what rdc itself already does (the `api/mod.rs` retry policy covers 429/502/503/504).

The README subsection pointing at the template will explicitly call out these omissions as deliberate, so a project that wants any of them knows it's layering on.

## Documentation

### README env-var table

Add to the Authentication section (replacing today's two-bullet list):

| Env var | Purpose | Notes |
|---|---|---|
| `RDC_TOKEN_<ENV>` | API token override. | Highest priority. Use for CI when a long-lived token exists. |
| `RDC_USER_<ENV>` | System username, paired with `RDC_PASS_<ENV>`. | Use when tokens are too short-lived (e.g. support-team accounts). rdc calls `POST /v1/auth/login` to exchange for a fresh token, cached in `secrets/<env>.secrets.json` with computed expiry. |
| `RDC_PASS_<ENV>` | System password, paired with `RDC_USER_<ENV>`. | Both must be set together. |
| `RDC_REPAIR_CURE` | Non-TTY cure selection for `rdc repair` anomaly prompts. | Values: `reinstall`, `skip`, default `convert`. |
| `EDITOR` | Editor opened for `[e]` in the conflict resolver. | Defaults to `vi`. Standard OS env var. |
| `NO_COLOR` | When non-empty, disables ANSI color output. | Honors the [no-color.org](https://no-color.org) convention. |

`<ENV>` is the env name from `rdc.toml`, uppercased, with non-alphanumerics replaced by `_` (e.g. `dev-ap` → `RDC_TOKEN_DEV_AP`).

The env-var name normalization paragraph already in README stays as the canonical reference and is linked from each `<ENV>` row.

A new "Daily archive in GitLab CI" subsection links to `templates/gitlab-ci-archival.yml` and lists the smoke-test recipe (set vars, trigger via Schedules → "Play", verify commit).

## Testing

### Unit tests (`src/secrets.rs`)

- `env_var_for(env, suffix)` produces the right names for `TOKEN`, `USER`, `PASS`, including the hyphen→underscore normalization (e.g. `(dev-ap, "USER")` → `RDC_USER_DEV_AP`).
- `expires_at` parsing: ISO-8601 UTC accepted; malformed surfaces a parse error pointing at the file path.
- Expiry check: `expires_at > now + 60s` → token used; `<= now + 60s` → fall-through.
- Missing `expires_at` → no-expiry behavior (back-compat).
- New end-to-end resolution cases:
  - token env-var wins over cache + creds.
  - non-expired cache wins over creds.
  - expired cache falls through to creds.
  - one creds var set, other missing → error names the missing var.

### Integration tests with `wiremock` (already a dev-dep)

- `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` set, no cache, no token env var → first invocation issues exactly one `POST /v1/auth/login`; the secrets file ends up with `{api_token, expires_at}`.
- Second invocation with the cache populated → no `POST /v1/auth/login`.
- Mid-run 401 with creds present → silent re-login (one `POST /v1/auth/login`), failed request retries once and succeeds.
- Mid-run 401 → second 401 after fresh login → error surfaces, no infinite retry.

### CLI tests (`tests/cli_auth.rs`)

- `rdc auth <env> --username u` with password piped via stdin → calls login, validates, writes file with `expires_at`.
- `rdc auth <env> --username u --token t` → clap rejects (mutual exclusion).
- `rdc auth <env> --username u` on a non-TTY with no stdin → actionable error pointing at the stdin pipe pattern.

### Manual smoke for the template

Not automated; documented in README:

1. Copy `templates/gitlab-ci-archival.yml` into a fresh sandbox Rossum project repo as `.gitlab-ci.yml`.
2. Set `RDC_USER_PROD` + `RDC_PASS_PROD` as masked + protected CI variables.
3. Create a schedule (or trigger via Settings → CI/CD → Schedules → "Play").
4. Verify: job downloads rdc, calls login, syncs, either commits diff or logs no-op, and the commit appears on the default branch.

### What is explicitly not tested

- Real Rossum API (existing tests use wiremock; no live calls).
- GitLab Runner behavior (no GitLab in the rdc test loop).
- The exact rdc release tarball URL (the template pins `RDC_VERSION`; the user owns version selection).

## Backwards compatibility

- Existing `secrets/<env>.secrets.json` files keep working — missing `expires_at` is treated as no-expiry, same as a manually-set token.
- `rdc auth <env> --token T` is unchanged.
- `RDC_TOKEN_<ENV>` is unchanged and still wins over everything.
- Existing CI workflows that already use `RDC_TOKEN_<ENV>` are untouched.

## Open questions

None at design time.

## References

- `README.md` — Authentication section, `--no-push` audit mode, `--force-overwrite-drift`, env-name normalization rules.
- `src/secrets.rs` — current `env_token_var`, `resolve_token`, `resolve_token_from`.
- `src/cli/auth.rs` — current `--token` flow and `validate_and_save_token`.
- Rossum API: `POST /v1/auth/login` returns `{key, domain}`; `max_token_lifetime_s` default 162h.
