# Organization `settings` — push and promote

## Problem

`organization` is the only kind rdc snapshots but never writes. `rdc pull` puts
the whole org body at `envs/<env>/organization.json`; push, sync and migrate all
skip it (`push::scan` has no org scanner, `change_list_from_classified` drops the
kind, `migrate::should_skip` names the file). So a per-org UI setting that people
genuinely maintain — `settings.annotation_list_table.columns`, the document-list
columns — is versioned in git and then ignored: editing the file changes nothing,
and the edit is silently discarded the next time the remote body changes.

The same field one level down, on a queue, has always been managed. Verified on
the wire: after a local edit to `queue.json`, rdc sends

```
PATCH /api/v1/queues/501
{"settings":{"annotation_list_table":{"columns":[
   {"visible":false,"column_type":"schema","width":77,"schema_id":"LOCAL_COL","data_type":"number"}]}},
 "automation_level":"never","name":"Invoices","workspace":"…/workspaces/401","schema":"…/schemas/601"}
```

The org level should work the same way, and should promote between envs like
everything else.

## Verified facts

### Rossum API (probed against a sandbox organization on `api.elis.rossum.ai`)

| Probe | Result |
| --- | --- |
| `OPTIONS /v1/organizations/{id}` | `actions` lists **only `PUT`**; `settings` is `read_only=false`, and so is every level under `settings.annotation_list_table.columns` |
| `PATCH /v1/organizations/{id}` with `{"settings":{…}}` | **200** — PATCH works despite not being advertised; a fresh `GET` confirms it persisted |
| PATCH only `annotation_list_table` while `request_dashboard_table` also existed | **the sibling key was destroyed** — a `settings` PATCH REPLACES the object, it does not merge |
| `{"settings":{}}` | **200**, clears `settings` entirely (this is the restore path) |
| `columns: []` | accepted |
| `annotation_list_table: {}` (no `columns`) | accepted, server normalizes to `columns: []` |
| `data_type: "bogus"` | **400** `{"settings":{"annotation_list_table":{"columns":{"0":{"data_type":["\"bogus\" is not a valid choice."]}}}}}` — really validated, not a blob |
| `schema_id: "zzz_no_such_field"` | **200 — no existence validation.** An unknown field id is accepted silently |
| the wrapper shape `OPTIONS` advertises, `{"schema":{…}}` | **400** `column_type: This field is required.` — OPTIONS lies; the wire shape is FLAT |
| the PATCH response vs the GET response | **not the same shape** — PATCH returns `rir_key`, which GET omits entirely, and returns `users` in a different order (36 entries; `workspaces` order matched). A mock that answers both verbs with one body cannot see this |

Writable on an organization: `settings`, `ui_settings`, `metadata`. Read-only
per OPTIONS: `id`, `url`, `name`, `workspaces`, `users`, `rir_key`, `sandbox`,
`created_at`, `creator`, `modified_at`, `modified_by`, `internal_info`,
`organization_group`, `is_trial`, `expires_at`, `trial_expires_at`,
`oidc_provider`.

Column shapes (both accepted, both under `annotation_list_table` and
`request_dashboard_table`):

- schema column — `{visible: bool, column_type: "schema", width: float, schema_id: str≤50, data_type: "string"|"boolean"|"date"|"number"}`
- meta column — `{visible: bool, column_type: "meta", width: float, meta_name: <one of ~55 choices>}`

`width` comes back as a float: sending `120` returns `120.0`.

### rdc code

- Pull already writes the base cache for the org (`apply_pull_action`,
  `pull/common.rs:789-837`), so `try_auto_merge` has a base to work from once the
  org can diverge on both sides.
- `Overlay` has **no** `#[serde(deny_unknown_fields)]`, so an overlay carrying a
  new `[organization]` section still loads on an older rdc — it is ignored.
- The overlay dangling-key check iterates `Overlay::kind_maps()`
  (`migrate/mod.rs:425`), so a slug-less section is not subject to it.
- `mirror_prune_paths` is `enumerate_files(tgt) - remap(enumerate_files(src))`.
  Adding the org file to `enumerate_files` without a guard would delete a
  target's `organization.json` whenever the source env has not been pulled.
- A flat push-capable kind is registered in ~15 places: `ChangeList` (+ `total`,
  `is_empty`, the three `check(…)` sweeps), `push::scan`, `detect_tombstones`,
  `change_list_from_classified`, `push::mod` dispatch, `sync::mod` scan-change
  insertion, and four `match it.kind` sites in `sync::execute`.

## Decisions

1. **Write scope: `settings` only.** Not `ui_settings` (it holds
   `applied_feature_flags`, a per-org entitlement list, plus branding) and not
   `metadata`.
2. **Migrate promotes it**, restricted to `settings`, with overlay support.
3. **On by default.** No new config key. The classifier only pushes when the
   local file diverges from the recorded base, and `sync --dry-run` reports it
   before anything is written.
4. A promoted column whose `schema_id` is absent in the target is **warned
   about, never dropped** — the API accepts it (200), and silently editing
   deployable content is worse than a dead column.
5. Both tables under `settings` are covered by construction: rdc sends the whole
   `settings` object, so `request_dashboard_table` rides along.

## Design

### A. Push contract

`push/organization.rs`, one slug (`"self"`), PATCH only:

```
PATCH /v1/organizations/{id}   { "settings": <local settings, verbatim> }
```

Whole-object send is mandatory, not a shortcut — a partial send replaces (see
Verified facts). New API method `update_organization(&self, id, &Value) ->
Organization`, mirroring the other `update_*` methods.

Rules that follow from the API semantics:

- **`settings` key absent from the local file → skip the push, warn.** rdc
  cannot distinguish "this project does not manage org settings" from "clear
  them", and guessing the second wipes the remote. Clearing is expressed
  explicitly, as `"settings": {}`.
- **No create, no delete, ever.** The org is never added to `detect_tombstones`,
  so a deleted `organization.json` is a no-op for push and there is no DELETE
  path to reach. It cuts both ways: because the deletion classifies as `Clean`,
  `rdc sync` does not restore the file either — it stays missing until the org's
  lockfile entry is rebuilt (delete it from `.rdc/state/<env>.lock.json`, or on
  the env's first sync), which forces a fresh pull.
- **Local edits outside `settings` are not pushed**, and the write-back leaves
  them alone: it replaces `settings` in the on-disk body and keeps every other
  field as the pull wrote it.

  Such an edit then PERSISTS rather than being reverted promptly, which is worth
  stating because it surprised the author: with `settings` unchanged there is
  nothing to push, so the driver returns the "`settings` unchanged" skip, the
  lockfile base never advances, and the org keeps classifying `LocalEdit` — so
  the pull driver never runs for it either. Live-verified: three consecutive
  syncs each reported `0 changed` and left the edited field alone. It is
  reverted only once a real `settings` change advances the base, after which the
  org classifies `RemoteEdit` and the pull rewrites the file.

  This supersedes the original design, which wrote the server's response
  wholesale and warned about the top-level keys that differed. Live
  verification killed that: the API's PATCH response is **not** shaped like its
  `GET` — it carries `rir_key`, which `GET /organizations/{id}` omits, and
  returns `users` in a different order. Adopting it wholesale put a field on
  disk that no pull produces, so every settings push was followed by a
  corrective pull, and the divergence notice fired on *every* push naming
  `rir_key, users, workspaces` as "about to be overwritten" when nothing was.
  Scoping the write-back to `settings` fixes both and retires the notice: with
  nothing outside `settings` ever overwritten, there is nothing truthful for it
  to say.

Write-back follows the established pattern: canonical bytes via
`codec("organization").disk_bytes`, then `record_object` with the response's id
and stamps, so the next sync is `Clean`.

### B. Sync wiring

- `push::scan` gains `scan_organization`; `ChangeList` gains
  `organization: Option<PathBuf>` (a singleton, so not a map).
- `sync::mod` inserts `("organization","self")` into `scan_changes` when the
  on-disk hash differs from the lockfile hash. This one addition is what makes
  `LocalEdit` and `BothDiverged` reachable for a kind that could previously only
  be `Clean`, `RemoteEdit` or `RemoteCreate`.
- `change_list_from_classified` gains an `"organization"` arm for `LocalEdit`
  only. `LocalCreate` is impossible (the org always exists remotely) and
  `LocalDelete` is dropped.
- Conflict path: register the org in the conflict-refs builder with
  `HashStrategy::Flat`. Because pull writes the base cache, `try_auto_merge`
  resolves divergence on disjoint keys — local `settings` vs a remote
  `ui_settings` edit — with no prompt. Genuine divergence inside `settings`
  reaches the resolver like any other kind.
- Not registered, deliberately: tombstones, remote-delete dispatch, slug
  collision detection, create-required-field checks.

### C. Offline pre-flight

`limits::check_organization_settings(body) -> Vec<LimitViolation>`, called from
the existing zero-network pre-flight so a bad column fails before a token is
even resolved:

- `schema_id` longer than 50 characters,
- `column_type` outside `{schema, meta}`, `data_type` outside
  `{string, boolean, date, number}`,
- missing required keys for the variant,
- the `{"schema": {…}}` / `{"meta": {…}}` wrapper shape, rejected with a message
  pointing at the flat form — OPTIONS advertises the wrapper and the API rejects
  it, so this is the one trap worth naming explicitly.

Applies to `annotation_list_table` and `request_dashboard_table`.

### D. Migrate promotion

- `enumerate_files` includes `envs/<env>/organization.json` (it sits at the env
  root, outside `MANAGED_DIRS`, so it is added explicitly rather than by
  removing it from `should_skip`); `classify` maps that path to
  `("organization", "self")`.
- `Organization::cross_env_body` retains **only** `settings` and strips
  everything else, so `reconcile_target_identity` restores the target's own
  identity, `name`, `ui_settings`, `metadata` and back-references from the
  target file. Promotion therefore writes the source's `settings` into the
  target's own org object.
- **Skip + warn when the target env has no `organization.json`.** There is
  nothing to reconcile against, and the generic path would otherwise emit a
  settings-only org file. `rdc sync <tgt>` pulls it first.
- Overlay: new flat `[organization]` section — field → value, with no slug layer,
  because there is only ever one org per env:

  ```toml
  # envs/prod/overlay.toml
  [organization.settings.annotation_list_table]
  columns = [
    { visible = true, column_type = "meta", width = 120.0, meta_name = "status" },
  ]
  ```

  Applied after the promotion with the documented precedence (overlay beats the
  promoted value), and merged the same way every other overlay is: objects
  recurse, arrays replace wholesale — so the `columns` list above replaces the
  promoted one rather than appending to it. Excluded from `kind_maps()` and hence
  from the dangling-key check, which has no slugs to validate here.

  The overlay is a **migrate-time** mechanism, as it is for every other kind:
  `apply_overrides` runs only in `migrate`. Push sends what is on disk, so an
  `[organization]` entry shapes the file that migrate writes, and push then
  sends that file's `settings`.
- `--only organization/self` (the org's one slug — `Matcher::parse` requires a
  `<kind>/<slug>` form for every kind, so a bare `--only organization` errors
  the same way a bare `--only hooks` does) works via `DEPLOYABLE_KINDS`;
  conversely `--only hooks/*` excludes the org.
- `mirror_prune_paths` skips the org file — a per-env singleton is never a
  "target-only object".
- Offline reference check: for each promoted `column_type: "schema"` column,
  warn when `schema_id` appears in no `schema.json` under the target env. The
  API accepts unknown ids with a 200, so this warning is the only place the
  mistake can surface.

### E. Idempotency

`width: 120` on disk comes back as `120.0`, and `annotation_list_table: {}`
comes back as `columns: []`. Both are server normalizations, both land on disk
through the write-back, and the sync after that is `Clean`. rdc does not fight
them; the on-disk file converges to the server's form after one push.

## Backward compatibility

- No lockfile schema change — the `organization`/`self` entry, its `id`, stamps
  and `content_hash` already exist.
- No `rdc.toml` change, no new flag.
- `overlay.toml` gains an optional section that an older rdc ignores (no
  `deny_unknown_fields`). A project can be shared between rdc versions.
- Existing snapshots are unchanged on disk. The first sync after upgrading is
  `Clean` for the org unless the local file already diverges from its recorded
  base, in which case it is pushed — the accepted risk of shipping this on by
  default. `sync --dry-run` shows it without writing.
- `migrate` starts producing `organization.json` in the target; a project that
  does not want that deletes the target's file (it is then skipped) or pins the
  values with an `[organization]` overlay.

## Testing

Unit:

- `cross_env_body` retains only `settings`; `create_body` stays a no-op.
- Each pre-flight rule, including the wrapper-shape rejection and a `schema_id`
  of exactly 50 vs 51 characters.
- `[organization]` overlay parse, apply, and exclusion from the dangling-key
  check.

Integration against the mock server:

- A local `settings` edit produces exactly one `PATCH /organizations/{id}` whose
  body is `{"settings": …}` and nothing else.
- An edit confined to an unmanaged field warns and sends no request.
- A remote-only `settings` change pulls and does not PATCH.
- Divergence on disjoint keys auto-merges without a prompt.
- A deleted `organization.json` never produces a DELETE, on any code path.
- A local file with no `settings` key warns and sends nothing.

Migrate, offline:

- Promotion writes the source's `settings` into the target file while preserving
  the target's `id`, `url`, `name`, `ui_settings` and `metadata`.
- An `[organization]` overlay entry beats the promoted value.
- A target without `organization.json` is skipped with a warning.
- The missing-`schema_id` warning fires for a schema column absent from the
  target's schemas, and not for a meta column.
- `--mirror` never prunes `organization.json`.

Live (opt-in harness): the round-trip already performed by hand — PATCH the
columns, GET to confirm, restore.

## Failure modes

- **A stale snapshot pushes old columns.** Mitigated by the base-hash
  classifier: a local file equal to its base is never pushed, and divergence on
  both sides goes through the 3-way merge or the resolver rather than
  overwriting.
- **A `settings` PATCH drops a key rdc has never seen** (a future API addition
  the local snapshot predates). The snapshot is refreshed by pull before push in
  the same sync, so the window is a remote change made between the listing and
  the PATCH. Same exposure as every other kind's `settings`.
- **A promoted column references a field the target lacks.** Warned, not
  dropped; renders as an empty column until fixed.

## Out of scope

- `ui_settings` and `metadata` on the organization.
- Creating or deleting organizations.
- Validating `schema_id` against the *remote* env at push time (the API does not,
  and the target-snapshot check in migrate covers the promotion case).
- Any change to how queue-level `annotation_list_table` works — it is already
  pulled, pushed and promoted.
