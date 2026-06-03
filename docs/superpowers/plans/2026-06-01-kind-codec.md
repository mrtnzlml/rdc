# Per-kind `KindCodec` refactor — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace rdc's scattered per-kind serialization/redaction/hash/strip registries with one `KindCodec` trait per Rossum kind, so on-disk bytes and the lockfile hash can never diverge and every call site routes through one codec.

**Architecture:** A `KindCodec` trait (one module per kind under `src/snapshot/codec/`) exposes `disk_bytes`, `base_hash` (derived from `disk_bytes`), `create_body`, `cross_env_body`, `overlay`, `path`. A `codec(kind)` registry is the single dispatch point. Codecs land first as additive, independently-tested code (Phases 1–2), then a single big-bang cutover (Phase 3) switches every `pull`/`sync`/`push`/`deploy` site, after which the old registries and dead writers are deleted (Phase 4) and the whole tree is verified (Phase 5).

**Tech Stack:** Rust 2024 edition, `serde_json`, `sha2`; tests via `assert_cmd` + `wiremock` (integration) and `#[test]`/`proptest` (unit). Lints: `clippy -D warnings`, workspace `dead_code = "deny"`.

**Reference spec:** `docs/superpowers/specs/2026-06-01-kind-codec-design.md` (read §3 trait, §4 per-kind table, §7 migration before starting).

**Global conventions for every task:**
- TDD: write the failing test, run it red, implement, run it green, commit.
- Run unit tests: `cargo test -p rdc --locked <filter>`. Run lints before each commit: `cargo clippy -p rdc --all-targets --locked -- -D warnings`.
- `cargo fmt` is required but `cargo-fmt` may be missing locally; if `cargo fmt --check` errors with "not installed", run `rustup component add rustfmt` first.
- Commit messages use Conventional Commits (`feat`/`fix`/`refactor`/`test`).
- Do NOT commit unless the executing engineer is told to; this repo's owner publishes manually.

---

## Phase 1 — Foundation: trait, artifact, registry, invariant harness

### Task 1: Define `DiskArtifact` and the `KindCodec` trait

**Files:**
- Create: `src/snapshot/codec/mod.rs`
- Modify: `src/snapshot/mod.rs` (add `pub mod codec;`)

- [ ] **Step 1: Create the module with the trait (no kinds yet).**

```rust
//! One codec per Rossum object kind. Each codec owns the kind's full
//! on-disk + hash behavior so disk bytes and the lockfile hash derive
//! from a single pipeline and cannot diverge. See
//! docs/superpowers/specs/2026-06-01-kind-codec-design.md.

use crate::overlay::Overlay;
use crate::paths::Paths;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Canonical on-disk artifact: the JSON file bytes plus any code/formula
/// sidecars extracted out of the JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskArtifact {
    /// Canonical `<file>.json` bytes (pretty + trailing '\n'), post
    /// redaction / hidden-field strip / key-order / sidecar extraction.
    pub json: Vec<u8>,
    /// (relative path, bytes) for each sidecar, e.g. ("<slug>.py", ...)
    /// or ("formulas/<id>.py", ...). Empty for kinds without sidecars.
    pub sidecars: Vec<(String, Vec<u8>)>,
}

pub trait KindCodec: Sync {
    /// Stable kind string ("engines", "hooks", ...). Lockfile key + dispatch.
    fn kind(&self) -> &'static str;

    /// Canonical on-disk artifact for a remote object's JSON Value.
    fn disk_bytes(&self, value: &Value) -> Result<DiskArtifact>;

    /// Lockfile base hash, DERIVED from `disk_bytes`:
    /// SHA256( canonicalize_for_hash(artifact.json) ⊕ each sidecar ).
    /// Defaulted; codecs never override the algorithm.
    fn base_hash(&self, value: &Value) -> Result<String> {
        let art = self.disk_bytes(value)?;
        Ok(crate::snapshot::codec::combined_hash(&art.json, &art.sidecars))
    }

    /// Mutate `value` into the within-env POST/PATCH body (strip server fields).
    fn create_body(&self, value: &mut Value);

    /// Mutate `value` into the cross-env (deploy) body / compare form.
    fn cross_env_body(&self, value: &mut Value);

    /// Overlay paths for `slug`, if this kind supports overlays.
    fn overlay<'a>(&self, _overlay: &'a Overlay, _slug: &str)
        -> Option<&'a BTreeMap<String, Value>> { None }

    /// On-disk location for `slug`.
    fn path(&self, paths: &Paths, slug: &str) -> PathBuf;
}

/// SHA-256 over canonical json bytes plus sidecars, using the existing
/// framing (`\0 <path> \0 <bytes>` per sidecar) so recorded hashes stay
/// algorithmically identical to the legacy combined-hash builders.
pub fn combined_hash(json: &[u8], sidecars: &[(String, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    for (path, bytes) in sidecars {
        hasher.update([0u8]);
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
    }
    crate::state::to_hex_public(&hasher.finalize())
}
```

> Note: `to_hex` in `state/lockfile.rs` is currently private. Either expose a `pub(crate) fn to_hex_public` wrapper or move `combined_hash` next to it. Confirm during implementation; adjust the path in the code above accordingly.

- [ ] **Step 2: Wire the module.** Add `pub mod codec;` to `src/snapshot/mod.rs`.

- [ ] **Step 3: Verify it compiles.**

Run: `cargo build -p rdc --locked`
Expected: compiles (the trait is unused; no `dead_code` error because it's `pub`).

- [ ] **Step 4: Commit.**

```bash
git add src/snapshot/codec/mod.rs src/snapshot/mod.rs
git commit -m "feat(codec): add KindCodec trait + DiskArtifact scaffold"
```

### Task 2: Registry skeleton

**Files:**
- Modify: `src/snapshot/codec/mod.rs`

- [ ] **Step 1: Add the registry function returning `None` for now.**

```rust
/// The single dispatch point. Returns the codec for a kind string.
/// Exhaustive once all kinds are registered (Phase 2).
pub fn codec(kind: &str) -> Option<&'static dyn KindCodec> {
    match kind {
        // filled in by Phase 2 tasks
        _ => None,
    }
}
```

- [ ] **Step 2: Build.** Run `cargo build -p rdc --locked`. Expected: compiles.

- [ ] **Step 3: Commit.**

```bash
git add src/snapshot/codec/mod.rs
git commit -m "feat(codec): add codec(kind) registry skeleton"
```

### Task 3: Cross-kind invariant test harness (the keystone)

**Files:**
- Create: `tests/codec_invariant.rs`

This test is parameterized over a list of (kind, sample `Value`) cases. Each Phase-2 codec adds its case. The invariant: `base_hash(v) == base_hash(parse(disk_bytes(v).json))` and disk→parse→disk is stable.

- [ ] **Step 1: Write the harness with one initial case (will fail until Task 4 lands the engines codec).**

```rust
use rdc::snapshot::codec::{codec, KindCodec};
use serde_json::{json, Value};

/// (kind, sample remote Value carrying volatile + hidden fields)
fn cases() -> Vec<(&'static str, Value)> {
    vec![
        ("engines", json!({
            "id": 401, "url": "https://x/api/v1/engines/401",
            "name": "E", "type": "extractor",
            "agenda_id": "tnt_live_123", "modified_at": "2026-04-20T08:00:00Z"
        })),
        // each Phase-2 codec appends its case here
    ]
}

#[test]
fn disk_bytes_and_base_hash_are_consistent_for_every_kind() {
    for (kind, v) in cases() {
        let c = codec(kind).unwrap_or_else(|| panic!("no codec for {kind}"));
        let art = c.disk_bytes(&v).unwrap();
        // Re-derive the hash from the produced disk json: must equal base_hash.
        let reparsed: Value = serde_json::from_slice(&art.json).unwrap();
        assert_eq!(
            c.base_hash(&v).unwrap(),
            c.base_hash(&reparsed).unwrap(),
            "{kind}: base_hash(v) != base_hash(parse(disk_bytes(v)))"
        );
    }
}

#[test]
fn disk_bytes_round_trip_is_stable_for_every_kind() {
    for (kind, v) in cases() {
        let c = codec(kind).unwrap();
        let once = c.disk_bytes(&v).unwrap();
        let reparsed: Value = serde_json::from_slice(&once.json).unwrap();
        let twice = c.disk_bytes(&reparsed).unwrap();
        assert_eq!(once.json, twice.json, "{kind}: disk_bytes not idempotent");
    }
}

#[test]
fn redaction_replaces_volatile_with_sentinel_not_raw() {
    // engines agenda_id must be the sentinel, never the live value, on disk.
    let c = codec("engines").unwrap();
    let art = c.disk_bytes(&json!({
        "id": 1, "url": "u", "name": "E", "agenda_id": "tnt_live_123"
    })).unwrap();
    let s = String::from_utf8(art.json).unwrap();
    assert!(!s.contains("tnt_live_123"), "raw agenda_id on disk: {s}");
    assert!(s.contains("refreshed live in Rossum"), "sentinel missing: {s}");
}
```

- [ ] **Step 2: Run to verify it fails** (no engines codec yet).

Run: `cargo test -p rdc --locked --test codec_invariant`
Expected: FAIL — `no codec for engines`.

- [ ] **Step 3: Commit (red test is intentional; it goes green in Task 4).**

```bash
git add tests/codec_invariant.rs
git commit -m "test(codec): add cross-kind disk<->hash invariant harness (red)"
```

---

## Phase 2 — One codec per kind

Each task: write the codec, register it, add its case to `tests/codec_invariant.rs::cases()`, add a unit test for its redaction/sidecar specifics, run the invariant + unit tests green, commit. Per-kind data comes from spec §4.

The three archetypes are coded in full first (engines = flat+redact, labels = flat plain, hooks = sidecar+redact+key-order); the remaining kinds follow the matching archetype with their own data, each fully coded in its task.

### Task 4: `engines` codec (archetype: flat + redact)

**Files:**
- Create: `src/snapshot/codec/engines.rs`
- Modify: `src/snapshot/codec/mod.rs` (register), `tests/codec_invariant.rs` (case already present from Task 3)

- [ ] **Step 1: Write the codec.**

```rust
use super::{DiskArtifact, KindCodec};
use crate::overlay::Overlay;
use crate::paths::Paths;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Engines;

impl KindCodec for Engines {
    fn kind(&self) -> &'static str { "engines" }

    fn disk_bytes(&self, value: &Value) -> Result<DiskArtifact> {
        let mut v = value.clone();
        // redact volatile -> sentinel (agenda_id)
        crate::snapshot::create::redact_for_disk(&mut v, "engines");
        // strip hidden fields recursively (modified_at)
        crate::snapshot::key_order::strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v).context("serializing engine")?;
        json.push(b'\n');
        Ok(DiskArtifact { json, sidecars: vec![] })
    }

    fn create_body(&self, value: &mut Value) {
        crate::snapshot::create::strip_for_create(value, "engines");
    }

    fn cross_env_body(&self, value: &mut Value) {
        crate::snapshot::create::strip_for_cross_env_patch(value, "engines");
    }

    fn overlay<'a>(&self, o: &'a Overlay, slug: &str)
        -> Option<&'a BTreeMap<String, Value>> { o.engine(slug) }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.engine_dir(slug).join("engine.json")
    }
}
```

- [ ] **Step 2: Register it.** In `src/snapshot/codec/mod.rs`:

```rust
mod engines;
// inside codec(): add an arm
        "engines" => Some(&engines::Engines),
```

- [ ] **Step 3: Run invariant + redaction tests green.**

Run: `cargo test -p rdc --locked --test codec_invariant`
Expected: PASS (engines case now resolves; redaction asserts sentinel).

- [ ] **Step 4: Lint + commit.**

```bash
cargo clippy -p rdc --all-targets --locked -- -D warnings
git add src/snapshot/codec/engines.rs src/snapshot/codec/mod.rs
git commit -m "feat(codec): engines codec (redact agenda_id)"
```

### Task 5: `labels` codec (archetype: flat plain, no redaction/sidecar/overlay-on-some)

**Files:** Create `src/snapshot/codec/labels.rs`; modify `codec/mod.rs`, `tests/codec_invariant.rs`.

- [ ] **Step 1: Write the codec.**

```rust
use super::{DiskArtifact, KindCodec};
use crate::overlay::Overlay;
use crate::paths::Paths;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Labels;

impl KindCodec for Labels {
    fn kind(&self) -> &'static str { "labels" }
    fn disk_bytes(&self, value: &Value) -> Result<DiskArtifact> {
        let mut v = value.clone();
        crate::snapshot::key_order::strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v).context("serializing label")?;
        json.push(b'\n');
        Ok(DiskArtifact { json, sidecars: vec![] })
    }
    fn create_body(&self, value: &mut Value) {
        crate::snapshot::create::strip_for_create(value, "labels");
    }
    fn cross_env_body(&self, value: &mut Value) {
        crate::snapshot::create::strip_for_cross_env_patch(value, "labels");
    }
    fn overlay<'a>(&self, o: &'a Overlay, slug: &str)
        -> Option<&'a BTreeMap<String, Value>> { o.label(slug) }
    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.labels_dir().join(format!("{slug}.json"))
    }
}
```

- [ ] **Step 2: Register + add a `labels` case to `cases()`** (a label Value with `modified_at`).

```rust
        ("labels", json!({
            "id": 42, "url": "https://x/api/v1/labels/42", "name": "L",
            "color": "#aabbcc", "modified_at": "2026-04-20T08:00:00Z"
        })),
```

- [ ] **Step 3: Add a unit test** asserting `modified_at` is absent from disk bytes.

```rust
#[test]
fn labels_strip_modified_at_from_disk() {
    let c = codec("labels").unwrap();
    let art = c.disk_bytes(&json!({"id":1,"name":"L","modified_at":"2026-01-01T00:00:00Z"})).unwrap();
    let s = String::from_utf8(art.json).unwrap();
    assert!(!s.contains("modified_at"), "modified_at must not be on disk: {s}");
}
```

- [ ] **Step 4: Run tests green, lint, commit.**

```bash
cargo test -p rdc --locked --test codec_invariant
cargo clippy -p rdc --all-targets --locked -- -D warnings
git add src/snapshot/codec/labels.rs src/snapshot/codec/mod.rs tests/codec_invariant.rs
git commit -m "feat(codec): labels codec (flat, strip modified_at)"
```

### Task 6: `hooks` codec (archetype: sidecar code + redact status + key-order)

**Files:** Create `src/snapshot/codec/hooks.rs`; modify `codec/mod.rs`, `tests/codec_invariant.rs`.

The existing `snapshot::hook::serialize_hook` already does split-code + sort_queues + strip_hidden + reorder + redact_for_disk("hooks"). Reuse it; the codec adapts its `(json, code)` output into a `DiskArtifact`.

- [ ] **Step 1: Write the codec.**

```rust
use super::{DiskArtifact, KindCodec};
use crate::model::Hook;
use crate::overlay::Overlay;
use crate::paths::Paths;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Hooks;

impl KindCodec for Hooks {
    fn kind(&self) -> &'static str { "hooks" }
    fn disk_bytes(&self, value: &Value) -> Result<DiskArtifact> {
        let hook: Hook = serde_json::from_value(value.clone())
            .context("deserializing hook for codec")?;
        let (json, code) = crate::snapshot::hook::serialize_hook(&hook)?;
        let ext = crate::snapshot::hook::hook_code_extension(&hook); // "py" | "js"
        let sidecars = match code {
            Some(c) => vec![(format!("__CODE__.{ext}"), c.into_bytes())],
            None => vec![],
        };
        Ok(DiskArtifact { json, sidecars })
    }
    fn create_body(&self, value: &mut Value) {
        crate::snapshot::create::strip_for_create(value, "hooks");
    }
    fn cross_env_body(&self, value: &mut Value) {
        crate::snapshot::create::strip_for_cross_env_patch(value, "hooks");
    }
    fn overlay<'a>(&self, o: &'a Overlay, slug: &str)
        -> Option<&'a BTreeMap<String, Value>> { o.hook(slug) }
    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.hooks_dir().join(format!("{slug}.json"))
    }
}
```

> The sidecar path `__CODE__.<ext>` is a hash-stable placeholder; the actual on-disk filename (`<slug>.<ext>`) is computed by the call site that knows the slug. The hash framing only needs a stable label — confirm the legacy `hook_combined_hash` used `"code"` as the label and match it (use `"code"` instead of `__CODE__.<ext>` if exact hash parity with old lockfiles is required; note that hook hashes change once this migration runs anyway, so either is acceptable — document the choice).

- [ ] **Step 2: Register + add a `hooks` case** (a function hook with `config.code` + `status`).

```rust
        ("hooks", json!({
            "id": 501, "url": "https://x/api/v1/hooks/501", "name": "H",
            "type": "function", "queues": [], "events": ["annotation_content"],
            "config": {"runtime":"python3.12","code":"def x(p):\n    return {}\n"},
            "status": "ready", "modified_at": "2026-04-20T08:00:00Z"
        })),
```

- [ ] **Step 3: Add a unit test** asserting `status` is the sentinel on disk and `code` is extracted to a sidecar.

```rust
#[test]
fn hooks_redact_status_and_extract_code() {
    let c = codec("hooks").unwrap();
    let art = c.disk_bytes(&json!({
        "id":1,"url":"u","name":"H","type":"function","queues":[],"events":[],
        "config":{"runtime":"python3.12","code":"def x(p):\n    return {}\n"},
        "status":"ready"
    })).unwrap();
    let s = String::from_utf8(art.json.clone()).unwrap();
    assert!(!s.contains("\"ready\""), "raw status on disk: {s}");
    assert!(s.contains("refreshed live in Rossum"), "status sentinel missing");
    assert!(!s.contains("def x"), "code must be extracted to sidecar");
    assert_eq!(art.sidecars.len(), 1, "one code sidecar expected");
}
```

- [ ] **Step 4: Run green, lint, commit.**

```bash
cargo test -p rdc --locked --test codec_invariant
cargo clippy -p rdc --all-targets --locked -- -D warnings
git add src/snapshot/codec/hooks.rs src/snapshot/codec/mod.rs tests/codec_invariant.rs
git commit -m "feat(codec): hooks codec (sidecar code + redact status)"
```

### Tasks 7–16: remaining codecs (one task each)

For each kind below, create `src/snapshot/codec/<kind>.rs` implementing `KindCodec` following the matching archetype (engines = flat+redact; labels = flat plain; hooks/rules/schemas = sidecar). Use the exact per-kind data from spec §4. Register in `codec/mod.rs`, add a `cases()` entry, add the relevant unit assertion, run the invariant test green, lint, commit. The full per-kind data:

- [ ] **Task 7 — `queues`** (archetype: flat+redact). redact: `counts`. overlay: `o.queue(slug)`. path: `paths.queue_dir(ws_slug, slug).join("queue.json")` — **note** queues need the workspace slug; the codec `path` signature takes a single slug, so for nested kinds use a composite slug `"<ws>/<q>"` OR add a `nested_path(paths, parent_slug, slug)` helper. Decide in Task 7 and apply to all nested kinds (queues, schemas, inboxes, engine_fields, workflow_steps, email_templates). Recommended: codec `path` takes the already-composed relative path from the caller; see Task 17 note. create_body strip: `queues` (strips hooks/webhooks/rules/inbox/counts/users/workflows). unit test: `counts` → sentinel on disk. Commit `feat(codec): queues codec (redact counts)`.
- [ ] **Task 8 — `schemas`** (archetype: sidecar formulas). Reuse `snapshot::schema::serialize_schema` for json + formulas; sidecars = `formulas/<field_id>.py` (sorted by field_id). No redaction. overlay: `o.schema(slug)`. unit test: formulas extracted; `base_hash` folds them (matches legacy `schema_combined_hash` framing). Commit `feat(codec): schemas codec (formula sidecars)`.
- [ ] **Task 9 — `rules`** (archetype: sidecar). Reuse `snapshot::rule::serialize_rule` for json + `trigger_condition` sidecar (`<slug>.py`, label `"trigger_condition"` in hash framing). No redaction. overlay: `o.rule(slug)`. Commit `feat(codec): rules codec (trigger_condition sidecar)`.
- [ ] **Task 10 — `inboxes`** (archetype: flat plain). No redaction. create_body strip: `inboxes` (strips `email`). overlay: `o.inbox(slug)`. Commit `feat(codec): inboxes codec`.
- [ ] **Task 11 — `workspaces`** (archetype: flat plain). No redaction. No overlay (return None). create_body strip: `workspaces` (strips `queues`). unit test: `modified_at` (incl. nested) absent on disk — this is the fix for bug (b). Commit `feat(codec): workspaces codec (recursive modified_at strip)`.
- [ ] **Task 12 — `engine_fields`** (archetype: flat plain). No redaction. overlay: `o.engine_field(slug)`. cross_env_body strip: also `name` (immutable). Commit `feat(codec): engine_fields codec`.
- [ ] **Task 13 — `workflows`** (archetype: flat plain). No redaction, no overlay. Commit `feat(codec): workflows codec`.
- [ ] **Task 14 — `workflow_steps`** (archetype: flat plain). No redaction, no overlay. Commit `feat(codec): workflow_steps codec`.
- [ ] **Task 15 — `email_templates`** (archetype: flat plain). No redaction. overlay: `o.email_template(key)`. create_body strip: `email_templates` (strips `triggers`). Commit `feat(codec): email_templates codec`.
- [ ] **Task 16 — `organization` and `mdh`** (pull-only). `organization`: flat plain, no overlay, `create_body`/`cross_env_body` are no-ops. `mdh`/`index_set`: `disk_bytes` reuses the existing `strip_server_managed` (mdh.rs:29) logic moved into the codec; no overlay; no-op outbound bodies. Add both `cases()` entries. Commit `feat(codec): organization + mdh codecs`.

After Task 16, `codec(kind)` returns `Some(..)` for every kind; change its fallthrough to `_ => None` and add a test asserting every known kind resolves:

- [ ] **Task 16b: completeness test.**

```rust
#[test]
fn every_known_kind_has_a_codec() {
    for k in ["engines","engine_fields","queues","schemas","inboxes","hooks",
              "rules","workspaces","labels","workflows","workflow_steps",
              "email_templates","organization","mdh"] {
        assert!(rdc::snapshot::codec::codec(k).is_some(), "missing codec: {k}");
    }
}
```

Run green; commit `test(codec): assert every kind resolves`.

---

## Phase 3 — Big-bang cutover (switch every call site)

Each live bug gets its reproduction test FIRST (red), then the cutover makes it green. Reuse the existing held repro tests for engines/hooks (already in `tests/cli_sync.rs`).

### Task 17: Decide nested-path handling, add codec `disk_artifact_to_files` writer helper

**Files:** Modify `src/snapshot/codec/mod.rs`; reference `src/cli/pull/common.rs`.

- [ ] **Step 1:** Add a helper that, given a `DiskArtifact`, a target json path, and a sidecar-naming closure, writes json + sidecars atomically and returns the bytes used for the lockfile hash. This centralizes "write + base-cache" so call sites shrink. Provide full code mirroring `apply_pull_action` semantics (three-way) but factored to accept codec output. (Full code authored here during implementation; ~40 lines. Must reuse `crate::state::base_cache` + `crate::snapshot::writer::write_atomic`.)
- [ ] **Step 2:** Resolve nested paths: change `KindCodec::path` to accept a pre-composed relative slug path, OR add `fn json_path(&self, paths, slug)` per kind. Recommended: each codec's `path` takes the composite slug the caller already computes (e.g. `"<ws>/<q>"`), keeping the registry uniform. Update Tasks 7–16 codecs accordingly.
- [ ] **Step 3:** Build + commit `feat(codec): writer helper + nested path resolution`.

### Task 18: Migrate pull drivers

**Files:** Modify each `src/cli/pull/*.rs`.

For each pull driver, replace the inline byte production with the codec. The uniform transformation per kind:

```
// before (example, engines):
let mut proposed = serde_json::to_vec_pretty(e)?; proposed.push(b'\n');
let proposed = maybe_strip_overlay(proposed, ctx.overlay.and_then(|o| o.engine(&slug)))?;
// after:
let art = crate::snapshot::codec::codec("engines").unwrap().disk_bytes(&serde_json::to_value(e)?)?;
let proposed = maybe_strip_overlay(art.json, codec("engines").unwrap().overlay(ovl, &slug))?;
```

- [ ] **Step 1:** Engines, queues (incl. inbox/schema sub-writes in `pull/queues.rs`), hooks, rules, labels, workflows, workflow_steps, engine_fields, email_templates, organization, mdh, workspaces — one edit each; the existing held edits in `pull/engines.rs` are superseded by the codec call (revert the held inline edit, use the codec).
- [ ] **Step 2:** Run the existing engine/hook repro tests (`tests/cli_sync.rs::sync_redacts_*`) — expected PASS.
- [ ] **Step 3:** Full suite + clippy. Commit `refactor(pull): route all kinds through KindCodec`.

### Task 19: Reproduce + fix the workspaces sync divergence (bug b)

**Files:** Test `tests/cli_sync.rs`; modify `src/cli/sync/mod.rs`.

- [ ] **Step 1: Write a failing test** — a workspace whose remote `modified_at` differs from the lockfile, asserting `sync` reports it Clean (no RemoteEdit/conflict, no rewrite).

```rust
// sync_workspace_modified_at_change_is_clean(): mock a workspace list with a
// modified_at that differs from a recorded baseline; assert sync makes no
// writes and emits no conflict. (Full mock mirrors sync_remote_create_writes_local_engine.)
```

- [ ] **Step 2: Run red** (currently RemoteEdit). Run: `cargo test -p rdc --locked --test cli_sync sync_workspace_modified_at`.
- [ ] **Step 3: Fix** — in `sync/mod.rs` workspaces block (~:658), replace raw `to_vec_pretty(w)` with `codec("workspaces").unwrap().base_hash(&serde_json::to_value(w)?)`.
- [ ] **Step 4: Run green**, full suite, clippy. Commit `fix(sync): hash workspaces via codec (no phantom drift)`.

### Task 20: Migrate the sync adapter remote-hash blocks

**Files:** Modify `src/cli/sync/mod.rs`.

- [ ] **Step 1:** Replace every per-kind remote-hash recompute (labels :483, organization :537, workflows :571, workflow_steps :619, workspaces :658, engines :706, engine_fields :775, hooks :844, rules :906, queues :1004, schemas :1019, inboxes :1033, email_templates :1132) with `codec(kind).unwrap().base_hash(&value)`.
- [ ] **Step 2:** Full suite + clippy. Commit `refactor(sync): adapter remote-hash via KindCodec`.

### Task 21: Reproduce + fix the sync-executor overlay non-convergence (bug d) and schema restore (bug e)

**Files:** Test `tests/cli_sync.rs`; modify `src/cli/sync/execute.rs`.

- [ ] **Step 1: Write two failing tests** — (d) an overlay-configured object resolved via conflict converges to Clean on the next sync; (e) a schema with ≥1 formula, locally deleted + remotely edited, restores with formulas and stays Clean.
- [ ] **Step 2: Run red.**
- [ ] **Step 3: Fix** — in the conflict-refs builder (:270-491) and remote-delete restore builder (:1719-1994), replace every inline `to_vec_pretty` with `codec(kind).unwrap().disk_bytes(&v)` and apply `codec(kind).overlay(ovl, slug)` strip; for schemas use the codec (restores formulas + combined hash) instead of the Flat downgrade.
- [ ] **Step 4: Run green**, full suite, clippy. Commit `fix(sync): executor uses KindCodec (overlay + schema restore)`.

### Task 22: Migrate push drivers (incl. engines post-PATCH, bug c)

**Files:** Modify each `src/cli/push/*.rs`.

- [ ] **Step 1:** For each kind, replace all three byte-production sites (POST-write, drift-compare, post-PATCH write) with `codec(kind).disk_bytes` / `base_hash`. This subsumes the held `push/engines.rs` edits and fixes the missed post-PATCH site (`push/engines.rs:156`).
- [ ] **Step 2:** Full suite + clippy. Commit `refactor(push): all kinds + 3 byte-sites via KindCodec`.

### Task 23: Reproduce + fix the deploy engine phantom-drift (bug a); delete `pull_redacts_kind`

**Files:** Test `tests/cli_deploy.rs`; modify `src/cli/deploy/apply.rs`, `src/cli/deploy/common.rs`.

- [ ] **Step 1: Write a failing test** — an engine whose remote `agenda_id` is live; after the tgt baseline is recorded (redacted), `tgt_drift_status` reports in-sync. Replace the stale `tgt_drift_status_engine_with_unchanged_agenda_id_is_in_sync` (common.rs:523) with a version that builds the baseline from the codec.
- [ ] **Step 2: Run red.**
- [ ] **Step 3: Fix** — `tgt_drift_status` computes the drift hash via `codec(kind).base_hash`; delete `pull_redacts_kind` and its conditional. Write-back uses `codec(kind).disk_bytes`. Cross-env compare/body uses `codec(kind).cross_env_body`.
- [ ] **Step 4: Run green**, full suite, clippy. Commit `fix(deploy): drift + write-back via KindCodec; drop pull_redacts_kind`.

---

## Phase 4 — Cleanup & migration self-heal

### Task 24: Delete dead writers and now-unused registries

**Files:** Modify `src/snapshot/queue.rs`, `src/snapshot/engine.rs`, `src/snapshot/create.rs`, `src/state/lockfile.rs`, `src/snapshot/hook.rs`.

- [ ] **Step 1:** Delete `write_queue` (queue.rs) and `write_engine` (engine.rs) if no longer referenced (`dead_code = "deny"` will flag them).
- [ ] **Step 2:** Remove the now-redundant hook `["status"]` extra-strip in `hook_combined_hash` only if `hook_combined_hash` itself is no longer used (the codec's `base_hash` replaces it). If still referenced, leave and add a deprecation note.
- [ ] **Step 3:** Build (the `dead_code` lint surfaces anything orphaned). Fix by deletion. Commit `refactor(snapshot): remove dead writers + redundant hash strips`.

### Task 25: Migration self-heal for the `modified_at`/redaction rehash

**Files:** Modify `src/cli/pull/common.rs` (extend `contains_hidden_fields` self-heal, ~:528).

- [ ] **Step 1: Write a failing test** — a legacy on-disk file carrying `modified_at` (or a raw `agenda_id`/`status`) with a recorded lockfile hash from the old format; assert the first pull/sync rewrites it to canonical form and rebases the lockfile **without** emitting a conflict.
- [ ] **Step 2: Run red.**
- [ ] **Step 3: Implement** — when the only delta between on-disk and codec-canonical bytes is a now-stripped hidden field or a now-redacted volatile field, treat it as a silent self-heal rewrite (rebase the lockfile to the codec hash), not a RemoteEdit/conflict.
- [ ] **Step 4: Run green**, full suite, clippy. Commit `fix(pull): self-heal legacy snapshots to codec-canonical form`.

---

## Phase 5 — Final verification

### Task 26: Full-tree verification + residual-site sweep

- [ ] **Step 1: Grep sweep** for any residual inline byte production in the migrated paths:

```bash
rg -n "to_vec_pretty\((e|q|h|w|f|l|t|s|inbox|remote_engine|&created|&updated)\b" src/cli/pull src/cli/sync src/cli/push src/cli/deploy
```
Expected: no matches (every model serialization goes through a codec). Investigate any hit.

- [ ] **Step 2: Run the whole suite + lints + format.**

```bash
cargo fmt --check          # rustup component add rustfmt if missing
cargo clippy -p rdc --all-targets --locked -- -D warnings
cargo test -p rdc --locked
cargo test -p rdc --locked --test codec_invariant
```
Expected: all green; codec_invariant passes for every kind.

- [ ] **Step 3: Manual smoke (optional, owner-run):** against a real or mocked org, run `rdc pull` then `rdc sync` twice; confirm the second run reports no changes (clean convergence) and `engine.json`/hook `.json` show the sentinel, not live values.

- [ ] **Step 4: Final commit.**

```bash
git commit -am "refactor(snapshot): complete KindCodec cutover; invariant test green"
```

---

## Self-review checklist (run before handing off)
- Spec §3 trait → Tasks 1–2. Invariant test (§8.1) → Task 3. Per-kind §4 → Tasks 4–16. Call sites §5 → Tasks 18,20,22,23,21. Live bugs §1: a→23, b→19, c→22, d→21, e→21, f→Tasks 4–16 (modified_at strip), g→24. Migration §7 → Task 25. Outbound §6 → codec `create_body`/`cross_env_body` (Tasks 4–16) + Task 23.
- Big-bang (§9): cutover is Phase 3 (all sites), guarded by invariant test + full suite.
