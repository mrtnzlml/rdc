# `rdc diff --raw` — design

- Date: 2026-05-27
- Status: approved (pending spec review)
- Scope: add a `--raw` flag to `rdc diff` that bypasses rdc's diff-time
  adjustment logic while keeping output readable.

## Motivation

`rdc diff` applies normalization so cross-env diffs surface only semantic
drift: it strips fields guaranteed to differ between environments
(`id`, `url`, `organization`, server back-references), strips noise
(`modified_at`, `modifier`), sorts keys and string-arrays, and rewrites
src→tgt cross-reference URLs via the deploy-managed mapping. This is the
right default — it answers "did the configuration meaningfully change?"

But sometimes you need the unadjusted view: to confirm exactly which
literal values differ between two environments (or between a local
snapshot and the live remote), including the fields normalization hides.
Today there is no way to see that without hand-inspecting files.

`--raw` adds that view. Default behavior is unchanged.

## Goals

- A single `--raw` flag on `rdc diff`, covering **both** diff modes:
  - env-vs-env (`rdc diff <a> <b>`, two local snapshots)
  - local-vs-remote (`rdc diff <env>`, local snapshot vs live remote)
- Semantics: **reveal, keep tidy.** Turn off field-stripping and URL
  rewriting; keep recursive key-sort, string-array sort, and code/formula
  sidecar separation so the output stays readable.
- Zero behavior change when the flag is absent.

## Non-goals

- No granular per-axis flags (`--show-ids`, `--no-rewrite`, …). One switch.
- No literal byte-for-byte dump. Sorting is retained; hook code stays in
  its sidecar stream rather than inlined.
- `--raw` does **not** re-apply overlays or otherwise materialize
  per-env effective config. It removes adjustments; it never adds data.

## Semantics: what `--raw` reveals

| Adjustment | Normal | `--raw` |
|---|---|---|
| Strip `id` / `url` / `organization` | stripped | **kept** |
| Strip server back-refs (`queue.hooks`, `queue.webhooks`, …) | stripped | **kept** |
| Strip `modified_at` / `modifier` | stripped | **kept** |
| Rewrite src→tgt cross-ref URLs (env-vs-env) | rewritten | **left literal** |
| Recursive key sort | yes | yes (kept) |
| String-array sort | yes | yes (kept) |
| Code / formulas in `.py` sidecar stream | yes | yes (kept) |

Note: `token_owner` already shows in today's env-vs-env diff (it is not a
stripped field). `--raw`'s marginal reveal in env-vs-env is `id`, `url`,
`organization`, server back-references, `modified_at`/`modifier`, and
literal (un-rewritten) cross-reference URLs. In local-vs-remote the two
sides are the same environment, so `id`/`url`/`organization` already
match; the marginal reveal there is `modified_at` on hooks/rules/schemas
(plus overlay-managed fields in overlay envs, which already surface in
that mode).

## Design

### CLI flag

- Add `#[arg(long)] raw: bool` to the `Diff` subcommand variant
  (`src/cli/mod.rs`, the `Diff { left, right }` arm).
- Thread it through dispatch (`crate::cli::diff::run(left, right, raw)`).
- Per-subcommand, not global. Composes with the existing global
  `--no-color`.
- Help text: "Show the unadjusted diff: reveal id/url/organization,
  server back-references, modified_at/modifier, and un-rewritten
  cross-reference URLs. Keys and string-arrays stay sorted for
  readability."

### Shared core: `tidy_raw`

A single primitive both modes call:

```
tidy_raw(value: &mut serde_json::Value):
    sort_string_arrays(value)        // existing helper
    sort_keys_recursive(value)       // existing helper (key_order.rs, pub)
    // caller pretty-prints + appends trailing newline
```

No stripping, no URL rewriting. Lives next to
`normalize_for_cross_env_compare` in `src/cli/deploy/common.rs` so it
sits beside the normalizer it mirrors and reuses the already-present
`sort_string_arrays`. Exposed `pub(crate)`.

### Env-vs-env mode (`diff_snapshot_vs_snapshot`)

When `raw`:

- Do not build `RewriteCtx` (skip mapping + lockfile loads — no rewrite).
- Replace `normalize_for_diff` with a tidy-raw path:
  - `.json` files: parse → `tidy_raw` → pretty-print (+ newline).
  - Non-JSON (`.py`, `.toml`, `.md`): pass through unchanged (identical
    to current behavior).
  - Parse failure: same fallback as today (pretty-print if parseable,
    else raw bytes).
- No kind lookup is needed in raw mode (the kind is only used to choose
  the per-kind strip set, which raw skips).

### Local-vs-remote mode (`diff_local_vs_remote` and helpers)

The `raw` flag threads into the six helpers (`diff_hooks`, `diff_rules`,
`diff_labels`, `diff_engines`, `diff_engine_fields`, `diff_queue_tree`).
Each kind builds its compared-JSON side via a thin raw serializer that
reuses the existing code/formula split but swaps canonical normalization
for `tidy_raw`:

- Hooks / rules / schemas: split code / `trigger_condition` / formulas to
  their sidecar stream exactly as today, then `tidy_raw` the remaining
  JSON. Do **not** strip `modified_at` (skip `strip_hidden_fields_recursive`)
  and do **not** apply the curated `HOOK_KEY_ORDER` reorder.
- Flat kinds (labels, engines, engine_fields, queues, inboxes,
  email_templates): `serde_json::to_value(model)` → `tidy_raw`.

Models carry `#[serde(flatten)] extra`, so serializing a model is lossless
— no API-client change is required to obtain "raw" remote data. Code and
formula `.py` streams diff exactly as today in both normal and raw mode.

### Key ordering in raw mode

Both modes use alphabetical `sort_keys_recursive` in raw (not the curated
`HOOK_KEY_ORDER`). This keeps the two raw modes consistent with each other
and with the existing env-vs-env normal path. Cosmetic only.

## Limitation (documented in `--help` notes and README)

In env-vs-env, `--raw` reveals what is hidden at **diff** time. It cannot
resurrect fields that *pull* stripped into `overlay.toml` — those values
are not present in the on-disk snapshot. To inspect overlay-managed
fields, use `--raw` in local-vs-remote mode (the live remote carries
them), or pull without the overlay.

## Affected files

- `src/cli/mod.rs` — `Diff` arg + dispatch.
- `src/cli/diff.rs` — thread `raw` through `run`, both mode functions,
  and the six local-vs-remote helpers; raw branch in the env-vs-env
  normalize step; raw serialize variants (or `raw` param) for the per-kind
  JSON sides.
- `src/cli/deploy/common.rs` — new `tidy_raw`; make `sort_string_arrays`
  reachable (already in this module).
- `src/snapshot/hook.rs`, `src/snapshot/rule.rs`, `src/snapshot/schema.rs`
  — thin raw serialize variants reusing the existing code/formula split
  plus `tidy_raw` (or a `raw` parameter on the existing serializers,
  whichever keeps call sites cleanest; decided in the plan).

## Testing

- Unit (`tidy_raw`): keys and string-arrays sorted; nothing stripped
  (`id`, `url`, `modified_at` survive); stable pretty output.
- Env-vs-env integration:
  - Two snapshots differing only in `id`/`url`/`organization` → normal
    diff is quiet; `--raw` shows them.
  - A differing server back-ref array (`queue.hooks`) → `--raw` shows it;
    normal does not.
  - A cross-ref URL that the mapping would rewrite → normal is quiet;
    `--raw` shows the literal per-env URLs.
- Local-vs-remote integration:
  - Hook whose remote `modified_at` differs from the snapshot → normal
    quiet; `--raw` shows `modified_at`.
  - A real code change → shown via the `.py` stream in both modes.
- Regression: with the flag absent, output is byte-identical to today
  (guard the existing diff snapshot/golden tests).

## Out of scope / future

- Applying `--raw` to `push --dry-run --diff` / `deploy --dry-run` (they
  share the renderer but not this flag). Can follow if wanted.
- Re-applying overlays to show materialized per-env config (a different
  feature).
