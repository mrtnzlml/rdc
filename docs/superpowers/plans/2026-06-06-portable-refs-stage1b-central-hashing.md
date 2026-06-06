# Portable Refs — Stage 1b: Central Lockfile-Aware Hashing (option B)

> **For agentic workers:** Execute with superpowers:subagent-driven-development or executing-plans. TDD, full suite + clippy -D warnings + fmt green, never push, no customer names, commit trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

**Goal:** Make rdc's content hash *reference-form-agnostic* so a URL-form body and an `rdc://`-form body of the same object hash IDENTICALLY. This lets portable `rdc://` snapshots coexist with the existing URL-based three-way merge WITHOUT portabilizing at every comparison site (the per-site approach sprawled across pull/classify/push/deploy/merge — see the WIP commits).

**Why B (decided with the user):** The friction is that on-disk = `rdc://`, API remote = URL, and they're compared everywhere. No lockfile-free normalization exists (URL carries the numeric id, `rdc://` carries the slug). The multi-threaded tokio runtime + `spawn`s rule out an ambient/thread-local lockfile, so the lockfile must be threaded into the hash layer. Done once in `canonicalize_for_hash`, it fixes ALL comparison sites at once and can't miss one.

---

## Starting point / base

Current `main` HEAD `3c55666` is WIP (RED). Decide ONE of:
- **Recommended:** `git reset --hard 0c14d0c` (clean portable-refs core: `refs` module, `is_url` rdc://, `walk_strings` lift, push `resolve_value`, pull post-pass, mdh clippy fix), then re-add only the still-needed pieces (below) on the way. This drops the per-site portabilize hacks cleanly.
- Or keep `3c55666` and *delete* the redundant per-site portabilize (classify 12 sites, pull-driver `portabilize_proposed`, deploy `tgt_drift_status`/`apply` inline) as part of this work.

**Keep regardless (needed by B):** `src/snapshot/refs.rs`; `noise::is_url` rdc://; push `resolve_value` (rdc://→URL for outbound bodies — NOT a hashing concern); the pull post-pass `portabilize_refs` (writes rdc:// to disk = the portability mechanism); `lockfile::lookup_url`/`slug_for_url` rdc:// recognition (B's normalizer uses `lookup_url`); `doctor::store_anomaly` old-hook-remote-url-by-id; `deploy::rewrite_urls` via `lookup_url` (resolves rdc:// in outbound deploy bodies).

---

## Task 1: normalize refs inside `canonicalize_for_hash`

**File:** `src/snapshot/noise.rs`

- Change `pub fn canonicalize_for_hash(bytes: &[u8]) -> Vec<u8>` → `pub fn canonicalize_for_hash(bytes: &[u8], lockfile: &crate::state::Lockfile) -> Vec<u8>`.
- After parsing to `Value`, FIRST call `crate::snapshot::refs::portabilize_value(&mut value, lockfile)` (URL→`rdc://` for portable kinds; `rdc://` and unresolvable URLs pass through unchanged), THEN the existing `strip_noise_fields` + `sort_url_arrays` + `sort_keys_recursive`.
- Net: `canonicalize_for_hash(url_bytes, lf) == canonicalize_for_hash(rdc_bytes, lf)` for the same object. An empty lockfile (`Lockfile::default()`) → portabilize is a no-op → preserves today's behavior (URLs hash as URLs) — this is what unit tests pass.

**TDD:** test `canonicalize_url_and_rdc_forms_hash_identically`: build a tiny lockfile (a workspace at a URL); assert `canonicalize_for_hash(br#"{"workspace":"https://x/api/v1/workspaces/1"}"#, &lf) == canonicalize_for_hash(br#"{"workspace":"rdc://workspaces/main"}"#, &lf)`. Existing noise tests: pass `&Lockfile::default()`.

---

## Task 2: thread `&Lockfile` through the hash entry points

**File:** `src/state/lockfile.rs` (+ `snapshot/codec/mod.rs` for `combined_hash`)

Add a `lockfile: &Lockfile` param to each and forward it to `canonicalize_for_hash`:
- `content_hash(bytes, lockfile)`
- `combined_hash(json, sidecars, lockfile)` (in `snapshot/codec/mod.rs`)
- `hook_combined_hash(json, code, lockfile)`, `rule_combined_hash(json, code, lockfile)`, `schema_combined_hash(json, formulas, lockfile)`

(For `content_hash` defined in `lockfile.rs`, taking `&Lockfile` is natural. Watch the borrow: callers usually have the lockfile by shared ref already.)

---

## Task 3: compiler-guided threading of all call sites

Run `cargo build` and fix every call site the compiler flags (~100, mostly mechanical):
- **Production sites** (pull/*, sync/mod.rs, sync/execute.rs, push/*, deploy/*, snapshot/*): pass the in-scope lockfile (`ctx.lockfile`, `tgt_lockfile`, `&self`/`lockfile`, etc.). The lockfile is in scope at essentially all of them.
- **Test sites** (`#[cfg(test)]`): pass `&Lockfile::default()` unless the test specifically exercises ref normalization (then build a small lockfile). Most tests hash fixture bytes with no portable refs → empty lockfile is correct and preserves expectations.
- Where a site truly has no lockfile (rare), thread one in from its caller.

Do this in commits grouped by directory (pull, sync, push, deploy, snapshot, state) so review is tractable. Keep the suite compiling after each group (the param is required, so it won't be green until all sites are done — build, don't test, between groups).

---

## Task 4: remove the now-redundant per-site portabilize

With B, the hash is form-agnostic, so these become dead/no-ops — remove them:
- `from_catalog_scan_lockfile` (sync/mod.rs): the 12 `portabilize_proposed(&json, lockfile)` lines.
- pull drivers: the `portabilize_proposed` shadow lines (the post-pass already writes rdc:// to disk).
- `deploy/common::tgt_drift_status`: the `portabilize_for_hash` line; `deploy/apply.rs`: the 3 inline `portabilize_for_hash` lines. (Keep `portabilize_for_hash` only if still used; else delete it.)
- `pull/common::portabilize_proposed`: delete if no longer referenced.

Leave the pull post-pass `portabilize_refs` IN PLACE (it writes rdc:// to disk for portability; its re-hash now matches because B-hash is form-agnostic, so it should report no-op on already-converted files).

---

## Task 5: verify

1. `cargo test` — FULL suite green (lib + cli_sync + cli_deploy + cli_doctor + codec_invariant + …). The cli_sync failures (idempotency, push-skip, conflict, legacy-converge, auto-merge) should ALL resolve because remote(URL) now hashes == base(rdc://). A few tests assert on-disk/merged self-`url` form — update those assertions to `rdc://` (legitimate: the snapshot is portable now); do NOT weaken behavioral assertions.
2. `cargo clippy --all-targets -- -D warnings`; `rustfmt --edition 2024 <changed files>` (NEVER `cargo fmt` — reformats the whole workspace).
3. e2e: `tests/portable_refs.rs` — pull writes rdc://, second pull byte-identical (idempotent), sync reports Clean.
4. Non-destructive live verify (temp copy of a real project, fresh-built binary, valid token): pull → rdc:// on disk; second pull no-op; `deploy --dry-run` still runs.

---

## Performance note (acceptable; optimize later if needed)

`canonicalize_for_hash` now parses + walks + portabilizes on every hash (100+/sync). If profiling shows it matters, cache normalized bytes per object or skip normalization when the bytes contain no `://`. Correctness first.

## Self-review

- Spec coverage: B makes the portable-snapshot design (spec §3) work without the per-site sprawl; lockfile v3 (§3.6) / mapping v2 (§3.7) / migrate (§4) / deploy removal (§5) remain LATER stages.
- The single behavioral risk is an empty-lockfile unit test that *should* normalize but doesn't — guard by giving ref-exercising tests a real small lockfile (Task 1/3).
