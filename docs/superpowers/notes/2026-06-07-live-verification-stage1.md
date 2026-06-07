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
