# FINAL fix status — all bugs fixed + live-verified (2026-06-08)

Every bug found across the CRUD/edge-case live testing is now FIXED, committed to `main` (local, unpushed), suite-green, and LIVE-verified against TEST org 214757:
- **fix#1 DELETE batch error-isolation** — `e8ad2f9`. Skip-and-continue past a failed DELETE (400/409) instead of aborting. Live-verified (queue-subtree delete skips unique-type 400s + schema 409, still deletes queue/workspace).
- **fix#2 auto-merge data-loss** — `c863229`. A successful disjoint auto-merge now PUSHES the merged result (was: silently reverted next sync). Live-verified (audit-hold: local color + remote name → merged color reaches remote, no revert).
- **fix#3 push-CREATE positive test coverage** — `924a197`. 4 new tests (dependency-ordered create + flat kinds).
- **bug#1 deletion_requested-queue hook churn** — `8fabb94`. The collision-fix catalog-augment over-seeded workspace-less (`deletion_requested`) queues that the pull driver excludes, so classify portabilized a hook's queue ref to rdc:// while the pull-base kept it raw → perpetual RemoteEdit. Fix: augment seeds a queue only when `workspace.is_some()` (mirrors pull driver). Live-verified: the 5 hooks referencing deletion_requested queue 3760895 stop churning; org fully idempotent (0 changed).
- **BUG2 inbox push one-cycle re-pull** — `c695404`. Root cause (live-confirmed): the inbox PATCH response OMITS `bounce_email_to` but GET includes it (`null`); push re-baselined from the PATCH response → base shape ≠ classify's GET shape → one-cycle bundle re-pull. Fix: push driver re-fetches the inbox via GET after PATCH and re-baselines from THAT. Live-verified: inbox edit → push → 2nd sync `0 changed`. (The prior subagent's wiremock test was a false-negative — it echoed `bounce_email_to`; rewritten to faithfully omit-on-PATCH/null-on-GET, RED pre-fix → GREEN.)

Method lesson reinforced: wiremock can false-negative when it doesn't replicate a real API response-shape quirk (PATCH-vs-GET field omission). LIVE verification caught both BUG2's reality and the false test. Token lifetime ~1h; re-auth as needed.

---

# Live CRUD + edge-case test pass — lockfile v3 (2026-06-08, fresh token)

Binary: `target/release/rdc` at HEAD `5c23363` (portable refs + migrate + deploy removed + lockfile v3). Project `/tmp/rdc-coll2` (lockfile was v2). All restored to baseline after testing (sync `0 changed`); intentional collision fixtures kept; one `deletion_requested` queue+schema auto-purge ~2026-06-09.

**PASS (live):**
- **v2→v3 lockfile migration** — first v3-binary sync migrated a real 307-url v2 lockfile → v3 transparently: `0 changed`, lockfile now `version 3` + top-level `api_base`, **0** per-entry urls; re-sync `0 changed`. Derived urls (`{api_base}/{endpoint}/{id}`, organization→organizations) satisfy the 3-way merge against the live API.
- **UPDATE (PATCH) 8/8 writable kinds** — queues/schemas/hooks/inboxes/workspaces/rules/labels/email_templates each edited→synced→API-verified→reverted. Confirms v3 derived-url push live.
- **CREATE (POST) dependency-ordered** — local workspace+schema+queue subtree (rdc:// refs) → POST in order workspace→schema→queue; queue's refs resolved to the FRESH tgt ids. This is the migrate+sync create path.
- **Duplicates** — added a 3rd `Collision Hook` via API → pulled as distinct `collision-hook-3`, re-sync `0 changed`; deleted cleanly.
- **Auto-merge** — disjoint-field edits (local color + remote name) merged automatically (no false conflict).
- **Conflict (BothDiverged, same field)** — JSON: `.json.test` shadow written, local preserved, lockfile base preserved, API untouched, `0 changed` (no silent overwrite). Resolve via adopt-remote (shadow→local) cleared it.
- **Code/.py conflict** — same mechanism, `.py.test` shadow.
- **RemoteDelete** — non-tty: deferred with a `.test-deleted` marker (does NOT silently delete local); accepting (rm file+marker) reconciles.
- **LocalDelete** — tombstone + `--allow-deletes` → DELETE (hooks, workspace, labels).

**FIX STATUS (after the CRUD pass):**
- **#1 DELETE error-isolation — FIXED + committed (`e8ad2f9`) + live-verified.** `run_deletes` now skip-and-continues on per-object DELETE failure (warn + `failed` tally + retry hint), so one un-deletable object (400/409) no longer aborts the batch. Live: deleting a queue subtree skipped 2×400 (unique-type templates) + 1×409 (schema-referenced) while still deleting the queue+workspace; sync succeeded.
- **#2 auto-merge data-loss — root-caused, fix prototyped (works for labels), DEFERRED (stashed).** The fix (execute.rs:1216 auto-merge → promote-to-push when `lp` non-empty) is correct for simple kinds but regresses ~15 hook/rule/schema "conflict_*_never_silently_pushes" combined-hash tests (second sync PATCHes → mock error). Landing it safely needs a per-test audit (disjoint-merge vs genuine-conflict — mis-flipping a `never_silently_pushes` assertion could mask a real conflict-push) + combined-hash base handling. Stashed (`git stash` "wip: fix#2 ...") to keep the tree green; narrow bug (disjoint-field BothDiverged), deferred.
- **#3 push-CREATE positive test coverage — in progress** (additive).

**EDGE-CASE / BUG findings:**
- **DELETE-ABORT (medium):** a new queue makes the server auto-create 5 default email_templates; the `rejection_default` one returns `400 Cannot delete template with unique type` on DELETE, and that error **aborts rdc's entire delete batch** (`src/cli/push/deletes.rs`), orphaning the queue/schema/workspace. rdc should skip-and-continue past un-deletable server-managed defaults (and/or not track them).
- **Async queue delete:** `DELETE /queues/{id}` → 202, status `deletion_requested`, `delete_after` ~24h (not immediate 404); a schema 409s while its (trashed) queue still exists — delete ordering depends on the async purge.
- **BUG2 (low, self-healing):** editing an inbox → next sync re-pulls the queue+inbox bundle once (`queues (1 pulled, …, inboxes 1)`) before settling — parent-queue `modified_at` cascade / inbox rebaseline; converges in one extra cycle.
- **Auto-merge convergence (low):** when local edits field A and remote edits field B (disjoint), rdc merges but the local-only A change is not pushed and converges back to remote on the next sync — a local-only field edit can be silently dropped if the other side also changed (different field).

---

# Live verification — Stage 1 portable refs (option B)

**Env:** TEST sandbox org 214757 on `https://api.elis.rossum.ai/v1` (`@mrtnzlml (sandbox)`).
**Binary:** `target/release/rdc` (HEAD with portable-refs + option B).
**Scope:** 6 workspaces / 26 queues / 78 schemas (26 with content) / 32 hooks / 1 inbox / 1 rule / 1 workflow / 52 labels / 130 email_templates / 10 mdh datasets. Engines + engine_fields 403 (token lacks permission) → skipped cleanly.

## Validated (PASS)

- **Pull → rdc:// on disk for ALL kinds.** Every snapshot file uses `rdc://<kind>/<slug>` for tracked refs (queue.schema/workspace/hooks, hook.queues, inbox.queues, schema.queues, self-urls). Untracked refs correctly stay URLs: `webhooks`, `generic_engine` (engines 403 → no lockfile entry to resolve), `modified_by`/`organization` (users/org not portable kinds).
- **Idempotent re-sync (option B keystone).** Real API returns URL-form bodies; disk is rdc://-form; the ref-form-agnostic hash sees them equal → repeated `sync` reports no writes for every kind EXCEPT the label collision below. This is the decisive real-API proof that portable snapshots coexist with the URL-based 3-way merge.
- **Push PATCH with ref resolution** (resolve_value rdc://→URL before send) verified end-to-end on: queue (schema+workspace+hooks refs), hook (queues refs), inbox (queues ref), schema (queues ref), workspace (flat). API reflected every edit; reverts round-tripped.
- **Create (POST)** — new label `rdc://labels/rdc-live-probe` → POSTed (id 12247), local file got id, url stayed rdc://.
- **Delete (DELETE)** — local tombstone + `--allow-deletes` → DELETE; API GET → 404, lockfile entry removed.

## Bugs found

### BUG 1 — same-name slug collision → perpetual non-idempotency (FIXED)
**Status: FIXED + verified live.** Root cause: `from_catalog_scan_lockfile`'s pre-rebuild `augmented` lockfile seeded catalog objects missing from the lockfile with `slugify(name)` + clobbering `upsert`. Two same-named objects (one tracked, one not — OR both untracked on a fresh pull) collapsed onto one slug: the clobber dropped the tracked entry's id+base hash so it false-classified as RemoteCreate every sync, and the sibling was lost. Reproduced live across labels/workspaces/hooks/rules/queues ("7 changed" forever, one file per pair). Fix: the augment now allocates a UNIQUE slug per kind (`slugify_unique` against the live lockfile slugs) so it never clobbers. Verified: fresh pull of the collision-heavy org → re-sync **0 changed**, both of every pair on disk (`collision-*` + `collision-*-2`, all 52 labels). Unit test: `from_catalog_scan_lockfile_same_name_collision_yields_distinct_slugs`.

Collision test bed seeded on org 214757 (per "create them if missing"): workspaces 1748185/1748186, hooks 1832219/1832220, rules 1862/1863, queues 3852265/3852266 — all same-named pairs. Plus pre-existing `Trial vendor` labels 9920/11492 and `Collision Schema A` 9836069/9836070. Schemas (30 dup names) + email_templates are keyed by *queue* slug on disk, so their name dups are harmless by design (confirmed: never churned).

### (historical) BUG 1 — label slug collision → perpetual non-idempotency (PRE-EXISTING)
Two labels are both named `Trial vendor` (ids 9920 + 11492). Both slugify to `trial-vendor`. The lockfile records `trial-vendor`→9920 only; 11492 never gets a stable slug. Every `sync` re-pulls exactly 1 label forever (`labels (1 pulled)`, `1 changed`). 52 labels listed, 51 files on disk. Unrelated to portable refs (labels carry no refs). Class = [[project-identity-collision]] but the *initial allocation* of two same-name objects in one kind isn't deduped consistently between classify (subset) and process (write); the collision loser's id-pinned slug (`trial-vendor-2`) is never recorded.

### BUG 2 — inbox push does not re-baseline → one spurious pull (minor, self-healing)
After PATCHing an inbox, the lockfile base content_hash for the inbox is NOT updated to the pushed/returned content. Next `sync` therefore sees remote≠base → re-pulls the queue+inbox bundle once, re-baselines, then stable. Workspace/hook/queue/schema push DO re-baseline (revert round-trips immediately); inbox does not. Inboxes are pulled as queue children (`queues (N pulled, …, inboxes M)`), so the spurious pull surfaces under the queue driver.

## Not exercised
- Engines / engine_fields (403). Workflows are effectively read-only in this sandbox (queues=None, steps=[]).
- mdh push (datasets/indexes always re-`fetched` each sync but `0` writes → idempotent in writes).
