# Selective Deployment — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-14
**Scope:** Add a per-invocation selection filter to `rdc deploy`, enabling customers to promote a chosen subset of their solution from one environment to another without touching unrelated objects. The existing whole-snapshot deploy remains the default behavior; no flag, no change.

## Goal

Let a customer who has already aligned two environments (test ↔ prod) promote a specific change — a hook, a queue plus its schema, a handful of objects — without re-deploying the rest of the snapshot.

The filter is **per-invocation**: a set of `--only` matchers on the command line. It does not touch `.rdc/map/<src>-to-<tgt>.toml`, the lockfile, overlays, or any other persistent state. Re-running with the same `--only` flags produces a byte-identical plan (predictability invariant). Re-running without `--only` performs a regular whole-snapshot deploy.

## Non-goals

- **Persisted "deployment subsets" or named features** in repo. YAGNI for v1; if patterns emerge, a thin layer that resolves a named subset into a list of `--only` flags can be added later without changing the engine.
- **VCS-derived selection** (e.g., "only what changed since last commit"). Out of scope; can be built externally by piping a list of slugs into `--only`.
- **Mapping-file edits as a side-effect of selection.** `--only` is read-only against the mapping; existing slug-pair entries and the auto-match step run unchanged.
- **Operation-type filtering** (e.g., `--no-update`, `--no-create`). One scope, all three phases. If a user doesn't want creates, they shouldn't include the new-only slugs in their selection.
- **A new top-level command.** `rdc deploy ... --only X` is the surface; no `rdc deploy-only` or similar.

## Background

`rdc deploy <src> <tgt>` (in `src/cli/deploy/run.rs`) currently operates on the whole snapshot in a single pipeline:

1. Auto-populate `Mapping` from same-slug pairs in both envs.
2. Compute a plan: per-kind file-system diff yielding **creates** (`src \ tgt`) and **deletes** (`tgt \ src`, only with `--mirror`).
3. Confirm on TTY, then execute:
   - **Creates** in dependency order: `workspaces → schemas → queues → inboxes → email_templates → hooks → rules → labels → engines → engine_fields`.
   - **Updates**: PATCH sweep driven by `Mapping.<kind>` entries; idempotent via drift check + canonical content compare.
   - **Deletes** (mirror): reverse dependency order, separate confirmation.

The pipeline has no scope filter. Every change in src that isn't already in tgt gets propagated. Selective deployment introduces exactly that filter, applied at the top of the pipeline and threaded through all three phases as a single concept.

## Design

### CLI surface

```
rdc deploy <src> <tgt> [--only <selector>]... [--mirror] [--dry-run] [--yes]
```

`--only` is repeatable. The effective selection is the union of every matcher's resolved set. Absent `--only` → whole-snapshot deploy, current behavior unchanged.

Selector grammar:

| Form | Example | Matches |
|---|---|---|
| `<kind>/<slug>` | `hooks/validator-invoices` | exact `(kind, slug)` |
| `<kind>/<glob>` | `schemas/cost-*`, `hooks/*` | `*` glob within the slug segment |
| `email_templates/<ws>/<q>/<tpl>` | `email_templates/main/cost-invoices/rejection` | matches the compound mapping key for email templates |
| `*/<glob>` | `*/cost-invoices` | any kind whose slug matches |

`<kind>` is one of the 10 deployable kinds (`workspaces`, `schemas`, `queues`, `inboxes`, `email_templates`, `hooks`, `rules`, `labels`, `engines`, `engine_fields`). Anything else → parse error.

`<glob>` supports `*` (zero or more characters within a single slug segment). No `?`, no character classes, no `**` — kept narrow because the selector unit is the slug, not a path.

### Selection resolution

Resolved once at the top of `cli::deploy::run::run`, before plan computation:

1. **Build the candidate set.** Walk both `envs/<src>/` and `envs/<tgt>/` to collect every `(kind, slug)` that exists in either env, across all 10 deployable kinds. (The same scanners `cli::deploy::run::list_slugs` already uses.)
2. **Apply each matcher.** For every `--only` matcher, match against the candidate set. Accumulate into a `BTreeSet<(String, String)>`.
3. **Empty-match guard.** A matcher that resolves to 0 objects is a hard error:
   ```
   error: --only 'hooks/foo' matched 0 objects in src 'test' or tgt 'prod'.
   ```
   Avoids silent no-op deploys from typos.
4. **Echo back.** The resolved selection is sorted and printed in the plan header so the user sees the exact set before confirming.

This single pass means: same `--only` flags + same snapshots → byte-identical selection → byte-identical plan. No matcher logic re-runs later in the pipeline.

### Cross-reference dependency check

Runs immediately after resolution, before any writes.

The deploy code already knows which JSON fields hold cross-object URLs — `cli::deploy::common::rewrite_urls` rewrites them. The dep check reuses the same field list to flag missing peers.

For each selected `(kind, slug)`:

1. Read the src JSON.
2. Walk the URL-typed fields:
   - `queue.workspace`, `queue.schema`
   - `email_template.queue`
   - `rule.queues[]`
   - `hook.queues[]`, `hook.run_after[]`
   - workspace and queue children references already covered by their parents' fields
3. For each referenced URL, identify the src `(kind, slug)` via the src lockfile. Classify:
   - Pair is in the selection → ok.
   - Pair is in the tgt lockfile under its mapped slug → ok (URL rewrite handles it at execute time).
   - Pair is missing from both → **unresolved dependency**.
4. If the unresolved list is empty → proceed silently.
5. If not empty:
   - **TTY**: print the list and prompt:
     ```
     The following objects in your selection reference peers that aren't in
     the selection and don't exist in 'prod' yet:

       hooks/validator-invoices  → queues/new-invoice-flow (missing)
       queues/new-invoice-flow   → workspaces/finance (missing)

     Include these 2 dependencies in the selection? [Y/n]
     ```
     Yes → fold them in; re-run the dep check (transitive: a newly-included dep may itself have unresolved peers). No → abort cleanly with exit code 0, no writes.
   - **Non-TTY / `--yes`**: refuse with the list and exit non-zero. Message includes the exact `--only` flags to append to re-try.

This makes the cross-ref check the **cascade mechanism**: a user can type `--only queues/cost-invoices` and the prompt offers to include the schema/inbox/templates that hang off it. No special recursive-selector syntax is needed.

### Pipeline integration

A single `Selection { items: BTreeSet<(kind, slug)> }` (or `None` when `--only` is absent) is threaded through three phases:

| Phase | Filter point |
|---|---|
| `compute_plan` | Filter both `src_slugs` and `tgt_slugs` per kind before computing creates/deletes. |
| Create loop in `run::run` | Skip slugs not in `Selection`. |
| `apply::run` (update sweep) | Walk only `mapping.<kind>` entries whose src slug is in `Selection`. |
| Mirror delete loop | Only consider tgt-only slugs that are in `Selection`. |

The existing drift check, URL rewriting, overlay application, and per-object idempotency are unchanged. `Selection` is purely subtractive on the candidate set.

### Plan output

Whole-snapshot deploy: unchanged.

Scoped deploy:

```
Plan: test → prod  (selection: 3 objects via --only)
  Selected:
    hooks/validator-invoices
    queues/cost-invoices
    schemas/cost-invoices
  Filtered out: 21 src objects not matched by --only

  + create:  1 schemas, 1 queues
  ~ update:  field-level deltas (resolved at execute time)
```

If the dep-check prompt added items:

```
  Selected (3 + 2 from dep check):
    hooks/validator-invoices
    queues/cost-invoices         [added by dep check]
    schemas/cost-invoices
    workspaces/finance           [added by dep check]
```

Summary line names the scope: `Deployed test → prod (scoped, 3 objects): 2 created, 0 deleted, 4 API calls, 8.1s`.

`--dry-run --only X` shows the scoped plan and per-object diffs for the in-scope items; zero API calls (one drift-check GET per in-scope updateable object is preserved for accurate field-level previews — same as today's `--dry-run`).

### Errors & UX

- 0-match selector → named error including the offending flag.
- Unknown kind in selector → error listing valid kinds.
- Bad glob (e.g., `?`, `**`) → fail at flag parse time with a one-line example of valid syntax.
- TTY dep-check prompt is suppressed by `--yes` (matches existing rdc convention). With `--yes`, the deploy refuses on missing deps.
- `--only` help text shows one example per selector form.
- `--mirror --only X`: both the deploy confirmation AND the destructive confirmation still fire on TTY. The "would delete" count only includes in-scope items.
- Plan header always names the scope when `--only` is active, so the user can't mistake a scoped run for a full deploy at a glance.

### Code shape

- New module: `src/cli/deploy/selection.rs`. Public items:
  - `pub struct Selection { items: BTreeSet<(String, String)> }`
  - `pub struct Matcher { kind: Option<String>, glob: Pattern }` (`kind=None` for `*/<glob>`)
  - `pub fn parse_matchers(raw: &[String]) -> Result<Vec<Matcher>>`
  - `pub fn resolve(matchers: &[Matcher], src_paths: &Paths, tgt_paths: &Paths) -> Result<Selection>`
  - `pub fn dep_check(selection: &mut Selection, src_paths: &Paths, src_lockfile: &Lockfile, tgt_lockfile: &Lockfile, interactive: bool) -> Result<()>`
  - `impl Selection { pub fn contains(&self, kind: &str, slug: &str) -> bool }`
- Glob compilation: hand-rolled `*`-only matcher (~20 lines). The `glob` crate isn't currently a dep, the grammar is intentionally narrow (just `*` within a slug segment), and adding a dep for one feature isn't worth the cost. `regex` is already a dep but its syntax (`.*`) is more error-prone for users — keep the surface narrow.
- `cli::deploy::run::run` signature gains `only: Vec<String>`. Internally builds `Option<Selection>`.
- `cli::deploy::run::compute_plan` gains `selection: &Option<Selection>` and `cli::deploy::apply::run` likewise. Filter is a single `selection.as_ref().map_or(true, |s| s.contains(kind, slug))` check.
- `cli/mod.rs::Command::Deploy` gains:
  ```rust
  #[arg(long = "only", value_name = "SELECTOR", action = clap::ArgAction::Append)]
  only: Vec<String>,
  ```

No changes to `Mapping`, `Lockfile`, `Overlay`, or any on-disk schema. The selection lives entirely in memory for the duration of the command.

### Testing

Unit tests (`src/cli/deploy/selection.rs`):

- `parse_matchers` accepts each valid form; rejects bad kind, bad glob.
- `Matcher::matches` for literal, kind-scoped glob, cross-kind glob, compound email_template key.
- `resolve` returns the union; empty-match selector errors.
- `dep_check` walker:
  - hook with `queues: [<url>]` → finds the queue ref.
  - queue with `workspace` + `schema` refs → finds both.
  - schema with no cross-refs → no-op.
  - email_template with `queue` ref → finds it.
- `dep_check` classification:
  - dep in selection → ok.
  - dep in tgt lockfile → ok.
  - dep missing from both → unresolved, returned in the list.
- Transitive dep inclusion: include a queue, re-check finds its workspace missing.

Integration tests (`tests/`, wiremock-backed, mirroring the existing deploy integration test layout):

- `--only hooks/X` on a setup with several local edits → only `hooks/X` is PATCHed; other unchanged kinds get zero API calls (verified via mock hit counts).
- `--only X` with missing dep, `--yes` → exits non-zero, zero writes hit mocks.
- `--only X` with missing dep, simulated TTY-yes (e.g., scripted stdin) → dep included, both objects PATCHed.
- `--only X --mirror` with tgt extras both in and out of selection → only the in-selection tgt extras are deleted.
- `--only 'hooks/*'` with three hook diffs → all three deployed, no other kinds touched.
- `--only '*/cost-invoices'` on a snapshot with `queues/cost-invoices`, `schemas/cost-invoices`, `inboxes/cost-invoices` → all three selected (cross-kind glob).
- `--only foo/bar` matching nothing → hard error, zero writes.
- `--only X --dry-run` → scoped plan printed, zero PATCH/POST/DELETE mocks hit.

## Open questions

- **Exit code for "user said No at dep prompt".** Proposal: 0 (clean abort, not a failure). Confirm during implementation.
- **Plan header truncation.** For large selections (50+ items), list them all in the header or fold to a count + first-N? v1 lists them all; revisit if real selections get unwieldy.

## Out of scope (deferred)

- `--skip`/`--except` (denylist) — `--only` with glob covers most needs; add later if a clear use case emerges.
- Named persisted selections in repo (`[selections.ap-invoices]` in `rdc.toml`).
- Interactive object picker (checkbox UI) — possible later UX layer on top of the same `Selection` engine.
- VCS-derived selection.
- `--only` on `rdc push` (single-env edits). This spec is deploy-only; push already operates per-file naturally.
