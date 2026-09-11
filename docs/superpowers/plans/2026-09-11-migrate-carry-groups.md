# `--carry` groups and target-owned queue automation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop `rdc migrate` from promoting a queue's automation configuration across organizations, and replace the two per-field opt-in booleans with one `--carry <GROUP>` option whose values are the groups of fields the target env owns.

**Architecture:** Three layers, each already present in the codebase. (1) A `Carry` value object resolved from a clap `ValueEnum`, threaded through `migrate::run` → `run_at` → `transform_file` in place of the two `bool` parameters. (2) `reconcile_training_enabled` generalized into `reconcile_target_owned_keys(value, kind, tgt_path, keys)` and called twice — unconditionally for `training_enabled`, and gated on `!carry.automation` for the three automation keys. (3) README restructured so the three groups are documented as one concept. No snapshot, lockfile, overlay or mapping format changes.

**Tech Stack:** Rust 2024, clap 4 (derive + `ValueEnum`), `serde_json` with `preserve_order`, `tempfile` + `assert_cmd` in tests.

**Spec:** `docs/superpowers/specs/2026-09-11-migrate-carry-groups-design.md` — read it before Task 1. It carries the verified API facts this plan assumes.

## Global Constraints

- **rdc compiles slowly.** Batch every edit within a task and compile ONCE per red/green phase — not once per step. The steps below are written so that edits accumulate and a single `cargo test` closes each phase.
- **The compiler is the safety net for the signature change.** Changing `run` / `run_at` / `transform_file` arity makes every missed call site a hard error. There is no silent-miss failure mode; do not hand-audit what `cargo check` will tell you.
- **Never run repo-wide `cargo fmt`.** This checkout is not fmt-clean under the locally installed rustfmt; a repo-wide format produces an enormous unrelated diff. Format only what you wrote, by hand, matching the surrounding style.
- **`cargo clippy --all-targets -- -D warnings` gates the weekly release.** Run it once per task before committing.
- **Commit to local `main`. Never `git push`.** The user publishes.
- **The working tree is shared with another worker.** `git add` only the exact files a task names — never `git add -A`, never `git stash` / `reset` / `clean`.
- **No customer names or customer-specific identifiers** in code, tests, docs, fixtures, or commit messages. Use `acme` / `main` / `invoices` / `dev`-`test`-`prod` placeholders.
- Commit the feature with a `feat!` type so the weekly release derives a minor bump.

---

### Task 1: `Carry` / `CarryGroup` and the `--carry` option

A pure interface change: no reconcile behavior moves in this task. `--migrate-score-thresholds` and `--migrate-email-prefixes` are deleted and `--carry` replaces them, with `automation` already accepted as a group value that nothing reads yet (Task 2 wires it).

**Files:**
- Modify: `src/cli/migrate/mod.rs` — add the two types; replace the two `bool` params in `transform_file` (lines 877, 881), `run` (2639–2640, 2650–2651) and `run_at` (2666–2667); update the gates (1059, 1100) and the `transform_file` call site (2954, 2958); update the in-module test call sites
- Modify: `src/cli/mod.rs` — replace the two `#[arg]` declarations (the block ending at line 249) with one, and update the `Command::Migrate` destructure + `migrate::run` call (377–378, 393–394)
- Modify: `tests/cli_migrate.rs` — 37 `migrate::run(...)` call sites
- Test: `tests/cli_misc.rs` — new clap parse tests

**Interfaces:**
- Produces, for Task 2 and the tests:
  - `pub enum CarryGroup { ScoreThresholds, EmailPrefixes, Automation, All }` in `crate::cli::migrate` — clap renders the values as `score-thresholds`, `email-prefixes`, `automation`, `all`
  - `pub struct Carry { pub score_thresholds: bool, pub email_prefixes: bool, pub automation: bool }` with `Carry::NONE`, `Carry::SCORE_THRESHOLDS` and `pub fn from_groups(groups: &[CarryGroup]) -> Carry`
  - `pub fn run(src: &str, tgt: &str, mirror: bool, dry_run: bool, only: Vec<String>, carry: Carry) -> Result<()>`
  - `pub fn run_at(cwd: &Path, src: &str, tgt: &str, mirror: bool, dry_run: bool, only: Vec<String>, carry: Carry) -> Result<()>`
  - `fn transform_file(...)` takes `carry: Carry` in the 8th position (where `migrate_score_thresholds` was) and no longer takes `migrate_email_prefixes`

- [ ] **Step 1: Write the failing parse tests**

Append to `tests/cli_misc.rs` (it already has `use clap::Parser;` at the top):

```rust
#[test]
fn migrate_carry_accepts_a_single_group() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "automation",
    ]);
    assert!(cli.is_ok(), "--carry automation must parse: {:?}", cli.err());
}

#[test]
fn migrate_carry_accepts_comma_separated_groups() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "score-thresholds,automation",
    ]);
    assert!(
        cli.is_ok(),
        "one --carry may name several groups: {:?}",
        cli.err()
    );
}

#[test]
fn migrate_carry_accepts_a_repeated_flag() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "score-thresholds", "--carry", "automation",
    ]);
    assert!(cli.is_ok(), "--carry must be repeatable: {:?}", cli.err());
}

#[test]
fn migrate_carry_rejects_an_unknown_group() {
    // Names the valid set for the reader rather than failing anonymously —
    // clap's ValueEnum error does this for free, which is why the option is a
    // value enum instead of a hand-parsed string.
    let err = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "thresholds",
    ])
    .expect_err("an unknown group must be rejected");
    let msg = format!("{err}");
    assert!(msg.contains("score-thresholds"), "{msg}");
}

/// `--migrate-score-thresholds` was removed in favour of `--carry
/// score-thresholds`. A pipeline that bumps `RDC_VERSION` and still passes it
/// must fail at argument parse — before any file is written — not silently
/// migrate thresholds it meant to keep.
#[test]
fn migrate_rejects_the_removed_score_threshold_flag() {
    let result = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--migrate-score-thresholds",
    ]);
    assert!(result.is_err(), "the removed flag must not parse");
}

/// The email-prefix half of the same removal.
#[test]
fn migrate_rejects_the_removed_email_prefix_flag() {
    let result = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--migrate-email-prefixes",
    ]);
    assert!(result.is_err(), "the removed flag must not parse");
}
```

- [ ] **Step 2: Run them and watch all six fail**

Run: `cargo test --test cli_misc`
Expected: FAIL — the four `--carry` tests fail because the option does not exist, and both removed-flag tests fail because those flags still parse today.

- [ ] **Step 3: Add the two types**

In `src/cli/migrate/mod.rs`, directly above `pub fn run(` (line 2633), insert:

```rust
/// One group of fields the TARGET env owns on migrate, as named on the CLI by
/// `--carry <GROUP>`.
///
/// Every value is a field — or a small set of fields — that migrate leaves to
/// the target env by default, because it is tuned per organization rather than
/// promoted with the solution. Naming the group carries the SOURCE env's
/// values instead, which is the pre-reconcile behavior.
///
/// `training_enabled` is deliberately absent. It is always the target's, with
/// no opt-in: Rossum resets it to `false` on queue creation, so carrying it
/// would make every migrate+sync conflict, and there is no case for blindly
/// propagating a training toggle across orgs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CarryGroup {
    /// A schema datapoint's `score_threshold` and a queue's
    /// `default_score_threshold`.
    ScoreThresholds,
    /// An inbox's `email_prefix`.
    EmailPrefixes,
    /// A queue's `automation_enabled`, `automation_level` and
    /// `quality_spot_check_percentage`.
    Automation,
    /// Every group above. A group added later widens it, which is the intended
    /// reading of "carry everything the target normally owns".
    All,
}

/// The resolved set of [`CarryGroup`]s for one migrate run.
///
/// [`Carry::NONE`] — every group left to the target — is the default, and what
/// an unflagged `rdc migrate` uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Carry {
    pub score_thresholds: bool,
    pub email_prefixes: bool,
    pub automation: bool,
}

impl Carry {
    /// Carry nothing: the target env owns every group.
    pub const NONE: Carry = Carry {
        score_thresholds: false,
        email_prefixes: false,
        automation: false,
    };

    /// Carry only the score thresholds. Most migrate tests want this: it is the
    /// behavior that predates the threshold reconcile, so a test asserting on
    /// some unrelated field is not perturbed by threshold handling.
    pub const SCORE_THRESHOLDS: Carry = Carry {
        score_thresholds: true,
        email_prefixes: false,
        automation: false,
    };

    /// Resolve the repeated / comma-separated `--carry` values. Unknown values
    /// never reach here — clap rejects them against the [`CarryGroup`] enum.
    pub fn from_groups(groups: &[CarryGroup]) -> Self {
        let mut carry = Carry::NONE;
        for group in groups {
            match group {
                CarryGroup::ScoreThresholds => carry.score_thresholds = true,
                CarryGroup::EmailPrefixes => carry.email_prefixes = true,
                CarryGroup::Automation => carry.automation = true,
                CarryGroup::All => {
                    carry.score_thresholds = true;
                    carry.email_prefixes = true;
                    carry.automation = true;
                }
            }
        }
        carry
    }
}
```

- [ ] **Step 4: Re-sign `run`, `run_at` and `transform_file`**

In `src/cli/migrate/mod.rs`:

`run` (line 2633) and `run_at` (2659) — replace the trailing two parameters:

```rust
    only: Vec<String>,
    carry: Carry,
) -> Result<()> {
```

and `run`'s forwarding call (2650–2651) becomes a single `carry,`.

`transform_file` (line 869) — replace line 877 `migrate_score_thresholds: bool,` with `carry: Carry,` and DELETE line 881 `migrate_email_prefixes: bool,`. Keep the existing `#[allow(clippy::too_many_arguments)]`.

The two gates inside it:

```rust
    if !carry.score_thresholds
        && let Some((kind, _)) = classify(rel)
```

```rust
    if !carry.email_prefixes
        && let Some((kind, src_slug)) = classify(rel)
```

The `transform_file` call site (2954, 2958) — pass `carry,` in the 8th position and delete the `migrate_email_prefixes,` line.

- [ ] **Step 5: Swap the clap declaration**

In `src/cli/mod.rs`, delete both `#[arg]` blocks (the doc comment + attribute + field for `migrate_score_thresholds` and for `migrate_email_prefixes`, ending at line 249) and put this in their place:

```rust
        /// Carry a group of the target env's own fields from the source env
        /// instead. Repeatable and comma-separated:
        /// `--carry score-thresholds,automation`.
        ///
        /// By default migrate leaves each group to the TARGET env, because
        /// these are tuned per organization rather than promoted with the
        /// solution: a matched target keeps its own values, and a brand-new
        /// object drops the fields so the server's defaults apply.
        ///
        /// * `score-thresholds` — a datapoint's `score_threshold` and a
        ///   queue's `default_score_threshold`.
        /// * `email-prefixes` — an inbox's `email_prefix`, the left-hand side
        ///   of its public address (`<email_prefix>-<hash>@<host>`): carrying
        ///   it re-addresses the target's mailbox, so mail to the old address
        ///   stops arriving. A brand-new inbox keeps the source's regardless,
        ///   because the field is mandatory on create.
        /// * `automation` — a queue's `automation_enabled`,
        ///   `automation_level` and `quality_spot_check_percentage`.
        /// * `all` — every group above.
        ///
        /// To give a target env its own value deliberately, declare it in that
        /// env's `overlay.toml`: an overlay wins over both the reconcile and
        /// this flag.
        #[arg(
            long = "carry",
            value_name = "GROUP",
            value_enum,
            value_delimiter = ',',
            action = clap::ArgAction::Append
        )]
        carry: Vec<crate::cli::migrate::CarryGroup>,
```

Then the dispatch arm (around line 372): destructure `carry` instead of the two bools, and call

```rust
            crate::cli::migrate::run(
                &src,
                &tgt,
                mirror,
                dry_run,
                only,
                crate::cli::migrate::Carry::from_groups(&carry),
            )
```

- [ ] **Step 6: Update every call site mechanically**

`tests/cli_migrate.rs` needs the import first — add `use rdc::cli::migrate::Carry;` beside the other `use` lines at the top of the file. Then:

```bash
perl -pi -e 's/vec!\[\], true, false\)/vec![], Carry::SCORE_THRESHOLDS)/g; s/vec!\[\], false, false\)/vec![], Carry::NONE)/g; s/vec!\[\], false, true\)/vec![], Carry { email_prefixes: true, ..Carry::NONE })/g' tests/cli_migrate.rs
perl -pi -e 's/vec!\[\], false, false\)/vec![], Carry::NONE)/g' src/cli/migrate/mod.rs
```

That leaves the multi-line calls, which `perl` cannot see. Find them and fix each by hand:

```bash
grep -n "migrate::run($" tests/cli_migrate.rs      # 5 sites
grep -n "migrate_score_thresholds\|migrate_email_prefixes" src/cli/migrate/mod.rs
```

In the multi-line test calls, the last two arguments are `migrate_score_thresholds` then `migrate_email_prefixes`; collapse them to one `Carry::…` argument using the same mapping (`true, false` → `Carry::SCORE_THRESHOLDS`, `false, false` → `Carry::NONE`).

In `src/cli/migrate/mod.rs`'s own tests, the two `transform_file` calls carry named comments — `/* migrate_score_thresholds = */ true,` becomes `/* carry = */ Carry::SCORE_THRESHOLDS,` in the 8th position, and the `/* migrate_email_prefixes = */ false,` line is deleted.

Do not hand-audit for misses. `cargo check` reports every one as an arity error.

- [ ] **Step 7: Add the resolution tests the implementation now makes possible**

In `src/cli/migrate/mod.rs`'s `#[cfg(test)] mod tests`, beside the other reconcile unit tests:

```rust
    #[test]
    fn carry_from_groups_resolves_each_group() {
        assert_eq!(Carry::from_groups(&[]), Carry::NONE);
        assert_eq!(
            Carry::from_groups(&[CarryGroup::ScoreThresholds]),
            Carry::SCORE_THRESHOLDS
        );
        assert_eq!(
            Carry::from_groups(&[CarryGroup::Automation]),
            Carry { score_thresholds: false, email_prefixes: false, automation: true }
        );
    }

    #[test]
    fn carry_from_groups_unions_repeats_and_expands_all() {
        // Repeating a group is idempotent, and `all` means every group —
        // including any group added after this test was written.
        assert_eq!(
            Carry::from_groups(&[CarryGroup::Automation, CarryGroup::Automation]),
            Carry { score_thresholds: false, email_prefixes: false, automation: true }
        );
        assert_eq!(
            Carry::from_groups(&[CarryGroup::All]),
            Carry { score_thresholds: true, email_prefixes: true, automation: true }
        );
    }
```

And in `tests/cli_misc.rs`, the test that reads the parsed values back:

```rust
#[test]
fn migrate_carry_parses_into_the_named_groups() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "score-thresholds,automation",
    ])
    .expect("valid CLI");
    let Some(rdc::cli::Command::Migrate { carry, .. }) = cli.command else {
        panic!("expected Migrate variant");
    };
    assert_eq!(
        rdc::cli::migrate::Carry::from_groups(&carry),
        rdc::cli::migrate::Carry {
            score_thresholds: true,
            email_prefixes: false,
            automation: true,
        }
    );
}
```

- [ ] **Step 8: Build once and run the affected suites**

Run: `cargo test --test cli_misc --test cli_migrate --lib`
Expected: PASS. `cli_migrate`'s existing assertions are unchanged — this task moved no behavior, so any failure there is a botched call-site rewrite.

- [ ] **Step 9: Lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean. Do not run `cargo fmt`.

- [ ] **Step 10: Commit**

```bash
git add src/cli/mod.rs src/cli/migrate/mod.rs tests/cli_migrate.rs tests/cli_misc.rs
git commit -m "$(cat <<'EOF'
feat!: replace the per-field migrate flags with --carry <group>

`--migrate-score-thresholds` and `--migrate-email-prefixes` were one
boolean per field, and the set of fields the target env owns is still
growing — so the surface grew a flag every time. They are replaced by a
single `--carry <GROUP>` value option whose values are the groups
themselves, resolved into a `Carry` struct that is threaded where the two
booleans were.

The removed spellings are deleted rather than aliased. They now fail at
argument parse, before any file is written, and a project that passed one
rewrites a single line.

No behavior moves here: `automation` is accepted as a group and nothing
reads it yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Queue automation becomes the target env's

**Files:**
- Modify: `src/cli/migrate/mod.rs` — generalize `reconcile_training_enabled` (lines 1814–1851) into `reconcile_target_owned_keys`; call it twice from `transform_file` (the block at 1065–1074); update the three existing training unit tests and add five
- Modify: `tests/cli_migrate.rs` — a fixture helper and three integration tests

**Interfaces:**
- Consumes from Task 1: `Carry` with its `automation` field, in scope inside `transform_file`
- Produces: `fn reconcile_target_owned_keys(value: &mut serde_json::Value, kind: &str, tgt_path: &Path, keys: &[&str])` and `const AUTOMATION_KEYS: &[&str]`, both module-private

- [ ] **Step 1: Write the failing unit tests**

In `src/cli/migrate/mod.rs`'s test module, replace the three `reconcile_training_*` tests with these eight (the first three are the existing ones retargeted at the new function name — keep their comments):

```rust
    #[test]
    fn reconcile_training_matched_adopts_target_value() {
        // Engine auto-training is a per-env policy; a matched target keeps its
        // own `training_enabled` (source `true` must not overwrite target
        // `false`, or migrate+sync perpetually conflicts).
        let mut source = serde_json::json!({ "name": "Q", "training_enabled": true });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "Q", "training_enabled": false }));
        reconcile_target_owned_keys(&mut source, "queues", &tgt, &["training_enabled"]);
        assert_eq!(source["training_enabled"], serde_json::json!(false));
    }

    #[test]
    fn reconcile_training_new_target_drops_flag() {
        // No target => brand-new queue => drop the flag; Rossum's create default
        // (false) applies and the round-trip is stable.
        let mut source = serde_json::json!({ "name": "Q", "training_enabled": true });
        let missing = std::path::Path::new("/nonexistent/does-not-exist/queue.json");
        reconcile_target_owned_keys(&mut source, "queues", missing, &["training_enabled"]);
        assert!(source.get("training_enabled").is_none());
    }

    #[test]
    fn reconcile_training_no_op_for_non_queue() {
        // Only queues carry `training_enabled`; other kinds are untouched.
        let mut source = serde_json::json!({ "name": "S", "training_enabled": true });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "S" }));
        reconcile_target_owned_keys(&mut source, "schemas", &tgt, &["training_enabled"]);
        assert_eq!(source["training_enabled"], serde_json::json!(true));
    }

    #[test]
    fn reconcile_automation_matched_adopts_every_target_value() {
        // A prod org that has earned automation must not be reset by a dev env
        // that has not — nor the reverse.
        let mut source = serde_json::json!({
            "name": "Q",
            "automation_enabled": true,
            "automation_level": "always",
            "quality_spot_check_percentage": 0.02,
        });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "name": "Q",
            "automation_enabled": false,
            "automation_level": "never",
            "quality_spot_check_percentage": 0.0,
        }));
        reconcile_target_owned_keys(&mut source, "queues", &tgt, AUTOMATION_KEYS);
        assert_eq!(source["automation_enabled"], serde_json::json!(false));
        assert_eq!(source["automation_level"], serde_json::json!("never"));
        assert_eq!(source["quality_spot_check_percentage"], serde_json::json!(0.0));
    }

    #[test]
    fn reconcile_automation_new_target_drops_every_key() {
        // A brand-new queue POSTs without them, so the server's own defaults
        // apply — `automation_enabled: false` and `automation_level: "never"`,
        // i.e. automation OFF. Verified against the published OpenAPI spec:
        // `POST /v1/queues` requires only `name` and `schema`.
        let mut source = serde_json::json!({
            "name": "Q",
            "automation_enabled": true,
            "automation_level": "confident",
            "quality_spot_check_percentage": 0.02,
        });
        let missing = std::path::Path::new("/nonexistent/does-not-exist/queue.json");
        reconcile_target_owned_keys(&mut source, "queues", missing, AUTOMATION_KEYS);
        assert!(source.get("automation_enabled").is_none());
        assert!(source.get("automation_level").is_none());
        assert!(source.get("quality_spot_check_percentage").is_none());
        assert_eq!(source["name"], serde_json::json!("Q"), "unrelated keys survive");
    }

    #[test]
    fn reconcile_automation_drops_only_the_keys_the_target_lacks() {
        // Per-key, not all-or-nothing: a target pulled before a field existed
        // keeps the reconcile honest for the fields it does carry.
        let mut source = serde_json::json!({
            "name": "Q",
            "automation_enabled": true,
            "automation_level": "always",
            "quality_spot_check_percentage": 0.02,
        });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "name": "Q",
            "automation_level": "confident",
        }));
        reconcile_target_owned_keys(&mut source, "queues", &tgt, AUTOMATION_KEYS);
        assert_eq!(source["automation_level"], serde_json::json!("confident"));
        assert!(source.get("automation_enabled").is_none());
        assert!(source.get("quality_spot_check_percentage").is_none());
    }

    #[test]
    fn reconcile_automation_no_op_for_non_queue() {
        // Only queues carry these; a schema of the same shape is untouched.
        let mut source = serde_json::json!({ "name": "S", "automation_level": "always" });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "S", "automation_level": "never" }));
        reconcile_target_owned_keys(&mut source, "schemas", &tgt, AUTOMATION_KEYS);
        assert_eq!(source["automation_level"], serde_json::json!("always"));
    }

    #[test]
    fn reconcile_leaves_a_key_the_source_does_not_carry_absent() {
        // The reconcile adopts, it does not introduce: a source body with no
        // automation keys must not grow them from the target, or migrate would
        // write fields into objects that never had them.
        let mut source = serde_json::json!({ "name": "Q" });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "name": "Q",
            "automation_level": "confident",
        }));
        reconcile_target_owned_keys(&mut source, "queues", &tgt, AUTOMATION_KEYS);
        assert!(source.get("automation_level").is_none());
    }
```

- [ ] **Step 2: Write the failing integration tests**

In `tests/cli_migrate.rs`, beside `setup_threshold_project`:

```rust
/// Helper: a src+tgt queue tree with an identity mapping where both queues
/// carry the three `automation` group fields. `(enabled, level, spot_check)`
/// per env. Returns the project root.
fn setup_automation_project(
    src: (bool, &str, f64),
    tgt: (bool, &str, f64),
) -> TempDir {
    let project = init_two_env_project();
    let root = project.path().to_path_buf();
    let queue = |a: (bool, &str, f64)| {
        serde_json::json!({
            "name": "Invoices",
            "workspace": "rdc://workspaces/main",
            "schema": "rdc://schemas/invoices",
            "automation_enabled": a.0,
            "automation_level": a.1,
            "quality_spot_check_percentage": a.2,
        })
    };
    let schema = serde_json::json!({
        "name": "Invoices schema",
        "content": [{
            "category": "section", "id": "header",
            "children": [{ "category": "datapoint", "id": "amount", "type": "number" }]
        }]
    });
    for (env, a) in [("test", src), ("prod", tgt)] {
        let base = root.join(format!("envs/{env}/workspaces/main/queues/invoices"));
        write(&base.join("schema.json"), &schema);
        write(&base.join("queue.json"), &queue(a));
        write(
            &root.join(format!("envs/{env}/workspaces/main/workspace.json")),
            &serde_json::json!({ "name": "Main" }),
        );
    }
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[workspaces]\n\"main\" = \"main\"\n\n[queues]\n\"invoices\" = \"invoices\"\n\n[schemas]\n\"invoices\" = \"invoices\"\n",
    )
    .unwrap();
    project
}

/// By default a matched target keeps its OWN automation configuration —
/// whether a queue auto-exports without review is the target organization's
/// operational decision, not something a schema promotion carries with it.
#[test]
fn migrate_ignores_queue_automation_by_default() {
    let project = setup_automation_project((true, "always", 0.02), (false, "never", 0.0));
    let root = project.path();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], Carry::NONE);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let queue = read_json(
        &root.join("envs/prod/workspaces/main/queues/invoices/queue.json"),
    );
    assert_eq!(queue["automation_enabled"], serde_json::json!(false));
    assert_eq!(queue["automation_level"], serde_json::json!("never"));
    assert_eq!(queue["quality_spot_check_percentage"], serde_json::json!(0.0));
}

/// `--carry automation` promotes the source's values verbatim — the behavior
/// that predates this reconcile, for an env pair that is meant to be a mirror.
#[test]
fn migrate_carries_queue_automation_with_the_carry_group() {
    let project = setup_automation_project((true, "always", 0.02), (false, "never", 0.0));
    let root = project.path();

    let carry = Carry { automation: true, ..Carry::NONE };
    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], carry);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let queue = read_json(
        &root.join("envs/prod/workspaces/main/queues/invoices/queue.json"),
    );
    assert_eq!(queue["automation_enabled"], serde_json::json!(true));
    assert_eq!(queue["automation_level"], serde_json::json!("always"));
    assert_eq!(queue["quality_spot_check_percentage"], serde_json::json!(0.02));
}

/// A queue the target has never had: the three keys are dropped so the POST
/// omits them and Rossum's own defaults apply (automation off). Carrying the
/// source's `always` here would auto-export documents in a fresh organization
/// from its first day.
#[test]
fn migrate_drops_queue_automation_for_a_brand_new_target_queue() {
    let project = setup_automation_project((true, "always", 0.02), (false, "never", 0.0));
    let root = project.path();
    std::fs::remove_file(root.join("envs/prod/workspaces/main/queues/invoices/queue.json"))
        .expect("the target queue file must exist to be removed");

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], Carry::NONE);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let queue = read_json(
        &root.join("envs/prod/workspaces/main/queues/invoices/queue.json"),
    );
    assert!(queue.get("automation_enabled").is_none(), "{queue}");
    assert!(queue.get("automation_level").is_none(), "{queue}");
    assert!(queue.get("quality_spot_check_percentage").is_none(), "{queue}");
}

/// `overlay.toml` is the user declaring the target's value on purpose, so it
/// must win over the reconcile — the documented precedence, and the only way to
/// choose a new env's automation deliberately. Without this the overlay entry
/// is silently inert forever: the reconcile writes the target's pulled value
/// back on every run.
#[test]
fn migrate_overlay_wins_over_the_automation_reconcile() {
    let project = setup_automation_project((true, "always", 0.02), (false, "never", 0.0));
    let root = project.path();
    std::fs::write(
        root.join("envs/prod/overlay.toml"),
        "version = 1\n\n[queues.invoices]\nautomation_level = \"confident\"\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], Carry::NONE);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let queue = read_json(
        &root.join("envs/prod/workspaces/main/queues/invoices/queue.json"),
    );
    assert_eq!(
        queue["automation_level"],
        serde_json::json!("confident"),
        "an explicit overlay automation_level must win over the reconcile"
    );
    assert_eq!(
        queue["automation_enabled"],
        serde_json::json!(false),
        "a key the overlay does not name still comes from the target"
    );
}

/// The second run reads the first run's output as its target, so the reconcile
/// must be a fixed point. A migrate that is not byte-stable shows up as endless
/// churn in `git diff` and re-pushes the whole env on every cycle.
#[test]
fn migrate_automation_reconcile_is_idempotent() {
    let project = setup_automation_project((true, "always", 0.02), (false, "never", 0.0));
    let root = project.path();
    let out = root.join("envs/prod/workspaces/main/queues/invoices/queue.json");

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    rdc::cli::migrate::run("test", "prod", false, false, vec![], Carry::NONE)
        .expect("first migrate");
    let first = std::fs::read(&out).expect("first output");
    rdc::cli::migrate::run("test", "prod", false, false, vec![], Carry::NONE)
        .expect("second migrate");
    let second = std::fs::read(&out).expect("second output");
    std::env::set_current_dir(&prev).unwrap();

    assert_eq!(
        String::from_utf8_lossy(&first),
        String::from_utf8_lossy(&second),
        "a second migrate must produce identical bytes"
    );
}
```

- [ ] **Step 3: Run both suites and watch them fail**

Run: `cargo test --lib reconcile_ && cargo test --test cli_migrate automation`
Expected: FAIL — `reconcile_target_owned_keys` and `AUTOMATION_KEYS` do not exist (compile error), and the integration tests would find the source's automation promoted.

- [ ] **Step 4: Generalize the reconcile**

In `src/cli/migrate/mod.rs`, replace `reconcile_training_enabled` (its doc comment at 1814 through the function's close at 1851) with:

```rust
/// The queue fields that make up the `automation` carry group.
///
/// All three are top-level on a queue — verified against 240 pulled
/// `queue.json` files, where every one carries them at the top level — so this
/// needs no position-agnostic search of the kind `default_score_threshold` has.
const AUTOMATION_KEYS: &[&str] = &[
    "automation_enabled",
    "automation_level",
    "quality_spot_check_percentage",
];

/// Reconcile top-level queue fields that the TARGET env owns, so migrate does
/// not promote them from the source.
///
/// Two callers, one rule:
///
/// * `training_enabled` — unconditional. Engine auto-training is a per-env
///   policy (you train the model in dev, not in a throwaway test clone), and
///   Rossum resets the flag to `false` when a queue is created. Carrying the
///   source's value makes every migrate+sync conflict — the migrated queue says
///   `true`, the deployed queue is `false`, and neither side ever converges.
/// * [`AUTOMATION_KEYS`] — unless `--carry automation`. Whether a queue
///   auto-exports without human review is the target organization's operational
///   decision, taken after watching its own accuracy; it is not solution
///   configuration that travels with a schema change.
///
/// The rule, following [`reconcile_score_thresholds`]:
///
/// - **Matched target** (`tgt_path` exists and carries the key): adopt the
///   TARGET's value, so each env keeps its own policy.
/// - **New target, or the target lacks that key**: drop it, so the server's
///   create-time default applies and the round-trip is stable. Per key, not
///   all-or-nothing.
///
/// A key the source body does not have is never introduced from the target:
/// this adopts, it does not populate.
///
/// A no-op for any kind other than `queues`.
fn reconcile_target_owned_keys(
    value: &mut serde_json::Value,
    kind: &str,
    tgt_path: &Path,
    keys: &[&str],
) {
    if kind != "queues" {
        return;
    }
    // Check before reading: a queue body carrying none of the keys needs no
    // target file at all, and `transform_file` runs this per file.
    let Some(obj) = value.as_object() else {
        return;
    };
    if !keys.iter().any(|k| obj.contains_key(*k)) {
        return;
    }
    // Absent/unparseable target => brand-new queue => every key drops.
    let target: Option<serde_json::Value> = std::fs::read(tgt_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for key in keys {
        if !obj.contains_key(*key) {
            continue;
        }
        match target.as_ref().and_then(|t| t.get(*key)).cloned() {
            Some(v) => {
                obj.insert((*key).to_string(), v);
            }
            None => {
                obj.shift_remove(*key);
            }
        }
    }
}
```

- [ ] **Step 5: Call it twice**

In `transform_file`, replace the `training_enabled` block (comment + call, lines 1065–1074) with:

```rust
    // The per-queue `training_enabled` flag — unconditional, no carry group.
    if let Some((kind, _)) = classify(rel) {
        reconcile_target_owned_keys(&mut value, kind, &dst_path, &["training_enabled"]);
    }

    // The queue's automation configuration, unless the user opted to carry it.
    // Unlike `training_enabled` these are NOT in `snapshot::noise::NOISE_FIELDS`
    // — rdc hashes and pushes them — so promoting the source's values is a real
    // change that the following `rdc sync` really applies to the target org.
    // That is the whole bug: a promotion silently switched a target queue's
    // automation to whatever the source env happened to have.
    if !carry.automation
        && let Some((kind, _)) = classify(rel)
    {
        reconcile_target_owned_keys(&mut value, kind, &dst_path, AUTOMATION_KEYS);
    }
```

- [ ] **Step 6: Build once and run both suites**

Run: `cargo test --lib reconcile_ && cargo test --test cli_migrate`
Expected: PASS, including every pre-existing `cli_migrate` test — no fixture in that file carries automation keys, so nothing else may move.

- [ ] **Step 7: Lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src/cli/migrate/mod.rs tests/cli_migrate.rs
git commit -m "$(cat <<'EOF'
feat!: leave a queue's automation configuration to the target env

`automation_enabled`, `automation_level` and
`quality_spot_check_percentage` were ordinary content, so promoting one
env over another overwrote the target organization's automation policy
with the source's. Whether a queue auto-exports without human review is
an operational decision the target org takes after watching its own
accuracy — not solution configuration that travels with a schema change.

They now follow `training_enabled`: a matched target keeps its own
values, and a brand-new queue drops the keys so the server's defaults
apply. Per the published OpenAPI spec, `POST /v1/queues` requires only
`name` and `schema`, and the two documented fields default to automation
OFF — so dropping them on create is both safe and the conservative
outcome.

`reconcile_training_enabled` is generalized into
`reconcile_target_owned_keys`, which both callers share. `--carry
automation` restores the previous behavior for an env pair meant to be a
mirror.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: README

**Files:**
- Modify: `README.md` — replace the `### Score thresholds` and `### Inbox email prefixes` sections (lines ~423–451, ending with the paragraph beginning "An overlay value always wins over these reconciles")

- [ ] **Step 1: Read the exact block you are replacing**

Run: `sed -n '420,455p' README.md`
Confirm it starts at `### Score thresholds` and ends with the `An overlay value always wins over these reconciles …` paragraph. Line numbers may have drifted; the headings are the anchors.

- [ ] **Step 2: Replace it**

```markdown
### Fields the target env owns

Some fields are tuned per organization rather than promoted with the solution, so migrate leaves them to the **target** env: a matched target keeps its own value, and a brand-new object drops the field so the server's default applies. Name a group with `--carry` to promote the source env's values instead:

| group | fields |
| --- | --- |
| `score-thresholds` | a schema datapoint's `score_threshold`, a queue's `default_score_threshold` |
| `email-prefixes` | an inbox's `email_prefix` |
| `automation` | a queue's `automation_enabled`, `automation_level`, `quality_spot_check_percentage` |
| `all` | every group above |

```sh
rdc migrate test prod                                    # every group stays the target's
rdc migrate test prod --carry automation
rdc migrate test prod --carry score-thresholds,automation
rdc migrate test prod --carry all
```

`--carry` is repeatable as well as comma-separated, and an unknown group is rejected with the valid set named.

A queue's `training_enabled` is always the target's, with no group and no opt-in: Rossum resets it to `false` when a queue is created, so carrying the source's value would make every migrate+sync conflict.

#### Score thresholds

Confidence thresholds are tuned per queue/organization and expected to differ across envs. A datapoint's `score_threshold` and a queue's `default_score_threshold` are therefore taken from the *target* when the object already exists there, and dropped (falling back to the queue/server default) for brand-new objects.

#### Inbox email prefixes

An inbox's `email_prefix` is the left-hand side of its **public address** — Rossum derives `email` as `<email_prefix>-<hash>@<host>` — so promoting the source env's value re-addresses the target's mailbox and mail sent to the old address stops arriving. A target that already has a prefix keeps its own.

A **brand-new** inbox is the exception, because the field is mandatory on create: `POST /inboxes` rejects a body with neither `email_prefix` nor `email`, and rdc strips the server-derived `email`. Such an inbox keeps the source's prefix — nobody is sending to a mailbox that does not exist yet, so there is no address to strand — and migrate `warn`s for each one, naming the overlay key that overrides it. The warning repeats on every migrate until the inbox is deployed or you change the value.

To set a target env's prefix deliberately, declare it in that env's `overlay.toml`:

```toml
[inboxes.cost-invoices]
email_prefix = "acme-prod"
```

#### Queue automation

`automation_enabled` and `automation_level` decide whether a queue auto-exports documents without human review, and `quality_spot_check_percentage` sets how many automated documents are sampled back for QA. Those are operational decisions a team takes in one organization after watching that organization's accuracy — so a matched target keeps its own three values, and a brand-new queue drops them: `POST /queues` requires only `name` and `schema`, and Rossum's own defaults are `automation_enabled: false` / `automation_level: "never"`. A fresh env therefore starts with automation off and is switched on deliberately, rather than inheriting whatever the source env happened to have.

This stops *future* promotions from overwriting the target's configuration. It cannot undo a past one: where an earlier migrate + sync already pushed the source's values, the target organization genuinely holds them now, and migrate reads the target snapshot — so it faithfully keeps what is there. Set the value you want in the target env (or its `overlay.toml`) once, and it survives from then on.

An overlay value always wins over these reconciles — that is the documented precedence (per-object override > kind-wide `"*"` default > reconciled value), and it applies to `score_threshold` / `default_score_threshold` / `training_enabled` / `automation_level` alike.
```

- [ ] **Step 3: Verify no stale flag references survive anywhere**

Run: `grep -rn "migrate-score-thresholds\|migrate-email-prefixes" README.md src/ tests/ templates/`
Expected: no matches. (Hits under `docs/superpowers/specs/` and `docs/superpowers/plans/` are historical records and must stay.)

- [ ] **Step 4: Run the documentation guard**

Run: `cargo test --test command_references`
Expected: PASS — it checks that docs name only commands the CLI actually has.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "$(cat <<'EOF'
docs: document --carry and the fields the target env owns

The two flag-specific sections become one concept with a group table,
plus a per-group section apiece. The automation section states the limit
plainly: this stops future promotions from overwriting the target's
configuration and cannot undo a past one, because migrate reads the
target snapshot and the target org really does hold the clobbered value.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Live verification

Requires credentials. If `RDC_LIVE_*` is unset, **stop after Step 1 and report** — the feature is correct and fully covered offline without this task; what it adds is proof against a real server and the answer to one documentation question.

**Files:**
- Modify: `docs/superpowers/specs/2026-09-11-migrate-carry-groups-design.md` — record the OPTIONS result
- Modify: `README.md` — only if the probe contradicts the wording written in Task 3
- Modify: `tests/live/scenarios/migrate_promotion.rs` — extend the existing scenario

- [ ] **Step 1: Probe `quality_spot_check_percentage`**

The public OpenAPI spec lists it in the queue **response** contract but not in `queue_base.properties`, so its writability is undocumented. With a token for the sandbox organization:

```bash
curl -sS -X OPTIONS -H "Authorization: Bearer $RDC_LIVE_TOKEN" \
  "$RDC_LIVE_API_BASE/queues" | python3 -c "
import json,sys
d=json.load(sys.stdin)
def walk(o,p=''):
    if isinstance(o,dict):
        for k,v in o.items():
            if 'spot_check' in k: print(p+'/'+k, json.dumps(v)[:400])
            walk(v,p+'/'+k)
    elif isinstance(o,list):
        for i,v in enumerate(o): walk(v,f'{p}[{i}]')
walk(d)
"
```

Record the result — writable or read-only, type, bounds — in the spec's "Verified facts" table with today's date. Both answers keep the field in the group (the spec explains why); only the README's phrasing could need a word.

- [ ] **Step 2: Extend the live promotion scenario**

`tests/live/scenarios/migrate_promotion.rs` already promotes a queue between two real organizations. Add the probe helper next to `locale`:

```rust
/// The queue's `automation_level` as it stands on disk.
///
/// A valid probe where `training_enabled` is not: `automation_level` is absent
/// from `snapshot::noise::NOISE_FIELDS`, so rdc hashes it, pushes it, and a
/// difference is a real diff — which is precisely why migrate carrying it
/// across organizations was worth fixing.
fn automation_level(project: &ProjectFixture, env: &str, q_slug: &str) -> Option<String> {
    let path = queue_file_path(project.path(), env, q_slug, "queue.json")
        .unwrap_or_else(|| panic!("queue.json not found for {env}/{q_slug}"));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v.get("automation_level").and_then(|l| l.as_str()).map(str::to_string)
}
```

and this block at the very END of `live_migrate_overlay_and_mirror`, immediately before `drop(teardown_tgt);` — after the deletion-promotion assertions, so nothing it leaves on disk can perturb them:

```rust
    // -------------------------------------------------------------------------
    // Queue automation belongs to the target organization.
    // -------------------------------------------------------------------------
    // Turn automation on in the TARGET org only — the shape of a real prod env
    // that has earned automation its source env has not. Only a real pull can
    // produce the target snapshot the reconcile reads, which is why this is not
    // provable offline.
    tgt_client
        .patch_fields(
            "queue",
            prod_qid,
            serde_json::json!({ "automation_enabled": true, "automation_level": "confident" }),
        )
        .await
        .expect("enable automation on the target queue");
    let pull_prod = project.run_rdc(&["sync", "prod", "--no-push"]);
    assert!(pull_prod.status.success(), "pulling prod failed: {}", combined(&pull_prod));
    assert_eq!(
        automation_level(&project, "prod", &q_slug).as_deref(),
        Some("confident"),
        "the pull must bring the target org's automation level onto disk"
    );

    let m5 = project.run_rdc(&["migrate", "test", "prod", "--only", &only]);
    assert!(m5.status.success(), "automation migrate failed: {}", combined(&m5));
    assert_eq!(
        automation_level(&project, "prod", &q_slug).as_deref(),
        Some("confident"),
        "migrate promoted the source org's automation level over the target's"
    );

    let m6 = project.run_rdc(&[
        "migrate", "test", "prod", "--only", &only, "--carry", "automation",
    ]);
    assert!(m6.status.success(), "--carry automation failed: {}", combined(&m6));
    assert_eq!(
        automation_level(&project, "prod", &q_slug).as_deref(),
        automation_level(&project, "test", &q_slug).as_deref(),
        "--carry automation must promote the source org's value"
    );
```

This extends an existing scenario rather than adding one, so `tests/live/scenario_wrappers.rs`'s `fake_*`/`live_*` pairing contract and its `EXPECTED_IGNORED_LIVE_TESTS` count are both unchanged — do not touch either.

- [ ] **Step 3: Run it**

Run: `cargo test --test live -- --ignored live_migrate_overlay_and_mirror --nocapture`
Expected: PASS. It SKIPS with a printed reason if `RDC_LIVE_TGT_*` (the second organization) is unset — a skip is not a pass; say so in the report.

A burst of live failures is usually an expired token (they last about 30 minutes): check with a plain `curl` before debugging the code.

- [ ] **Step 4: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add tests/live/scenarios/migrate_promotion.rs docs/superpowers/specs/2026-09-11-migrate-carry-groups-design.md
git commit -m "$(cat <<'EOF'
test(live): prove queue automation stays the target org's

The reconcile reads the TARGET snapshot, which only a real pull against a
second organization can produce — so the one thing the offline tests
cannot show is that a promotion leaves a genuinely automated prod queue
alone. This turns automation on in the target org, pulls, promotes, and
asserts the level survived; then asserts `--carry automation` promotes it.

`automation_level` is the probe field because it is not in NOISE_FIELDS:
rdc hashes and pushes it, so a difference is a real diff.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

- [ ] `cargo test` — the whole offline suite
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `git log --oneline -4` — three or four commits on local `main`, nothing pushed
- [ ] `rdc migrate --help` on a debug build shows `--carry` with its four values and no `--migrate-*` flag
