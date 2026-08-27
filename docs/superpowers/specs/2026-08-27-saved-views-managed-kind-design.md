# Saved views — a managed kind

## Problem

A Rossum *saved view* (`/v1/saved_views`) is a stored annotation-dashboard
filter: a `name`, a `query` in the search-annotations format, a `shared` flag,
and an optional `queues_filter`. Teams build a set of shared views — "awaiting
approval", "blocked on vendor", "exported today" — agree on them, and then have
no way to version them or move them between environments. They are rebuilt by
hand in every env, drift silently, and are lost when someone deletes one.

Every other configuration surface a team maintains is already an rdc kind.
Saved views should be one too: pulled into the snapshot, pushed on edit,
promoted `dev` → `test` → `prod` by `rdc migrate`.

The complication is ownership. Every kind rdc manages today is org-owned or
queue-owned. A saved view is **user-owned** — it has a read-only `created_by`,
and a `shared` flag that decides whether anyone else can see it. That property
drives most of the decisions below.

## Verified facts

### Rossum API (probed against a sandbox organization on `api.elis.rossum.ai`, 2026-08-27)

| Probe | Result |
| --- | --- |
| `GET /v1/saved_views` | **200**. Standard envelope: `pagination.{next, previous, total, total_pages}` + `results` |
| `?page_size=100&ordering=id&page=1` | **200** — rdc's `list_paginated` works on its NORMAL path, not the `total_pages == 0` fallback |
| `?ordering=-id` | genuinely reverses. `?ordering=zzz` → 200, silently ignored |
| `?shared=true`, `?shared=false`, `?created_by=<anything>` | **200 but SILENTLY IGNORED** — each returned the full list, including rows contradicting the filter |
| `OPTIONS /v1/saved_views` | `name` required, `max_length` **255**; `query` required, no declared cap; `shared` + `queues_filter` writable; `id`, `url`, `organization`, `created_by`, `created_at`, `modified_by`, `modified_at` all `read_only` |
| `POST` sending `created_by` and `organization` overrides | **201, both silently ignored** — the server substitutes the authenticated user and the real org |
| two views POSTed with an identical `name` | both created — **`name` is NOT unique** |
| `PATCH` echoing every read-only field back | **200**, ignored (no 400) |
| `DELETE` | **204**, then `GET` → **404**. Hard delete, no async task (unlike queues) |
| `query` with a bogus queue URL | **400** `Invalid hyperlink - Object does not exist.` |
| `query` with a bare integer queue id | **400** `Incorrect type. Expected URL string, received int.` |
| `query` with a stringified integer queue id | **400** `Invalid hyperlink - No URL match.` |
| `query` with an unknown filter key | **400** `At least one definition of a field required.` |
| `query` = `{"$and": []}` (empty `$and`) | **400** `{"query":{"$and":["This list may not be empty."]}}` — an empty `$and` is NOT a valid "match everything" query (found by the live scenario, 2026-08-27; the spike had only ever probed non-empty `$and`) |
| `query` with `labels: [bogus label URL]` | **400** hyperlink error — label refs ARE validated |
| `query` with `modifier: [real active org user URL]` | **201** — user refs are valid and hyperlink-validated |
| `query` with `field.<nonexistent_schema_id>.string` | **201 — NOT validated.** Field-id keys are unchecked |
| `queues_filter: [bogus queue URL]` | **400** `Invalid hyperlink - Object does not exist.` |

Two consequences worth stating plainly:

- **Object references inside `query` must be full URLs.** Ints and stringified
  ints are both rejected, so whatever the Rossum UI writes must use URLs too.
  rdc's existing URL walker is therefore the correct and complete mechanism for
  `query` portability.
- **`query` is validated, `field.<schema_id>` keys are not.** A mis-portabilized
  queue/label/user ref fails LOUDLY with a 400. A per-env renamed schema field
  is accepted silently and yields a quietly broken view.

### Visibility and permissions (probed with a second identity — a throwaway user created, logged in via `POST /v1/auth/login`, then deleted)

| Viewer | Own private | Other's private | Shared `qf=[]` | Shared `qf=[their queue]` | Shared `qf=[other queue]` |
| --- | --- | --- | --- | --- | --- |
| `organization_group_admin`, **0 queues assigned** | yes | **yes** | yes | yes | yes |
| `admin` (plain), **0 queues assigned** | yes | **yes** | yes | yes | yes |
| `annotator`, 1 queue assigned | yes | not tested | yes | yes | **no** |

Writes follow reads: the plain `admin` successfully `PATCH`ed another user's
**shared** view (200) *and* another user's **private** view (200).

So **any org admin has full read and write access to every saved view in the
organization, private ones included, regardless of queue assignment.** The
queue-coverage restriction in the public docs binds non-admins only. rdc already
requires an admin token for everything else it does, so a saved-view snapshot is
**token-independent** — the same property every existing kind has.

The public documentation is wrong on two points that were checked: it describes
pagination as "opaque signed cursors" (this deployment returns `total_pages`),
and it lists `organization` as an optional POST field (OPTIONS says read-only,
and an override is ignored).

### rdc code

- `refs.rs::walk_strings_mut` visits every string leaf at any depth **but not
  object keys**. So refs nested arbitrarily deep inside `query` portabilize for
  free, while `field.<schema_id>` keys can never be rewritten.
- `is_portable_kind` excludes only `organization`, `mdh_indexes`, `mdh_data`.
  Non-snapshotted targets — users, hook templates — never resolve through the
  lockfile and are left verbatim.
- `UNIVERSAL_SERVER_FIELDS` (`snapshot/create.rs`) already contains
  `created_by` and `created_at`, so they are stripped from POST and PATCH
  bodies — but **not** from the on-disk JSON.
- `HIDDEN_FIELDS` (`snapshot/key_order.rs`) is exactly
  `["modified_at", "modified_by"]`, applied at the top level only. Widening it
  would change on-disk bytes for every kind that carries the added field.
- A clean `RemoteDelete` auto-resolves through `delete_local_object`
  (`sync/execute.rs`), which ends in `drop_lockfile_entry` — the local file and
  the lockfile entry are removed together, so no tombstone can follow and no
  remote delete is triggered.
- `<env>.lock.json` **is committed**; `.rdc/state/*.base` and `.rdc/conflicts`
  are gitignored (`cli/init.rs` `PATTERNS`).
- The generated CI deploy job runs `rdc sync --allow-deletes --yes` unattended.
- `migrate::strip_source_host_env_refs` is deliberately scoped to env fields:
  *"deployable content … is never touched here — an unresolvable source-host ref
  there is a source data bug the user must fix, not something migrate may
  silently rewrite."* `query` is deployable content, so migrate will not rewrite
  it, and a residual foreign ref reaches the wire as a 400.
- `snapshot/limits.rs`'s `every_pushable_kind_has_limits` test fails the build
  unless a new pushable kind has a `field_limits` entry.
- `GenericMapping::KINDS` is `[&'static str; 10]`; `realign::OVERLAY_KINDS` is
  `[&str; 9]`. Both are fixed-size arrays — extending them is a compile-time
  change.
- A flat push-capable kind is registered in roughly 15 places: `ChangeList`
  (plus `total`, `json_parse_errors`, `field_limit_violations`), `Tombstones`
  (plus `total`), `push::scan::scan`, `detect_tombstones`, `push::mod` dispatch,
  `push::deletes` (counts, tally, dispatch, delete call), `sync::mod`'s
  remote-hash / scan-change / tombstone / locked blocks, and four `match it.kind`
  sites in `sync::execute`.

## Decisions

1. **Scope: shared views only.** rdc manages `shared == true` and nothing else.
2. **Full lifecycle**: pull, push, delete, and promote across envs.
3. **Filter at listing time**, not at push time — private views are invisible to
   rdc rather than snapshotted-but-skipped.
4. **Refuse, never silently rewrite**, when a `query` or `queues_filter`
   cannot cross an env.
5. **Saved views opt out of deferred relink.** An unresolvable ref is a hard
   error, not a deferred PATCH.
6. **No `rdc.toml` gating.** Adding the kind changes what every existing project
   pulls on its next sync; that is accepted.
7. **No `LOCKFILE_VERSION` bump. No new CLI flag.**

## Design

### A. Identity and layout

- Kind string: `saved_views` — the lockfile, mapping and overlay key, matching
  the `email_templates` underscore convention.
- Directory: `envs/<env>/saved-views/<slug>.json` — hyphenated, matching
  `email-templates`.
- Slug: `slugify(name)` with `-2`, `-3` … collision suffixes, pinned to the
  object id through the existing `lockfile.slug_for_id`. Duplicate names are not
  a corner case here: `name` is not unique and per-user namespaces make
  collisions routine.
- **Flat, not queue-nested.** `queues_filter` is a 0..n list, so there is no
  single owning queue. `rules` is the precedent — also a `queues` list, also a
  flat top-level directory. The precedent is about layout only — `rules` carry a
  `.py` sidecar and saved views carry none.

### B. The shared-only filter

`pull::saved_views::list` calls `GET /saved_views` and then filters
`shared == true` **client-side**, because the server's `?shared=` parameter is
silently ignored. Private views never enter `RemoteCatalog`, so they are never
snapshotted, never pushed, and never deleted.

This is a safety boundary, not a convenience:

- Without it, every user's personal dashboard filters land in the customer's git
  repository — and `query` holds filter *values*, which is customer business
  data (vendor names, amounts, document numbers). Everything rdc snapshots today
  is configuration; MDH row data is opt-in per dataset.
- `created_by` is read-only, so a private view that rdc ever deleted and
  re-pushed would come back owned by the pushing token's user and be invisible to
  the person who created it. Unrecoverable through the API.

Because an admin token sees everything (verified above), the filter is the *only*
thing keeping private views out. It is not a workaround for restricted
visibility.

### C. On-disk shape

Model (`src/model/saved_view.rs`), following the `Label`/`Workflow` pattern with
`#[serde(flatten)] extra` for forward compatibility:

```rust
pub struct SavedView {
    pub id: u64,                     // null_as_default
    pub url: String,                 // null_as_default
    pub name: String,
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    pub queues_filter: Vec<String>,
    pub query: serde_json::Value,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}
```

Codec `disk_bytes` applies `strip_hidden_fields` (`modified_at`, `modified_by`)
and additionally strips **`created_by` and `created_at`**. `created_by` is an
env-specific user URL that never portabilizes — the same leak class as the
already-fixed `rir_url`, `token_owner` and `hook_template` bugs — and
`created_at` is pure noise.

That strip lives in the saved-views codec **only**, not in the global
`HIDDEN_FIELDS`. Widening the global list would rewrite the on-disk bytes of
every other kind that carries those fields and churn every existing project.

Top-level key order: `id`, `url`, `name`, `shared`, `queues_filter`, `query` —
the blob last, so diffs of the scalar fields stay readable. No sidecars: `query`
is JSON, not code, and keeping it inline avoids a second hash input.

`organization` stays on disk, exactly as it does for `labels`.

### D. Push contract

- **Create** — `POST /saved_views` with `strip_for_create` applied. The server
  assigns `created_by` to the pushing token's user. For a shared view the owner
  is cosmetic; for a private view it would be destructive, which is one of the
  reasons Section B excludes them.
- **Update** — `PATCH /saved_views/{id}` with `strip_patch_extra`. The server
  tolerates echoed read-only fields, but stripping keeps the contract identical
  to every other kind.
- **Delete** — `DELETE /saved_views/{id}` → 204, through the standard tombstone
  path, gated by `--allow-deletes`.
- `limits.rs` gains `"saved_views" => &[("name", 255)]`, which also satisfies
  `every_pushable_kind_has_limits`.

### E. Offline pre-flight

One new check, in the `ChangeList` validation slot that already refuses a push
before the first remote write (alongside `json_parse_errors` and
`field_limit_violations`), so a permanent failure can never wedge a project
mid-sync:

**`shared: false` in a local file is refused.** Pushing it would create a view
that rdc immediately filters back out, producing a create-then-vanish loop the
user cannot diagnose. The message states that rdc manages shared views only.

`name` length is covered by the generic `field_limit_violations` sweep once the
`limits.rs` entry above exists — no saved-view-specific code.

### F. Migrate promotion

`queues_filter` and `query`'s queue and label refs portabilize and resolve
automatically — they are URLs, and `walk_strings_mut` reaches them at any depth.
`cross_env_body` needs no kind-specific additions: `created_by` and `created_at`
are already universal, and `organization` is already dropped by
`strip_for_cross_env_patch`.

**New validation, two halves, both offline and both hard errors.**

*Non-portable refs.* After portabilization, any string in `query` still matching
`https://…/api/v1/…` is non-portable — a user ref (`modifier`, `creator`,
`assignees`), or a foreign host. The error names the view slug, the JSON path
within `query`, and the offending ref.

*Unresolvable portable refs.* Every `rdc://queues/<slug>` and
`rdc://labels/<slug>` in `queues_filter` and `query` must exist in the **target
snapshot**. Migrate is a snapshot-to-snapshot transform, so this is checked
against the target tree with no network.

The second half exists because saved views must **not** participate in the
deferred-relink mechanism (`resolve_value_deferring`), which removes a top-level
field still holding `rdc://` refs and PATCHes it after the referent is created.
For this kind that mechanism is unsafe in two distinct ways:

- **`queues_filter` dropped to `[]` silently widens visibility.** By the API's
  own semantics a shared view with an empty `queues_filter` is visible to the
  *entire organization*. A view scoped to two queues would quietly become
  org-wide — a privacy change dressed up as a deferral.
- **`query` is required on POST.** Deferring it makes the create fail with
  `400 This field is required`; on a PATCH it degrades to a silent no-op that
  leaves the target's old query in place.

So the saved-views driver resolves refs strictly: everything resolves, or the
run stops with a message naming what is missing.

The alternative — stripping the clause — is rejected on purpose. Dropping a
`modifier` filter turns "modified by one person" into "everything", a meaning
change disguised as a cleanup, and `strip_source_host_env_refs` already
establishes that migrate does not rewrite deployable content.

Escape hatch: `[saved_views."<slug>"] query = { … }` in the target env's
`overlay.toml`. Overlay object overrides replace wholesale, so this substitutes a
target-appropriate query outright. When the overlay supplies `query` for a view,
the check is satisfied for that view.

### G. Unshare semantics

A view flipped shared → private in the UI drops out of rdc's filtered listing.
The existing classifier sees it as a clean `RemoteDelete` and auto-resolves —
`delete_local_object` removes the file and the lockfile entry together, so no
tombstone follows and the remote view is untouched. Re-sharing later reappears as
a `RemoteCreate` and the file returns. The state is convergent, and the deletion
is recoverable from git because the lockfile is committed.

The only change required is the message: *"no longer shared — no longer managed
by rdc"*, not *"deleted remotely"*, which would be a lie.

Warn-and-keep was considered and rejected: it leaves a permanently dirty object
that warns on every sync forever, with no action that clears it.

### H. Registry deltas

| Registry | Change |
| --- | --- |
| `model/mod.rs` | `pub mod saved_view;` + re-export |
| `api/mod.rs` | `list_saved_views`, `create_saved_view`, `update_saved_view`, `delete_saved_view` |
| `snapshot/codec/mod.rs` | `mod saved_views;` + `"saved_views" => …` arm |
| `paths.rs` | `saved_views_dir()` |
| `pull/common.rs` | `RemoteCatalog` field, `Listed` variant, `Kind` variant, `kinds[]` entry, dispatch arm |
| `pull/portabilize.rs` | `locate_json_path` arm |
| `push/scan.rs` | `ChangeList` + `Tombstones` fields, `total`, `json_parse_errors`, `field_limit_violations`, `scan`, `detect_tombstones` |
| `push/deletes.rs` | counts field, tally, dispatch, delete call |
| `push/mod.rs` | driver dispatch |
| `sync/mod.rs` | remote-hash, scan-change, tombstone, locked blocks |
| `sync/execute.rs` | slug derivation, remote-bytes lookup, tombstone arm, change-list arm, pull-subset dispatch |
| `snapshot/limits.rs` | `field_limits` arm |
| `mapping.rs` | `Mapping` + `GenericMapping` fields, `Default`, `KINDS` 10→11, `kind_rows`/`kind_rows_mut`, `kind_map` |
| `overlay.rs` | section, accessor, `kind_maps()` return array 9→10 (fixed-size, compile-time) |
| `migrate/mod.rs` | `classify`, `MANAGED_DIRS`, overlay dispatch, `remap_relative` |
| `deploy/selection.rs` | `DEPLOYABLE_KINDS`, after `queues` |
| `deploy/realign.rs` | `PendingRename::SavedView`, `OVERLAY_KINDS` 9→10 |
| `cli/index.rs` | `RICH_KINDS` + emitter |

Deliberately **not** changed: `SUBST_KINDS` — nothing references a saved view, so
it is never a substitution target, the same reasoning that omits `engine_fields`
and `email_templates`.

## Backward compatibility

**New binary, existing project.** The first sync pulls every shared view into a
new `saved-views/` tree. Purely additive: a delete requires a pre-existing
lockfile entry, and there are none, so nothing can be removed. In CI the archive
job commits the tree once and then reaches steady state. Accepted — see
Decision 6.

**Old binary, new-format project.** An unknown directory is ignored by
`push::scan` and by migrate's `MANAGED_DIRS`; an unknown lockfile kind is ignored
by `detect_tombstones`. So an older rdc deletes nothing locally or remotely. It
does silently omit saved views from a `migrate` target. Downgrade is lossy, not
destructive.

**State files.** No `LOCKFILE_VERSION` bump — `objects` is keyed by kind string.
Existing `mapping.toml` and `overlay.toml` files parse unchanged because every
field is `#[serde(default)]`; neither struct uses `deny_unknown_fields`, so a
newer file also loads on an older binary and the section is ignored.

## Testing

Unit:

- Model round-trip preserves unknown fields; `null` id/url tolerated.
- Codec strips `modified_at`, `modified_by`, `created_by`, `created_at`; emits no
  sidecars; path is `saved-views/<slug>.json`.
- Global `HIDDEN_FIELDS` is unchanged, so other kinds keep `created_by` on disk.
- Slug collision: two views with the same name produce `<slug>` and `<slug>-2`.
- `field_limits("saved_views")` is non-empty; a 256-char name is refused offline.
- Pre-flight refuses `shared: false`.
- Portabilization rewrites a queue URL nested inside `query.$and[i].queue.$in[j]`
  and restores it on resolve; a `field.<schema_id>` key is left untouched.
- Migrate hard-errors on a residual user URL inside `query`, and does not when
  the target overlay supplies `query`.
- Migrate hard-errors when `queues_filter` names a queue slug absent from the
  target snapshot, and the error names that slug.
- A saved view whose `query` holds an unresolvable `rdc://` ref is never
  deferred: the push driver errors instead of sending a body without `query`.

Cross-kind: add `sample_saved_view()` to `tests/codec_invariant.rs`.

Integration (`wiremock`): pull writes the tree and lockfile; a private view in
the mocked listing is ignored; an unshared view is reported with the
unmanaged wording rather than a remote-delete wording.

Live (`tests/live/scenarios/`, opt-in): round-trip a shared view — create,
pull, edit, push, delete — and confirm a private view created alongside it is
absent from the snapshot. Cleanup must delete every object it creates.

## Failure modes

| Situation | Behavior |
| --- | --- |
| Token lacks permission for `/saved_views` | `skip_on_permission_denied` logs a skip and yields an empty list; the rest of the sync proceeds |
| Local file has `shared: false` | Offline refusal before any network write |
| `name` longer than 255 | Offline refusal |
| `query` carries a user ref, cross-env migrate | Hard error naming view, path and ref; overlay `query` override is the escape hatch |
| `query` carries a user ref, within-env push | Succeeds — a raw user URL is valid in its own env |
| `queues_filter` names a queue missing from the migrate target | Hard error naming the slug. Never deferred — an empty `queues_filter` would widen the view to the whole org |
| `query` holds an unresolvable `rdc://` ref at push time | Hard error. Never deferred — `query` is required on POST and a deferred PATCH would silently keep a stale query |
| `query` references a renamed schema field | Accepted by the server; view is silently wrong. Documented, not guarded — `walk_strings_mut` cannot reach object keys |
| Shared view unshared in the UI | Local file dropped with unmanaged wording; re-sharing restores it |
| Two views renamed to the same name | Second gets a `-2` slug; ids keep both pinned |

## Out of scope

- Private (`shared: false`) views — never read, written or deleted.
- Restoring or transferring a view's `created_by`. The API has no writable owner.
- Rewriting `field.<schema_id>` keys inside `query` during migrate.
- Any `rdc.toml` opt-in flag for the kind.
- Validating `query` semantics beyond what the server already enforces.
- Participating in the deferred-relink pass. Saved views resolve strictly.
