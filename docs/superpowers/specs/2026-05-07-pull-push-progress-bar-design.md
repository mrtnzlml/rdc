# Pull/Push UX Hardening — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-07
**Scope:** `rdc pull <env>` and `rdc push <env>`. Three coupled improvements:

1. **Per-kind progress bar** during pull/push.
2. **Colorful conflict resolver** for easier orientation.
3. **Noise-field suppression** in conflict detection (e.g. `modified_at`) — fields that the server churns on every touch should not produce false-positive conflicts.

`rdc deploy/apply`, `rdc diff`, `rdc status` keep their current output for this milestone, but noise-field suppression is shared infrastructure and naturally applies to `rdc status`'s edit detection too (called out in §3.4).

## Goal

Make pull/push feel quiet, confident, and predictable:

- **Bars instead of warning streams** → user sees ETA and steady progress instead of scrolling text.
- **Suppress orphan warnings** → replace per-item noise with a one-integer summary on each kind's done-line.
- **Colorize the conflict resolver** → when a real conflict surfaces, the user can scan local-vs-remote at a glance.
- **Strip noise fields from hash inputs** → conflicts only fire on meaningful divergence, not on `modified_at` churn.

## Non-goals

- Changing pull/push semantics other than the noise-field strip (network, hashing for non-noise fields, conflict resolution flow are unchanged).
- Re-defining the `--verbose` flag. All non-orphan warnings continue to print at their current verbosity.
- Adding progress bars to other commands (`deploy`, `diff`, `status`). Clear follow-ups.
- Animating in non-TTY output. CI / piped runs degrade to one log line per kind.
- Coloring `rdc diff <env>` output. (Same crate would make this a one-line follow-up; out of scope for this milestone.)
- Letting users configure the noise-field list at runtime. The list is a code constant; expanding it is a code change.

## User-visible behavior

### Pull

```
⠁ workspaces  listing…                                    ← phase 1: list call
⠁ workspaces  [████████░░░░] 8/12  ETA 0.7s              ← phase 2: bar
✓ workspaces  12 items                              2.1s  ← done line
⠁ queues      [████░░░░░░░░] 3/25  ETA 5.2s
✓ queues      23 items  (2 orphans skipped)         4.7s  ← orphans surfaced as count
⠁ hooks       [██████████░░] 28/30  ETA 0.2s
✓ hooks       30 items                              3.4s
…
✓ pull envs/dev  256 items, 2 orphans skipped, 0 conflicts   18.6s
```

- One bar at a time, top to bottom, in the existing pull-driver order (organization, workspaces, queues, hooks, rules, labels, engines, engine_fields, workflows, workflow_steps, email_templates, mdh, plus the index step at the end).
- For kinds with sub-fetches (queues fetch schema + inbox; mdh datasets fetch indexes + search-indexes), one tick = one top-level item fully written including all sub-fetches.
- Orphan-skipped counts appear on the done-line only when non-zero. Zero orphans → plain `✓ kind  N items  Xs`.
- Final pull-summary line is unchanged in shape but reuses the bar's accumulated counters.

### Push

Two visible phases.

```
⠁ push envs/dev  hashing local files…                     ← phase 1: hash scan
✓ push envs/dev  256 files scanned, 1 changed       0.4s

⠁ hooks  [█░░░░░░░░░░░] 0/1   ETA —                       ← phase 2: only kinds with changes
✓ hooks  1 patched                                  0.6s

✓ push envs/dev  1 patched, 0 conflicts             1.0s
```

If phase 1 finds zero changes:

```
⠁ push envs/dev  hashing local files…
✓ push envs/dev  no changes  (256 files scanned)    0.4s
```

No phase-2 bars when nothing to push.

### Other warnings

Conflict / drift-refusal / 403-permission / 405-method-not-allowed / 429-retry / push-canonical-overwrite warnings keep their existing text and stderr destination. They emit through `ProgressBar::suspend(|| eprintln!(...))` so the bar redraws cleanly.

### Conflict resolver — colorized

The M32 resolver already prints a unified diff and a prompt. This change colorizes both. Colors honor the same TTY/`--no-color`/`NO_COLOR` rules as the progress bar (§Section 6 below).

Sample (TTY, colors approximated in markdown):

```
[2/3]  envs/dev/labels/audit-hold.json — conflict          ← bold yellow

--- local                                                  ← bright red
+++ remote                                                 ← bright green
@@ -3,4 +3,4 @@                                            ← cyan
   "id": 42,
   "name": "Audit Hold",
-  "description": "Withhold for audit"                      ← red, prefixed `-`
+  "description": "Audit hold (revised wording)"            ← green, prefixed `+`
   "queues": [...]

[k]eep local  [r]emote  [e]dit  [s]kip (shadow file)  [a]bort >    ← bracketed letters bold cyan
```

Color scheme (fixed; not user-configurable in this milestone):

| Element | Color |
|---|---|
| `[N/M]  path — conflict` header | bold yellow |
| `--- local` line | bright red |
| `+++ remote` line | bright green |
| `@@ … @@` hunk header | cyan |
| `-` lines (local-only content) | red |
| `+` lines (remote-only content) | green |
| Action letters `[k]/[r]/[e]/[s]/[a]` | bold cyan |
| Errors / re-prompt (`unrecognized — pick one of …`) | dim |

Implementation: a small `colorize_diff_line(line: &str) -> String` helper applied in `prompt_resolve` after `unified_diff()` produces text. The `unified_diff` function itself is **not** changed — `rdc diff` uses it too and stays plain-text in this milestone.

The push-side drift resolver (`resolve_push_drift`) also goes through `prompt_resolve` and inherits coloring for free.

### Non-TTY (CI / piped) fallback

When stderr is not a TTY, `KindProgress` runs in **log mode**:

```
→ workspaces: listing…
✓ workspaces: 12 items, 2.1s
→ queues: listing…
✓ queues: 23 items, 2 orphans skipped, 4.7s
…
```

One `→` line when work starts, one `✓` line when work ends. No escape codes, no animation. Existing integration tests (which run non-TTY) observe this output and assert against it.

## Architecture

### New module: `src/progress.rs`

Owns the `indicatif` dependency. Exposes one type `KindProgress` with this surface:

```rust
pub struct KindProgress { /* private */ }

impl KindProgress {
    /// Create a new bar/log-line for a kind. Spinner-only; call `set_total` once known.
    pub fn start(kind: &'static str) -> Self;

    /// Once `list_*` returns N, switch from spinner to bar with denominator N.
    pub fn set_total(&self, n: u64);

    /// Advance the bar by one (one top-level item fully written).
    pub fn tick(&self);

    /// Increment the orphan counter (does NOT advance the main tick).
    pub fn skipped_orphan(&self);

    /// Wrap an stderr write so it doesn't corrupt the bar.
    /// Routes to indicatif's `suspend()` in TTY mode; passes through in log mode.
    pub fn suspend<F: FnOnce()>(&self, f: F);

    /// Drop emits the `✓` done-line. Explicit `finish()` is also available
    /// for kinds that need to emit a custom done message.
    pub fn finish(self);
}

/// Detect mode at construction. `Bar` uses indicatif. `Log` uses plain eprintln!.
enum Mode { Bar(indicatif::ProgressBar), Log { kind: &'static str, started: Instant, orphans: AtomicUsize, count: AtomicU64 } }
```

The `Drop` impl prints the done-line. Drivers do not see indicatif types directly.

The mode is decided at construction:
- `std::io::stderr().is_terminal()` AND no `--no-color` AND no `TERM=dumb` → `Bar`
- otherwise → `Log`

`--no-color` (the spec §6 reserved flag) gets its first real wiring through this module: TTY but disable colors in the bar template.

### Push two-phase

A new `src/cli/push/scan.rs` (or extension of `mod.rs`) owns phase 1: walk every writable kind's local files, compute the local combined hash, compare to lockfile, build a `Vec<(Kind, Slug)>` of items needing PATCH. Phase 1 runs under a single global spinner (`push envs/dev  hashing local files…`).

Phase 2 is the existing per-kind push drivers, but the driver receives the pre-computed change list and the loop iterates only over those items. Bar denominator = changed items for that kind. Skipped/no-change items don't contribute ticks because they're not in the iterator at all.

This is a real refactor: today each push driver hashes inline as it walks. The change separates "decide what to push" from "push it" — net code clarity win, plus it gives the bar an honest denominator.

### Orphan plumbing

Two specific call sites change behavior:

- `src/cli/pull/queues.rs:53` — `eprintln!("warning: skipping queue ... (orphan/hidden)")` → `progress.skipped_orphan()`. The text disappears from stderr; the count surfaces in the queues done-line.
- `src/cli/pull/email_templates.rs:36` and `:45` — same swap.

No other callers of "orphan" warnings exist (verified by grep).

### Noise-field suppression (§3)

**Problem.** Today, `content_hash` is computed over the full canonical JSON of an object. Rossum's API stamps several fields on every server-side touch (`modified_at` is the canonical example; `modifier` likewise). When the server touches an object for any reason — even a no-op — the local file is unchanged but the *remote* hash diverges from the lockfile's `content_hash`. Re-pulling then surfaces a "conflict" the user cannot resolve meaningfully (the only difference is a timestamp).

**Fix.** Apply a stripping projection at hash-computation time. The on-disk JSON file keeps every field (matches API output, useful in editor and diff). The hash is computed over a copy with noise fields removed.

**Noise field list (initial).** A constant in `src/snapshot/noise.rs`:

```rust
/// Fields stripped from JSON values before content_hash is computed.
/// These are server-managed and change without user intent, so including
/// them in the hash creates false-positive conflicts.
pub const NOISE_FIELDS: &[&str] = &["modified_at", "modifier"];
```

The list is a code constant. Adding more fields is a code change with explicit rationale per addition (no runtime config).

**Stripping helper.**

```rust
/// Walk `value` and remove any object key whose name is in NOISE_FIELDS.
/// Recurses into nested objects and arrays. Mutates in place.
pub fn strip_noise_fields(value: &mut serde_json::Value);
```

**Where it's applied.**

| Site | Today | After |
|---|---|---|
| Pull driver `serialize_X` → `bytes` → `sha256(bytes)` | hash includes noise | parse bytes → strip → re-serialize → hash |
| Push driver local-hash for drift check | hash includes noise | same projection |
| Push driver remote-hash compare | hash includes noise | same projection |
| `cli::status` flat-kind edit detection | hash includes noise | same projection |
| Conflict resolver diff display | shows full bytes | shows post-strip projection so the user sees only meaningful diffs |
| `cli::diff` (the user-facing diff command) | unchanged | unchanged (out of scope this milestone) |
| Lockfile's `modified_at` field | populated from object | unchanged (separate metadata; not part of content_hash) |

A new helper `hash_canonical(bytes: &[u8]) -> Result<[u8; 32]>` parses, strips, re-serializes (sorted keys via `serde_json::to_vec`'s default object ordering matches existing canonical form), and SHA-256s. All hash sites that compute `content_hash` for storage in the lockfile or for comparison go through this helper. The combined-hash variants (`hook_combined_hash`, `schema_combined_hash`) call `hash_canonical` for the JSON portion and feed the formula `.py` bytes through unchanged.

**Backward compat.** Existing M32 lockfiles have hashes computed without stripping. After this change, every kind's hash will diverge from its lockfile entry on first pull → false-positive conflicts on every object. Mitigations:

1. **Documented one-time effect.** The README's "upgrading from M32" note explains the one-time conflict storm and says "if nothing meaningful is different, run `rdc repair --rebuild-lock <env>` to re-baseline without per-conflict prompts."
2. **No lockfile schema bump.** The hash algorithm change is internal; the file shape stays at v2. (M10 set the precedent here.)
3. **Live verification.** Ahead of merging, run pull twice on the @mrtnzlml sandbox: first pull surfaces the false conflicts; `rdc repair --rebuild-lock` clears them; subsequent pulls are clean.

**What this does NOT change.**

- The on-disk JSON files keep every field, including `modified_at`. Users who edit them in `$EDITOR` see the full object.
- Lockfile `ObjectEntry::modified_at` (a separate metadata field, set during M2) is unchanged.
- Real diffs — anything other than `modified_at` / `modifier` — surface as before.

### Driver integration pattern

Each pull driver, before:
```rust
pub async fn pull_workspaces(ctx: &mut PullCtx) -> Result<()> {
    let items = ctx.client.list_workspaces().await?;
    for ws in items { /* … write … */ }
    Ok(())
}
```

After:
```rust
pub async fn pull_workspaces(ctx: &mut PullCtx, progress: &KindProgress) -> Result<()> {
    let items = ctx.client.list_workspaces().await?;
    progress.set_total(items.len() as u64);
    for ws in items {
        /* … write … */
        progress.tick();
    }
    Ok(())
}
```

`pull/mod.rs` constructs the `KindProgress` per kind, passes it in, and drops it after the call (drop emits the done-line). The `for` loop in `pull/mod.rs::run_drivers` becomes a sequence of `let p = KindProgress::start("workspaces"); pull_workspaces(ctx, &p).await?; drop(p);` blocks (or a small helper).

For queues + mdh (which fan out concurrently via `buffer_unordered`), the tick is called inside the per-item futures, after the per-item write. `KindProgress`'s tick path is `&self` and uses `AtomicU64` for the log-mode count, so it's safe to call from concurrent tasks. Indicatif's `ProgressBar::inc()` is thread-safe.

## UX commitments

(Per Martin's UX-first principle — naming the UX surface of each component.)

- **Bar template:** `{spinner} {prefix:<14}  [{wide_bar}] {pos}/{len}  ETA {eta}` (TTY) / `→ {kind}: listing…` then `✓ {kind}: {count} items[, {n} orphans skipped], {dur}` (log mode). Bar refresh 10 Hz.
- **Color:** indicatif default scheme (cyan bar, green `✓`, dim `→`). `--no-color` disables.
- **Width:** bar fills available stderr width; falls back to 40 chars on detection failure.
- **ETA accuracy:** based on average tick duration so far. Inaccurate for first ~2 ticks, stable thereafter. ETA `—` shown until at least 2 ticks have completed.
- **No tearing:** every `eprintln!` in pull/push paths runs through `progress.suspend()`.
- **Drop safety:** if a driver returns `Err`, the `KindProgress` is dropped on unwind; the done-line is suppressed in that case (no `✓` for a failed kind). The error then propagates as today.
- **Idempotency feel:** `rdc push` with no local changes prints one short line and exits. No bars appear. Same shape as `rdc status` clean output.

## Predictability commitments

(Per Martin's predictability principle — non-negotiable acceptance criteria.)

- **Determinism:** progress output is non-deterministic by nature (timing-based). Lockfile / snapshot output is unchanged byte-for-byte. The pull-twice-zero-diff property holds.
- **Atomicity:** the bar is a UI layer; no progress state is persisted. Ctrl-C tears down the bar (indicatif handles SIGINT) and the existing atomic-write guarantees apply unchanged.
- **No hidden mutations:** orphan counts surface in the done-line, in the final summary, and in the (already-existing) lockfile state. No silent drops.
- **Plan-before-apply:** push phase 1 / phase 2 split now mirrors pull's "list then apply" — push briefly shows what it's about to do (`256 files scanned, 1 changed`) before the bar appears. This is closer to plan-then-apply than today's interleaved version.

## Implementation footprint

- **New deps:**
  - `indicatif = "0.17"` (progress bars).
  - `anstyle = "1"` (ANSI colors, NO_COLOR-aware, ~10KB; already a transitive dep of clap so the Cargo.lock add is small).
- **New files:**
  - `src/progress.rs` — `KindProgress`, `Mode`, mode-detection helper. ~150 LOC + ~50 LOC of unit tests.
  - `src/snapshot/noise.rs` — `NOISE_FIELDS` constant, `strip_noise_fields(value)`, `hash_canonical(bytes) -> [u8; 32]`. ~40 LOC + ~60 LOC of unit tests.
- **Modified files:**
  - `src/cli/pull/mod.rs` — construct/drop `KindProgress` around each driver call; thread orphan total + duration into final summary.
  - `src/cli/pull/queues.rs` — orphan `eprintln!` → `progress.skipped_orphan()`; tick after each queue.
  - `src/cli/pull/email_templates.rs` — same swap; tick after each template.
  - `src/cli/pull/{workspaces,hooks,rules,labels,engines,engine_fields,workflows,workflow_steps,organization,mdh}.rs` — accept `&KindProgress`; call `set_total` after `list_*`; `tick()` per item.
  - `src/cli/pull/common.rs` — wrap remaining `eprintln!`s in `progress.suspend()`. `apply_pull_action` grows a `&KindProgress` parameter so conflict warnings route through suspend.
  - `src/cli/push/mod.rs` — phase-1 hash scan extracted; phase-2 driver dispatch consumes a pre-computed change list.
  - `src/cli/push/{hooks,rules,labels,queues,schemas,inboxes,email_templates,engines,engine_fields}.rs` — iterate change list (not full local tree); tick per PATCH.
  - `src/cli/resolve.rs` — colorize `[N/M]` header, `--- / +++ / @@ / -line / +line`, action-letter prompt. Strip noise fields from local + remote bytes before `unified_diff`. Honor `--no-color` and `NO_COLOR`. (Push-side `resolve_push_drift` reuses `prompt_resolve` and inherits color for free.)
  - All `serialize_X` / `*_combined_hash` call sites — switch the JSON-content hash path to `hash_canonical` (which strips noise before hashing). Sites: `src/snapshot/{hook,schema,workspace,queue,inbox,email_template,rule,label,engine,engine_field,workflow,workflow_step,organization,mdh/*}.rs` and any current direct `sha256(json_bytes)` calls in pull/push drivers.
  - `src/cli/status.rs` — naturally inherits noise-stripping via the shared helper; no logic change beyond switching its hash call.
  - `src/api/retry.rs` — retry-warning `eprintln!` runs through `progress.suspend()` when a progress handle is in scope. Plumbing: explicit parameter passed down through `send_with_retry` (preferred over thread-local for testability); falls back to plain stderr when None.
- **Cargo.lock:** updated.

## Test plan

### Unit tests

In `src/progress.rs`:
- `kind_progress_log_mode_done_line_no_orphans` — capture stderr, assert format `✓ workspaces: 12 items, X.Xs`.
- `kind_progress_log_mode_done_line_with_orphans` — assert format `✓ queues: 23 items, 2 orphans skipped, X.Xs`.
- `kind_progress_log_mode_start_line` — assert `→ workspaces: listing…` printed on construction.
- `kind_progress_drop_on_error_suppresses_done_line` — construct, drop without `finish`; assert no `✓` printed.

In `src/snapshot/noise.rs`:
- `strip_removes_top_level_modified_at` — input has `modified_at` at top-level → removed.
- `strip_removes_nested_modified_at` — nested objects (e.g. inside arrays) also stripped.
- `strip_removes_modifier` — both noise fields are handled.
- `strip_leaves_other_fields_alone` — `name`, `id`, `url`, custom fields untouched.
- `strip_handles_array_of_objects` — array elements walked recursively.
- `hash_canonical_equal_when_only_modified_at_differs` — two payloads differing only in `modified_at` produce identical hashes.
- `hash_canonical_differs_on_real_content_change` — change to `name` produces a different hash.

In `src/cli/resolve.rs`:
- `colorize_diff_no_color_returns_plain` — when `NO_COLOR=1` (or stderr non-TTY) is set, no ANSI codes appear.
- `prompt_short_circuits_when_only_noise_differs` — local + remote differ only in `modified_at` → resolver short-circuits to `KeepLocal` (extends existing `prompt_skips_when_local_equals_remote`).
- `colorize_diff_renders_minus_lines_red` — opt-in test; force-enable colors via the resolver's color-mode override and assert ANSI bytes in the output.

No bar-rendering tests; trusting indicatif's coverage. No real-color rendering tests beyond byte presence; trusting `anstyle`'s coverage.

### Integration tests (existing files, updated)

- `tests/cli_pull.rs` — assertions move from "stderr contains 'pulled N items'" to "stderr contains '✓ workspaces:' and '✓ queues:'". Tests run non-TTY (cargo test default) so they observe log mode.
- New: `pull_with_orphan_queue_surfaces_count_in_done_line` — fixture has one queue with `workspace: null`. Assert stderr contains `✓ queues: N items, 1 orphans skipped`.
- New: `pull_no_conflict_when_only_modified_at_differs` — fixture: clean snapshot, then re-pull where the mock returns the same objects with newer `modified_at`. Assert no conflict surfaces; lockfile `content_hash` is unchanged; no `✓ <kind>: …, N conflicts` count.
- `tests/cli_push.rs` — new assertion shape: `→ push envs/dev: hashing…` then `✓ push envs/dev: 256 files scanned, 1 changed` then `✓ hooks: 1 patched`.
- New: `push_with_no_changes_prints_no_bars` — assert exactly two stderr lines (`→` and `✓ push envs/dev: no changes`).
- New: `push_no_drift_when_only_modified_at_differs` — local edited (real content change); remote returns the object with stale-base content but newer `modified_at`. Assert push proceeds (no drift refusal) — the `modified_at` change does not register as drift.

### Manual verification (against @mrtnzlml sandbox)

Per the M15-onward convention. Live-pull, live-push (edit hook description, push, pull, verify idempotent). Confirm:
- TTY output renders cleanly (no torn lines on retry warnings).
- Non-TTY (`rdc pull dev 2>&1 | tee /tmp/log`) produces log-mode output.
- Orphan queue (live sandbox has known orphans) produces `✓ queues: N items, M orphans skipped` line.
- Push with no changes exits cleanly with one short summary line.
- Trigger a real conflict (edit local + tamper lockfile content_hash for one item) and confirm the colored prompt is readable. Run again with `NO_COLOR=1` and confirm plain output.
- After upgrade, the first `rdc pull dev` surfaces the expected one-time false conflicts (hash-algorithm change). `rdc repair --rebuild-lock dev` clears them. Subsequent `rdc pull dev` is clean.
- Pull twice with no remote changes between runs: confirm no conflicts surface (the `modified_at` server churn is now invisible).

## Open questions

None — all design choices made during 2026-05-07 brainstorm:

1. ✅ Per-kind bar shape (option A from brainstorm).
2. ✅ Orphans summarized as count, not silent and not verbose-gated (option B).
3. ✅ Push two-phase: hash scan + PATCH bar (option B).
4. ✅ Other warnings: pass through unchanged via `suspend()` (option A).
5. ✅ Library: `indicatif` for bars; `anstyle` for colors.
6. ✅ TTY fallback: log mode with `→` / `✓` lines.
7. ✅ Out of scope this milestone: `deploy`, `diff`, `status` progress bars.
8. ✅ Conflict resolver coloring: fixed scheme (red/green git-style for diff, yellow header, cyan action letters); honors `NO_COLOR` and `--no-color`.
9. ✅ Noise-field list: `modified_at`, `modifier` (code constant; expansion is a code change with rationale).
10. ✅ Backward compat: hash-algorithm change accepted as a one-time false-conflict storm; `rdc repair --rebuild-lock` is the documented recovery path.

## Risks and mitigations

- **R: `--verbose` will eventually want to re-enable orphan warnings.** Mitigation: leave the call site as `progress.skipped_orphan()` and let `--verbose` mode in `KindProgress` re-emit the per-item text later. No coupling lost.
- **R: Push two-phase refactor breaks existing tests.** Mitigation: refactor is mechanical (extract hash logic from each driver into `scan.rs`); existing test assertions update to new output shape only. No behavior change.
- **R: Retry warnings during the bar might still tear in narrow terminals.** Mitigation: indicatif's `suspend()` handles redraw correctly; if not, fall back to plain eprintln in log mode.
- **R: `indicatif` + `anstyle` add binary size.** ~60KB combined on a release binary. Acceptable per spec §16 ("single static binary, distribute via brew/curl"). Cargo.lock churn is largely transitive deps already in use.
- **R: One-time false-conflict storm after upgrade (noise-field hash-algorithm change).** Mitigation: documented in README upgrade note; `rdc repair --rebuild-lock` is the recovery path; live-verified before merge. Same shape as the M10 hash-algorithm migration.
- **R: A field we should be stripping isn't on the noise list and surfaces as a noisy conflict.** Mitigation: `NOISE_FIELDS` is one constant in one file; adding a field is a one-line code change. Existing customers stay on the M32 behavior until they upgrade. Rationale per addition is documented in the source comment.
- **R: Stripping `modified_at` from the hash means we can't detect "remote was touched but content unchanged" anymore.** Accepted: that is exactly the goal. The lockfile's separate `modified_at` metadata field still records the timestamp for audit purposes; only the *conflict-detection* path stops looking at it.

## Acceptance criteria

The change is shippable when:

1. `rdc pull <env>` against a TTY shows per-kind bars, ETAs, done-lines, and a final summary. No orphan warnings appear on stderr; orphan counts appear on the relevant kind's done-line.
2. `rdc pull <env> 2>&1 | cat` produces log-mode output (no escape codes, one start + one done line per kind).
3. `rdc push <env>` shows a single phase-1 hash spinner, then phase-2 per-kind bars only for kinds with changes. No-change push exits with a single summary line.
4. Conflict / drift / 403 / 405 / 429 / canonical-overwrite warnings still print, do not corrupt the bar.
5. All existing tests pass (with assertion updates to new output shape).
6. Triggering a real conflict on a TTY surfaces a colored prompt: bold-yellow header, red `-` lines, green `+` lines, cyan hunk markers, cyan action letters. `NO_COLOR=1` (or `--no-color`) renders the same content with no escape codes.
7. Pulling twice in a row with no remote changes between the two pulls produces zero conflicts (the `modified_at` server churn is invisible to conflict detection).
8. Pushing a real edit succeeds even when the server has bumped `modified_at` since the last pull (`modified_at`-only divergence does not trigger drift refusal).
9. After upgrade from M32, the first `rdc pull` warns the user about the one-time hash-algorithm change and points at `rdc repair --rebuild-lock`. Running `repair` clears the storm; subsequent pulls are clean.
6. New tests pass: orphan-count surface, no-change-push silence, log-mode formatting.
7. Live verification on @mrtnzlml sandbox confirms TTY rendering, non-TTY fallback, and orphan handling.

---

*Generated during 2026-05-07 brainstorm. Implementation plan to follow via writing-plans skill.*
