# Rossum Store Extensions — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-13
**Scope:** Full lifecycle of Rossum store-installed extensions (e.g., Master Data Hub, Email Notifications, Document Splitting) in `rdc pull`, `rdc push`, `rdc deploy`, and `rdc status`. Read paths already work today; this design closes the create gap that currently breaks `rdc deploy` whenever the source environment contains a store extension.

## Goal

Make store extensions behave like every other rdc kind:

- **Pull** brings them back as plain JSON on disk, byte-for-byte round-trippable.
- **Push** creates a new one (via the dedicated install endpoint) when only a local file exists, and updates an existing one when a lockfile entry exists. Same one-line UX as a regular hook.
- **Deploy** bootstraps store extensions on a fresh target environment alongside everything else. Cross-cluster template URLs and per-org system-user URLs are resolved automatically, with a one-time interactive prompt for the system user that bakes the choice into the target overlay.
- **Delete** removes them from the remote when the local file is removed, identical to regular hooks.

The "snapshot is canonical" principle applies: every byte that lives on the remote also lives on disk; rdc never silently elides template-managed prose. The user can grep, diff, and review the full body.

## Non-goals

- Building a UI for browsing the Rossum store catalog or installing templates from inside rdc. Discovery still happens in the Rossum web UI; rdc only manages already-installed extensions and re-installs them across environments.
- Handling `install_action: "request_access"` templates (NetSuite, Workday, certified SAP / Coupa, etc.) in any way other than refusing with an actionable error. These need Rossum sales to enable per-org access; rdc can't unblock that.
- Editing extension code outside of `config.code` (some function-type templates carry code; the existing `<slug>.py` sidecar mechanism handles them with zero changes).
- Modelling the Rossum store catalog itself. Templates are a remote resource queried at deploy time; they are not pulled into the snapshot.
- Diffing template-version drift. If Rossum upgrades a template upstream (e.g., new `guide` HTML), the next pull surfaces that as a regular content diff. There is no "template version" concept on disk.

## Background

A *store extension* is a hook installed via the Rossum store. Empirically verified on `elis.rossum.ai`:

| Field on the installed hook | Source |
|---|---|
| `extension_source: "rossum_store"` | Server, on install. Marker. |
| `hook_template: "<url>"` | Server, on install. Back-pointer to the template. |
| `description`, `guide`, `read_more_url`, `extension_image_url` | Copied from the template. |
| `config`, `settings`, `settings_schema`, `secrets_schema`, `sideload`, `token_lifetime_s`, `events` | Copied from the template. |
| `name`, `queues`, `active`, `run_after`, `metadata`, `token_owner` | Set from the install request. |
| `id`, `url`, `created_at`, `modified_at`, … | Universal server-managed fields, already stripped. |

Templates declare an `install_action`:

- `copy` (28 of 34 templates on elis) — anyone in the org can install. The store endpoint copies the template body into a new hook.
- `request_access` (6 templates) — pre-sale gating. The store UI shows "contact us" instead of an install button. The `/hooks/create` endpoint refuses without prior enablement.

The install endpoint is `POST /api/v1/hooks/create` with a minimal payload:

```json
{
  "name": "Master Data Hub",
  "hook_template": "https://elis.rossum.ai/api/v1/hook_templates/39",
  "events": ["annotation_content.initialize", "annotation_content.started", "annotation_content.updated"],
  "queues": [],
  "token_owner": "https://elis.rossum.ai/api/v1/users/938493"
}
```

Standard `POST /hooks/` *cannot* recreate one: store webhooks have `config: { private: true, app: null, … }` with no `config.url`, and `POST /hooks/` rejects with `400 {"config":{"url":["This field is required."]}}`. Both update (`PATCH /hooks/<id>`) and delete (`DELETE /hooks/<id>`) work on store extensions identically to regular hooks — the gap is only on create.

## The model

### Detection

A hook is a store extension iff its JSON has `extension_source == "rossum_store"`. The `hook_template` URL must be present whenever that flag is set; rdc refuses to push a hook with one without the other (anomaly guard, not expected in practice).

### Hook struct

`extension_source` and `hook_template` are always present on every hook in the API response — regular hooks have `extension_source: "custom"` and `hook_template: null`. To preserve byte-exact round-trip (the snapshot's "every field round-tripped" principle), the fields stay in the existing `extra: BTreeMap<String, Value>`. Detection is done through accessor methods, mirroring the existing `modified_at()` accessor pattern:

```rust
impl Hook {
    pub fn extension_source(&self) -> Option<&str> {
        self.extra.get("extension_source").and_then(|v| v.as_str())
    }
    pub fn hook_template(&self) -> Option<&str> {
        self.extra.get("hook_template").and_then(|v| v.as_str())
    }
    pub fn is_store_extension(&self) -> bool {
        self.extension_source() == Some("rossum_store")
    }
}
```

This keeps `null` vs `"custom"` vs `"rossum_store"` distinct on disk without any serde gymnastics, and the detection collapses to one boolean for callers.

### Snapshot

Store extensions live under `envs/<env>/hooks/<slug>.json` like every other hook. The full server body is round-tripped (no stripping): `description`, `guide`, `extension_image_url`, `settings_schema`, `secrets_schema`, `sideload`, `token_lifetime_s`, `config`, `settings`, `hook_template`, `extension_source` all land on disk. The existing universal strip (`id`, `url`, `created_at`, `created_by`, `modified_at`, `modified_by`, `status`) still runs.

Function-type store templates (Address Prefilling, SAP ECC IDoc Integration, …) carry `config.code`. The existing `<slug>.py` sidecar mechanism in `src/snapshot/hook.rs` handles them with zero changes — `serialize_hook` extracts code, `read_hook_value` splices it back. Webhook-type templates have no code → no sidecar.

Example on disk for `hooks/master-data-hub.json`:

```json
{
  "name": "Master Data Hub",
  "type": "webhook",
  "description": "Enhance the extracted data with details from your master records.",
  "settings": {
    "configurations": [
      {
        "name": "Default configuration",
        "source": { "dataset": "vendors", "queries": [...] },
        "mapping": { "dataset_key": "Vendor ID", ... },
        "result_actions": { ... }
      }
    ]
  },
  "active": true,
  "events": ["annotation_content.initialize", "annotation_content.started", "annotation_content.updated"],
  "queues": [],
  "run_after": [],
  "metadata": {},
  "config": { "private": true, "timeout_s": 60, ... },
  "sideload": ["schemas"],
  "settings_schema": null,
  "secrets_schema": { "type": "object", ... },
  "token_owner": "https://elis.rossum.ai/api/v1/users/938493",
  "extension_source": "rossum_store",
  "hook_template": "https://elis.rossum.ai/api/v1/hook_templates/39",
  "guide": "<div>...</div>",
  "read_more_url": "https://elis.rossum.ai/svc/master-data-hub/api/docs",
  "extension_image_url": "https://.../Master-Data-Hub.png",
  "token_lifetime_s": 7200
}
```

### Lockfile

No new section. Store extensions are stored under `"hooks"` exactly like regular hooks, keyed by slug. The combined hash (`hook_combined_hash`) covers JSON + optional `.py` and naturally handles the "no code" case.

## Push (same-env)

`rdc push <env>` keeps its existing four-case scan-then-act loop. Only one branch changes:

| Hook in snapshot | Lockfile entry? | Behavior |
|---|---|---|
| Regular | absent | `POST /hooks/` (unchanged) |
| Regular | present | `PATCH /hooks/<id>` (unchanged) |
| Store extension | absent | **Two-call create**: `POST /hooks/create` with `{name, hook_template, events, queues, token_owner}` extracted from the snapshot, then `PATCH /hooks/<id>` with the divergence between the server's just-installed body and the local snapshot (customer's `settings`, `active`, `run_after`, `metadata`, any `config` tweaks beyond template defaults). |
| Store extension | present | `PATCH /hooks/<id>` (unchanged — same as test E confirmed, server accepts the full body with `extension_source`/`hook_template`) |

### Two-call create

Conceptually one logical create. Implementation:

0. **Orphan check**: before step 1, list `/hooks` on the remote once per push (cached for the loop). If a hook with the same `name` and `hook_template` already exists and the local lockfile has no entry for it, treat that as an orphan from a prior step-2 failure: skip the install, adopt the existing id, jump to step 2 against it.
1. **Install**: build the minimal install body from the snapshot — `{name, hook_template, events, queues, token_owner}`. Call `POST /hooks/create`. On 201, the server returns the new hook with all template-copied fields populated.
2. **Reconcile (PATCH)**: build the full update body the same way `rdc push` builds its existing PATCH bodies — read the snapshot, apply overlay, serialize the typed `Hook`. If the resulting body's canonical form differs from the server's post-install response (using the existing `bytes_equal_after_strip` + code comparison), issue one `PATCH /hooks/<id>` with the full body. Server treats PATCH as merge; the call is a no-op when the install already matches the snapshot exactly (the common case for a hook that was never customized).
3. **Write back to disk + lockfile**: overwrite the local JSON with the post-PATCH canonical form, record the combined hash. Identical to the current single-call path.

The lockfile **is only written when steps 1-3 all succeed**. A failure between steps 1 and 3 leaves the remote with a partially-configured hook; the next `rdc push` runs step 0 (orphan check), finds it, and resumes. This makes the two-call atomic from the user's point of view: retries converge, no manual cleanup.

### Status, diff, dry-run

`rdc status` already detects "local exists, no lockfile entry" as a pending create. The output gains one extra line per pending store-extension create to make the special path visible:

```
edits:    2 file(s) to create:
          hooks/master-data-hub        (store extension; will POST /hooks/create then PATCH)
          hooks/email-notifications    (store extension; will POST /hooks/create then PATCH)
```

`rdc push --dry-run` lists the same. `rdc push --dry-run --diff` prints the would-be install body for step 1 (the minimal install payload, before the template fills it in) and the would-be PATCH body for step 2 (the divergence). Existing path is untouched.

## Deploy (cross-env)

`rdc deploy <src> <tgt>` already runs `create` (bootstrap missing kinds) then `update` (PATCH sweep) then optional `delete` (mirror mode). The bootstrap is where store extensions need new behavior; update and delete pass through unchanged.

### URL rewriting — two new relations

Today's URL rewriter rewrites by slug-to-slug map: a src queue URL is mapped to the tgt queue URL of the same slug. Store extensions add two URL fields that are not slug-mapped:

- **`hook_template`** → resolved by `(name, type, extension_source)` match against `GET /hook_templates` on the tgt cluster.
- **`token_owner`** → resolved per-hook from `envs/<tgt>/overlay.toml`, populated interactively on first deploy.

Both are looked up once at the start of the deploy and cached. The cache for templates is also persisted to `.rdc/map/<src>-to-<tgt>.toml` under a new `[hook_templates]` section, so subsequent deploys don't re-list:

```toml
# .rdc/map/test-to-prod.toml
[hooks]
master-data-hub = "master-data-hub"

[hook_templates]
"https://test.rossum.app/api/v1/hook_templates/39" = "https://prod.rossum.app/api/v1/hook_templates/41"
```

This section can be hand-edited the same way `[hooks]` already can.

### Template resolution

On deploy start, rdc lists `GET /hook_templates` on the tgt cluster once. It builds an in-memory index keyed by `(name, type, extension_source)`. For each store extension being bootstrapped:

1. Read the src snapshot's `hook_template` URL.
2. Look it up in the *src* lockfile or fetch the src template (small, cached) to read its `(name, type, extension_source)`.
3. Find the matching tgt template by tuple.
4. Write the resolved URL pair to the in-memory rewriter and to the persistent map cache.
5. Use the tgt URL in the install body.

Failure modes:

- **No match** → `template 'Master Data Hub' is not available on prod. Templates with install_action=request_access require Rossum sales to enable; copy templates may have been withdrawn. Install manually via the UI on prod, then re-run rdc pull prod.`
- **Multiple matches** → `ambiguous templates for 'Master Data Hub' on prod (ids 39, 41); add a mapping under [hook_templates] in .rdc/map/test-to-prod.toml.`

### `token_owner` resolution

`token_owner` is per-env: the URL points to a service-account user that lives in the target org. The src snapshot's `token_owner` is always wrong on tgt because it references the wrong org's user. The overlay is the right place — it's already how rdc handles per-env divergences like display names and runtime versions.

The overlay key is plain `token_owner` under the per-hook section:

```toml
# envs/prod/overlay.toml
[hooks.master-data-hub]
"token_owner" = "https://prod.rossum.app/api/v1/users/<system-user-id>"

[hooks.email-notifications]
"token_owner" = "https://prod.rossum.app/api/v1/users/<system-user-id>"
```

**First-deploy interaction.** When `rdc deploy test prod` encounters a store extension to bootstrap and finds no `token_owner` set in `envs/prod/overlay.toml` for that hook (and no `[defaults] store_extension_token_owner` either — see below), the planner pauses, lists candidate users on tgt, and prompts:

```
Pick the token_owner for store extension 'master-data-hub' on prod
(used as the API service account for the extension's calls; usually a system user):

  [1] system_user__a556534d   admin, active
      https://prod.rossum.app/api/v1/users/521884
  [2] svc-mdh-prod   admin
      https://prod.rossum.app/api/v1/users/621119
  [3] martin.zlamal@rossum.ai   you, admin
      https://prod.rossum.app/api/v1/users/200001
  [a] abort the deploy

[1] > 1

Apply this choice to all remaining store extensions in this deploy? [y/N] y
✓ saved as [defaults] store_extension_token_owner in envs/prod/overlay.toml
```

The list ranks `system_user__*` candidates first, then other users in the admin group, with the active session's own user clearly tagged. The default is `[1]` (the first option), reinforcing the convention that the org's system user is the right answer.

The follow-up "apply to all" question is asked once per deploy:

- `y` → rdc writes `[defaults] store_extension_token_owner = "<chosen-url>"` and stops asking for the rest of this deploy.
- `n` → rdc writes `[hooks.<slug>] token_owner = "<chosen-url>"` (per-hook), and re-prompts on the next store extension.

Per-hook overlay keys always win over `[defaults]`. A user who picked the default once but later wants a different owner for one specific hook can add the per-hook key by hand.

**Non-TTY deploys.** CI / piped runs without an existing overlay entry refuse the deploy with an actionable message:

```
error: deploy needs token_owner for store extension 'master-data-hub' on prod, but envs/prod/overlay.toml has no [hooks.master-data-hub] token_owner and no [defaults] store_extension_token_owner.
Run 'rdc deploy test prod' on a TTY once to pick interactively, or edit the overlay directly. Aborting before any remote writes.
```

The abort happens during plan, before any `POST` / `PATCH` is issued. Matches the existing "fail fast in plan, never half-apply" pattern.

### Bootstrap create flow

`src/cli/deploy/create.rs::create_hook` branches:

- **Regular hook**: existing path. POST `/hooks/` with the rewritten body.
- **Store extension**: build the minimal install body (`{name, hook_template, events, queues, token_owner}`) with the resolved tgt template URL and tgt overlay `token_owner`. POST `/hooks/create`. Then compute the divergence against the snapshot (with all the cross-env URL rewrites applied) and PATCH it.

Dependency order is unchanged (`workspaces → schemas → queues → inboxes → email_templates → hooks → rules → labels → engines → engine_fields`). The PATCH-after-install step's URL rewriter has the in-memory tgt lockfile of just-created peers available, so `run_after` and `queues` URLs resolve to tgt IDs as they do today.

### Update sweep

Unchanged. The existing PATCH sweep in `src/cli/deploy/apply.rs` already handles store extensions correctly — confirmed by test E that `PATCH /hooks/<id>` accepts the full body including `extension_source` and `hook_template`. The only adjustment is that overlay-applied `token_owner` and rewritten `hook_template` are merged in alongside the existing overlay/URL-rewrite passes.

### Mirror delete

Unchanged. `DELETE /hooks/<id>` works for store extensions (HTTP 204 confirmed by test G). The existing two-confirmation flow applies.

## Overlay surface — exact shape

The overlay schema (`envs/<env>/overlay.toml`) gains:

1. **New optional key inside `[hooks.<slug>]`**: `token_owner = "<user-url>"`. Already implicitly supported by the existing overlay merger (it walks arbitrary keys); the only change is rdc now consumes it on the deploy create path.
2. **New optional `[defaults]` section** with one key: `store_extension_token_owner = "<user-url>"`. Read as a fallback when a store hook has no per-hook `token_owner`.

Per-hook `token_owner` wins over `[defaults]`. Both are stripped on pull (existing `maybe_strip_overlay` already handles per-hook keys; `[defaults]` is not stripped because it doesn't correspond to a snapshot field).

## CLI UX additions

### Plan output during deploy

```
Plan: test → prod
  + create:  4 workspaces, 24 schemas, 24 queues, 27 hooks, 1 rule, 46 labels
             ↳ 2 of the 27 hooks are store extensions (POST /hooks/create + PATCH each):
                 master-data-hub  → template 'Master Data Hub' on prod (id 41)
                 email-notifications → template 'Email Notifications' on prod (id 27)
  ~ update:  field-level deltas

Proceed? [y/N]
```

### Status output during pull/push

`rdc status` already prints per-kind counts; store-extension counts are surfaced when non-zero:

```
$ rdc status test
Env 'test'
  api_base: https://test.rossum.app/api/v1
  org_id:   123456
  token:    present
  auth:     ok
  lockfile: v2, 256 objects across 11 kinds  (2 store extensions)
  edits:    (none)
```

### Progress bar / phase labels

No changes — store extensions tick under the same `hooks` phase as regular hooks. The interactive token_owner prompt suspends the bar via `ProgressBar::suspend(|| …)` exactly like the existing conflict resolver does.

## Implementation map

High-level only; the implementation plan will break this down further.

| File | Change |
|---|---|
| `src/model/hook.rs` | Add `extension_source()`, `hook_template()`, and `is_store_extension()` accessor methods that read from the existing `extra` map (same pattern as the current `modified_at()` accessor). No struct change. |
| `src/api/mod.rs` | New method `create_hook_via_install(body) → Hook` that POSTs `/hooks/create`. Existing `create_hook` (POST `/hooks/`) stays for regular hooks. |
| `src/api/mod.rs` | New method `list_hook_templates() → Vec<HookTemplate>` and `HookTemplate` model. |
| `src/api/mod.rs` | New method `list_users_in_admin_group() → Vec<User>` for the interactive prompt (lightweight `User` model — id, url, username, first_name, last_name, is_active). |
| `src/cli/push/hooks.rs` | New-hook branch dispatches to install-path when `is_store_extension()`; existing PATCH branch unchanged. |
| `src/cli/deploy/create.rs::create_hook` | Same branch as push. |
| `src/cli/deploy/common.rs::rewrite_urls` | Rewrite `hook_template` and `token_owner` URL fields using the new in-memory maps. |
| `src/cli/deploy/mod.rs` (or new `deploy/store_resolver.rs`) | Build the `(name, type, source) → tgt_template_url` index at deploy start. Run the interactive `token_owner` prompt + overlay write. |
| `src/mapping.rs` | New `[hook_templates]` section in the map TOML; (de)serializer additions. |
| `src/overlay.rs` | Optional `[defaults] store_extension_token_owner` lookup. |
| `src/cli/status.rs` | "N store extensions" count on the lockfile summary line. |
| `src/cli/resolve.rs` (or new module) | Interactive token_owner picker, modelled on the existing `resolve_push_drift` UI. |
| `tests/cli_push.rs`, `tests/cli_deploy.rs` | Wiremock-based coverage for the install endpoint, two-call create, template resolution, ambiguous-template error, missing-token_owner error, non-TTY abort. |
| `testdata/fixtures/` | New fixture: a store-extension hook (MDH-shaped), a hook_templates_list response. |

## Failure modes

A consolidated list (mostly already mentioned inline):

| Trigger | Behavior |
|---|---|
| Snapshot has `extension_source: "rossum_store"` and no `hook_template` | Push refuses with `hooks/<slug>.json: marked as store extension but missing hook_template`. |
| Template by `(name, type, source)` not found on tgt | Deploy refuses with template-unavailable error pointing at manual UI install. |
| Multiple matching templates on tgt | Deploy refuses with ambiguous-templates error pointing at `.rdc/map` `[hook_templates]`. |
| Non-TTY deploy with no overlay `token_owner` and no `[defaults]` | Deploy refuses during plan with overlay-missing error. |
| Step 1 (`POST /hooks/create`) succeeds, step 2 (PATCH) fails | Lockfile not written; next push/deploy's orphan-check (step 0) finds the partial hook by `(name, hook_template)`, adopts its id, resumes from step 2. No manual cleanup required. |
| Step 1 (`POST /hooks/create`) fails with 400 (template requires access) | Deploy refuses with request-access error pointing at manual UI install. |
| Hook with `hook_template` URL set but `extension_source` null | Treated as a regular hook (no marker). Snapshot keeps the dangling URL via `extra`. Defensive: a warning prints once in pull. |

## Open questions / future work

- **Pulled template snapshot.** Optional follow-up: cache the latest seen template body for each store extension under `.rdc/template-cache/<id>.json` so we can detect "Rossum upgraded MDH upstream" between pulls and surface it to the user. Out of scope here.
- **Function-type store templates with code.** Address Prefilling, SAP ECC IDoc Integration, Numeric Calculations [DEPRECATED], etc. carry `config.code`. The existing `<slug>.py` sidecar handles them; no design changes needed. Worth adding at least one to the fixture corpus.
- **Secrets.** Some store templates declare `secrets_schema`. Today rdc has no first-class secrets handling for hooks (the encrypted `secrets` blob is server-side). If a store extension's secrets are needed for runtime behavior to match across envs, the user sets them via the Rossum UI after deploy. Documenting this in the README is a follow-up.
- **Default `token_owner` lookup heuristics.** The interactive prompt's "rank `system_user__*` first" is a usability nicety. If org conventions vary, the list always shows everyone in the admin group so the user can pick anyway.
