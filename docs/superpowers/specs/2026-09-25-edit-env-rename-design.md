# `rdc edit env rename` — design

Date: 2026-09-25. Status: approved in conversation (surface + scope); this
document adds the architecture for review.

## 1. Goal

Rename an environment of an rdc project from the CLI, with the same
implementation the desktop app uses. It is the first action of a new umbrella
verb, `rdc edit`, meant to hold local project maintenance: later env
removal, creating records, renaming records.

Success means:

- `rdc edit env rename dev sandbox` leaves a project that `rdc sync sandbox`,
  `rdc migrate sandbox prod` and `rdc init` treat exactly as if the env had
  always been called `sandbox`.
- The desktop app's rename goes through the same core function and gains its
  behaviour (CI rewrite, target-exists refusal, cross-process lock).
- No code path renames an env in two different ways.

## 2. Command surface

```
rdc edit env rename <old> <new> [--dry-run]
```

- `edit` is a new top-level verb. Its first letter is free, so
  `every_verb_starts_with_a_distinct_letter` (`tests/cli_misc.rs`) keeps
  passing and `rdc e` becomes a valid prefix.
- `env` is a noun group; `rename` its only action for now. Clap nests them as
  `Command::Edit { command: EditCommand }` → `EditCommand::Env { command:
  EnvCommand }` → `EnvCommand::Rename { old, new, dry_run }`.
- `<old>` gets the existing `env_name_candidates` completion. Both positionals
  are required; there is no picker, because a rename is never a guess.
- `--dry-run` prints the full report and writes nothing, not even the lock.

Offline. The Rossum org, its objects and the token do not change, so no sync
is needed afterwards.

## 3. What a rename touches

Local, moved (each only if it exists):

| Path | Note |
| --- | --- |
| `envs/<old>/` | includes `overlay.toml` and code sidecars |
| `secrets/<old>.secrets.json` | |
| `secrets/<old>.hook-secrets.json` | |
| `.rdc/state/<old>.lock.json` | lockfile; content names no env |
| `.rdc/state/<old>.base/` | base cache |
| `.rdc/conflicts/<old>/` | conflict shadows |

`.rdc/state/<old>.lock` (the advisory `EnvLock`) is **not** moved. The
rename holds it for the whole run and deletes it after releasing; the next
sync of `<new>` creates its own.

Local, rewritten:

- `rdc.toml`: `[envs.<old>]` → `[envs.<new>]`, via `ProjectConfig::save`.
  Hand-written comments are lost, the same as with `rdc init --env` today.
- `.rdc/mapping.toml`: `GenericMapping::rename_env`.
- `README.md`, `CLAUDE.md`: their `rdc:` regions re-rendered from the new
  env set with `render_doc_regions` + `regions::splice`. A file with no
  markers, or absent, is left alone; rename never creates a scaffold.
- `.gitlab-ci.yml`, if present and markered — see §4.

Outside the repo, reported but not changed: the GitLab CI variable
`RDC_TOKEN_<OLD>` (and `RDC_USER_<OLD>` / `RDC_PASS_<OLD>` if used; names
from `secrets::env_var_for`, so `dev-us` gives `RDC_TOKEN_DEV_US`) must be
renamed by hand, and GitLab starts a fresh environment history for `<new>`.
These follow-ups are printed only when the project has a `.gitlab-ci.yml`.

## 4. `.gitlab-ci.yml`

The pipeline has two regions that are spliced differently (see the repo
`CLAUDE.md`). The rename works on the on-disk text in three steps:

1. **Rewrite `<old>` → `<new>` inside both regions.** Within
   `rdc:deploy-jobs` and `rdc:archive-envs` only, on lines of the form
   `key: value` (and the job-key line), replace a scalar that equals `<old>`
   — plain or double-quoted — and the job key `deploy:<old>` (plain or
   quoted). This covers `RDC_ENV`, `RDC_SRC`, `resource_group`,
   `environment.name`, the job key, and the archive matrix's `RDC_ENV`.
   Substrings are never touched: `RDC_SRC: "dev-us"` survives a rename of
   `dev`. Every changed line is recorded for the report as
   `(job, key, old value, new value)`.
2. **Splice** with `render_regions_for_existing(rewritten_text, new_envs)`.
   Because step 1 already put `<new>` into the on-disk archive region,
   `merge_deploy_jobs` sees `<new>` as offered and appends **no** draft; the
   archive matrix (including `RDC_VAR_SUFFIX`) is re-rendered from
   `rdc.toml`.
3. **Warn** (no change) for each line outside the regions where `<old>`
   still appears as a whole word. The static half belongs to the project.

A file with no markers is left alone and produces a warning that the
pipeline is not rdc-managed. A splice error (broken markers) aborts the whole
rename during planning, before anything moves.

## 5. Architecture

Core lives in `src/cli/edit/env.rs` (with `src/cli/edit/mod.rs` for clap
dispatch), next to the other verbs and reachable from the desktop crate as
`rdc::cli::edit::env`, as `rdc::cli::sync::embed` already is.

```rust
pub struct RenameReport {
    pub old: String,
    pub new: String,
    pub dry_run: bool,
    pub moved: Vec<(PathBuf, PathBuf)>,     // project-relative
    pub rewritten: Vec<PathBuf>,            // rdc.toml, mapping, docs, CI
    pub ci_changes: Vec<CiChange>,          // job, key, from, to
    pub warnings: Vec<String>,
    pub follow_ups: Vec<String>,            // GitLab work rdc cannot do
}

pub fn rename_env(root: &Path, old: &str, new: &str, dry_run: bool)
    -> Result<RenameReport>;
```

`rename_env` runs in two phases.

**Plan (no writes).**

1. Validate both names with `config::valid_env_name` (moved into core from
   the desktop bridge; `init`'s prompt uses it too). `new` is trimmed.
2. Load `rdc.toml`; require `old` present, `new` absent, `new != old`, and
   no other env whose `env_var_for` suffix equals `<new>`'s (`dev-us` vs
   `dev_us` would share `RDC_TOKEN_DEV_US`; the init wizard refuses the same
   collision).
3. Unless `dry_run`, acquire `EnvLock` on `<old>` with a zero wait. A held
   lock fails with "a sync is running on `<old>`". The lock guard lives until
   `rename_env` returns.
4. Require every target path in §3 to be absent. Refuse naming the first one
   found. (Today's bridge silently overwrites a leftover secrets file.)
5. Compute, in memory, the new contents of `rdc.toml`, `mapping.toml`,
   `README.md`, `CLAUDE.md`, `.gitlab-ci.yml`, keeping each original text.
   Any parse or splice error returns here.

`dry_run` returns the report now.

**Apply.**

1. Move the §3 paths, recording each completed move.
2. Write mapping, `README.md`, `CLAUDE.md`, `.gitlab-ci.yml` with
   `write_atomic`.
3. Write `rdc.toml` last — the authoritative record that `<new>` exists.
4. Release the lock; delete `.rdc/state/<old>.lock` (best-effort).

Any failure in apply rolls back: restore each rewritten file from its
original text, then reverse the completed moves in reverse order. Rollback
errors are appended to the returned error rather than swallowed. Unlike the
bridge today, no move is "best-effort": a failed state or conflicts move is
an error that triggers rollback.

## 6. CLI output

```
$ rdc edit env rename dev sandbox
renamed dev -> sandbox
  moved     envs/dev -> envs/sandbox
  moved     secrets/dev.secrets.json -> secrets/sandbox.secrets.json
  ...
  rewrote   rdc.toml, .rdc/mapping.toml, README.md, CLAUDE.md, .gitlab-ci.yml
  pipeline  deploy:dev -> deploy:sandbox
            deploy:test RDC_SRC "dev" -> "sandbox"

still to do in GitLab:
  rename the CI variable RDC_TOKEN_DEV to RDC_TOKEN_SANDBOX
  GitLab keeps environment "dev"'s history; "sandbox" starts a new one
```

`--dry-run` prints `would rename dev -> sandbox` and the same body. Warnings
go to stderr. Exit code 0 on success, 1 on any refusal or failure.

## 7. Desktop adoption

- **Bridge.** `desktop/rust/src/api/rdc.rs::rename_env` becomes a thin
  wrapper: call `rdc::cli::edit::env::rename_env(folder, old, new, false)`,
  then `discover::inspect`. Its own move/rollback code is deleted. It returns
  a new `RenameEnvResult { project: ProjectSummary, follow_ups: Vec<String>,
  warnings: Vec<String> }`, so the FRB bindings are regenerated and committed.
  `valid_env_name` in the bridge is replaced by the core one for `add_env`,
  `remove_env` and `add_project` as well.
- **Dart.** `AppState.editEnvEntry` returns the rename's follow-ups and
  warnings (empty when no rename happened). `EditConnectionDialog` pops with
  them, and its caller shows one snackbar listing them when non-empty. The
  existing `canRenameEnv` mid-sync guard stays as fast UI feedback; the core
  lock is now the real guard and also covers a CLI sync in another process.
- Error texts that bridge tests and the UI rely on ("letters, digits", "no
  \"<old>\" environment", "already exists") keep their wording in core.

## 8. Testing

Core (`src/cli/edit/env.rs`, unit):

- Happy path: every §3 path moved, `rdc.toml` + mapping rewritten, report
  lists them.
- Refusals: invalid `old`/`new`, unknown `old`, existing `new`, `new == old`,
  a credential-variable collision with another env,
  each target-path class already present, lock held by another `EnvLock`.
  Each asserts the tree is byte-identical afterwards.
- Rollback: a failing write mid-apply (target made unwritable by placing a
  directory at a file path) restores the tree byte-for-byte.
- `--dry-run` leaves the tree byte-identical, including no `.lock` file.
- CI: from the committed template example, rename `dev` → `sandbox` with a
  hand-filled `deploy:test` `RDC_SRC: "dev"`: the job key and values are
  rewritten, `RDC_SRC: "dev-us"` elsewhere is untouched, no draft is
  appended, the archive region equals what `render_regions` produces for the
  new env set, and
  everything outside the markers is byte-identical.
- Docs: regions equal `render_doc_regions(new_envs)`; unmarkered and absent
  files are untouched.
- No `.gitlab-ci.yml` → no follow-ups.

CLI (`tests/`): an end-to-end `rdc edit env rename` on a temp project,
asserting the printed report; `--dry-run` writes nothing. The existing
`command_references`, `the_cli_exposes_no_hidden_verbs` and distinct-letter
tests must pass unchanged.

Desktop: the bridge's `rename_env_*` tests shrink to wrapper checks (happy
path + one validation message + follow-ups returned); the rollback test
moves to core. A widget test for the snackbar. `bridge_test.dart`'s rename
round-trip stays green. Full suite before done: `cargo test`, `cargo clippy
-D warnings`, `cargo doc`, desktop `cargo test`, `flutter analyze`,
`flutter test`.

## 9. Docs

README gets an `rdc edit` section, and any place that lists the verbs gains
the seventh. `tests/command_references.rs` already checks that every
`` `rdc <verb>`` in the repo parses.

## 10. Out of scope

- `edit env remove` / `edit env add`. Note for later: the bridge's
  `remove_env` also leaves `secrets/<env>.hook-secrets.json` behind.
- Anything about records.
- Preserving comments in `rdc.toml` (would need `toml_edit`; `init` has the
  same limitation).
