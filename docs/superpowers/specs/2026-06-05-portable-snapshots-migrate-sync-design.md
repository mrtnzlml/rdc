# Portable snapshots + `migrate`/`sync` — replacing `rdc deploy` — Design

- **Date:** 2026-06-05
- **Status:** Draft, pending review
- **Author:** Martin (with Claude)
- **Scope:** Make the on-disk snapshot environment-portable by storing cross-references as `rdc://<kind>/<slug>` instead of live API URLs — and align the lockfile (§3.6) and the env-pair mapping (§3.7) on the *same* notation, so one reference scheme spans all three surfaces. Then collapse `rdc deploy` into two thinner commands: a pure-local `rdc migrate` and an enhanced `rdc sync` whose push gains dependency-ordered creation + ref resolution. Deletes the URL-rewriting engine (`rewrite_urls`).

## 1. Problem & motivation

`rdc deploy <src> <tgt>` exists for one structural reason: **the snapshot stores environment-specific URLs.** Every cross-reference a pulled object carries — `queue.workspace`, `queue.schema`, `hook.queues`, `rule.actions[].payload.queue`, `engine.training_queues`, … — is a fully-qualified URL into the *source* cluster (`https://src.rossum.app/api/v1/queues/2860392`). You cannot POST that body to a different organization; the target has different ids and (often) a different cluster host. So `deploy` carries a second machine — `rewrite_urls` (`deploy/common.rs`) — that walks every string in every payload and translates `src URL → (kind, src_slug) → (mapping) → tgt_slug → tgt URL` (three lockfile/mapping lookups per reference), driven by a hand-curated `Mapping` and two lockfiles. That machine, plus its create/apply/auto-match scaffolding, is essentially all of `src/cli/deploy/`.

Meanwhile `rdc sync` already pushes local changes to *one* environment. The observation that started this redesign: **`deploy` is `sync` with a URL-translation step bolted on.** If the snapshot were env-portable to begin with, "deploy to tgt" would reduce to "copy the snapshot into tgt's directory, rename a few slugs, apply tgt overlays" (no remote calls) followed by an ordinary "push tgt" — the same push `sync` already does.

The blocker is purely the URL representation. **Make references portable and the second machine disappears.**

### Why slugs are already a stable identity

rdc pins every object's `slug` to its server **id** via `Lockfile::slug_for_id(kind, id)`: a pull keeps the existing slug even when the remote `name` changes, and only derives a new slug (via `slugify_unique`) for ids it has never seen (verified across every pull driver; this is the global id-pinned-slug work landed earlier — `pull::queues::process` et al.). Queue/schema/inbox slugs are globally unique within their kind. So a slug is a durable, rename-proof handle — exactly what a portable reference needs. The `name` field becomes just another deployable value, not an identity.

## 2. Goals / Non-goals

**Goals**
1. Store every **internal** cross-reference on disk as `rdc://<kind>/<slug>` — an environment-agnostic handle — so a snapshot is portable between organizations/clusters by construction. Align the **lockfile** (§3.6, drop the derivable `url`) and the env-pair **mapping** (§3.7, a unified `rdc://` dict) on the same notation, so one reference scheme — `rdc://<kind>/<slug>` = the lockfile coordinate `objects[kind][slug]` — spans snapshots, lockfile, and mapping.
2. Collapse `rdc deploy` into:
   - `rdc migrate <src> <tgt>` — a **pure-local** snapshot→snapshot transform (slug-rename + overlay), zero remote calls.
   - `rdc sync <tgt>` — pull-then-push as today, with the push phase upgraded to **dependency-ordered creation + `rdc://`→URL resolution**, shared by within-env sync and migrated cross-env changes.
3. Delete `rewrite_urls` and the URL↔URL translation path entirely; cross-env reference handling becomes slug→slug (in `migrate`) + slug→URL (in `sync` push).
4. Fix `sync` push's existing dependency-ordering wart (it currently assumes the workspace already exists; see `push/queues.rs`) for free, by reusing `deploy`'s proven create order.
5. Be robust to remote renames (the reference is a slug, not a name or URL).
6. Subsume MDH index propagation into the same migrate(copy)/sync(push) split.
7. One-time, transparent hash re-baseline so existing URL-form snapshots converge on first pull after upgrade.

**Non-goals**
- Changing the wire protocol, the directory layout, or the conflict-resolution UX. (Two state-file *formats* change minimally, both back-compat on load: the lockfile v2→v3 drops the redundant stored `url`, §3.6; the mapping v1→v2 unifies to `rdc://` pairs, §3.7.)
- Changing which fields are server-managed / stripped / redacted (the existing registries are preserved; see the prior identity/drift fixes and the KindCodec design).
- Changing how **external** references (users, `organization`, `hook_template`, `token_owner`, `triggers`) are handled — they keep their current overlay / prompt / cluster-pair-mapping / strip behavior.
- Keeping a `deploy` alias. `deploy` is removed; the binary emits a guiding error pointing at `migrate` + `sync` (explicit user decision).
- Realign (`rdc doctor`) and MDH dataset *data* (rows) — out of scope.

## 3. The portable reference: `rdc://<kind>/<slug>`

### 3.1 Convention

An internal cross-reference is serialized on disk as the URI `rdc://<kind>/<slug>`, where `<kind>` is the lockfile/URL kind token (e.g. `queues`, `schemas`, `workspaces`, `engines`, `inboxes`, `hooks`) and `<slug>` is the target's stable slug.

```jsonc
// workspaces/demo/queues/invoices/queue.json  (env-agnostic on disk)
{
  "name": "Invoices",
  "workspace": "rdc://workspaces/demo",
  "schema":    "rdc://schemas/cost-invoices",
  "engine":    "rdc://engines/mtr-training"
}
```

Why kind-qualified (settled, §11): a slug is unique only *within* a kind (a workspace and a queue can both be `invoices`), and refs appear in unpredictably nested places (`rule.actions[].payload.queue`, `hook.run_after`). Encoding the kind makes a reference **self-describing**: the resolver walks *all* strings and resolves any `rdc://…` with one mechanical lookup — `rdc://<kind>/<slug>` ⇒ `lockfile.objects[kind][slug]` ⇒ `/api/v1/<kind>/<id>`. No per-field schema is needed at resolution time, so no nested ref can be silently missed (the chief bug risk for a change this broad). The `rdc://` scheme also cannot be mistaken for real Rossum data (formula operands, Jinja templates, mime types), which a bare slug could.

### 3.2 Reference taxonomy (grounded inventory)

Every reference-bearing field across the 12 kinds, classified. **Internal** → becomes `rdc://`. **External** → stays a raw per-env URL (points at a kind rdc does not deploy; handled by strip/overlay/prompt/cluster-mapping as today). **Backref** → server-computed reverse list, stripped from outbound bodies.

| kind | field | class | target | card. | handling |
|---|---|---|---|---|---|
| queues | `workspace` | internal | workspaces | one | `rdc://` |
| queues | `schema` | internal | schemas | one | `rdc://` |
| queues | `inbox` | internal | inboxes | one | `rdc://` (omitted when absent) |
| queues | `engine` | internal | engines | one | `rdc://` |
| queues | `hooks`,`webhooks`,`rules`,`users`,`workflows` | backref | — | array | strip on push; see §3.5 |
| queues | `modified_by` | external | user | one | raw URL; stripped on push |
| schemas | `queues` | backref | queues | array | strip on push |
| inboxes | `queues` | internal | queues | array | `rdc://` |
| inboxes | `modified_by` | external | user | one | raw URL; stripped |
| email_templates | `queue` | internal | queues | one | `rdc://` |
| email_templates | `organization` | external | organization | one | raw URL; stripped |
| email_templates | `triggers` | external | trigger | array | stripped (not deployed) |
| email_templates | `modified_by` | external | user | one | raw URL; stripped |
| hooks | `queues` | internal | queues | array | `rdc://` (order-normalized) |
| hooks | `run_after` | internal | hooks | array | `rdc://` (intra-kind; §3.4) |
| hooks | `token_owner` | external | user | one | per-env; overlay/prompt |
| hooks | `hook_template` | external | hook_template | one | cross-cluster; `Mapping.hook_templates` URL-pair |
| hooks | `created_by`,`modified_by` | external | user | one | raw URL; stripped |
| rules | `queues` | internal | queues | array | `rdc://` |
| rules | `actions[].payload.queue` | internal | queues | one | `rdc://` (nested — only reachable by walk-all-strings) |
| rules | `organization` | external | organization | one | raw URL; stripped cross-env |
| rules | `created_by`,`modified_by` | external | user | one | raw URL; stripped |
| labels | `organization` | external | organization | one | raw URL; stripped |
| engines | `training_queues` | internal | queues | array | `rdc://` |
| engines | `organization` | external | organization | one | raw URL; stripped cross-env |
| engine_fields | `engine` | internal | engines | one | `rdc://` |
| workspaces | `queues` | backref | queues | array | strip on push |
| workspaces | `organization` | external | organization | one | raw URL; stripped |
| workspaces | `created_by`,`modified_by` | external | user | one | raw URL; stripped |
| workflows | `steps` | backref | workflow_steps | array | strip on push |
| workflows | `organization` | external | organization | one | raw URL |
| workflow_steps | `workflow` | internal | workflows | one | `rdc://` |
| workflow_steps | `organization` | external | organization | one | raw URL |

**The redesign only changes the `internal` rows.** External rows keep today's behavior unchanged.

### 3.3 Pull: URL → `rdc://` (post-pass, reuses index machinery)

Conversion runs as a **post-pull pass** over the just-written snapshot, *not* inline in `disk_bytes`, because forward references require every target's slug to already be known (a queue may reference a schema whose pull driver ran later). This is exactly how `cli/index.rs` already resolves `url_to_slug` / `urls_to_slugs` against the lockfile today — that machinery is reused/extended:

```
for each string s in each snapshot Value:
    if let Some((kind, slug)) = lockfile.lookup_url(s):     # already exists
        if kind is INTERNAL (deployable):                   # decided by kind, not field
            replace s with f"rdc://{kind}/{slug}"
        # external kinds (users/org/hook_template/triggers): leave the URL untouched
```

Because the decision keys on the **resolved kind** (from `lookup_url`), not on a field path, every internal reference — including deeply nested and array elements — is converted, and external references are left alone, with no per-field schema to maintain. `lookup_url` already returns `(kind, slug)`, so the kind is free.

Unresolvable URLs (target not in the lockfile — e.g. a not-pulled kind) are left as-is and surfaced by `doctor`, never silently dropped.

### 3.4 Push: `rdc://` → URL (dependency-ordered, shared by sync & migrated changes)

The push phase resolves each `rdc://<kind>/<slug>` to the **target** environment's URL via the target lockfile, creating missing objects in dependency order so a referent always exists before its referer is sent. This is `deploy`'s proven create loop, generalized to be the single push path:

```
create order (existing, proven):
  workspaces → schemas → queues → inboxes → email_templates
            → hooks → rules → labels → engines → engine_fields
for each kind, for each local object not yet on tgt:
  body = disk_value                                  # carries rdc:// refs
  resolve every rdc://<k>/<slug> in body via tgt lockfile.url_for_slug(k, slug)
  strip server-managed + backrefs (create_body)      # unchanged registries
  POST → record tgt id/url/slug in tgt lockfile      # so later kinds resolve
existing objects: same resolution, then drift-check + PATCH (apply.rs logic)
```

- **Resolution replaces `rewrite_urls`.** Old: `src URL → src_slug → mapping → tgt_slug → tgt URL` (3 lookups, needs src+tgt lockfiles + mapping). New: `rdc://kind/slug → tgt lockfile url_for_slug` (1 lookup, tgt lockfile only). The slug carried on disk is *already* the target slug after `migrate` renamed it (§4.2), so push does no cross-env mapping at all.
- **Fixes the sync wart:** `sync` push currently has no dependency ordering (it assumes the workspace exists). Adopting this order makes within-env sync correctly create a new workspace before its queues.
- **Intra-kind / cyclic references:** `hook.run_after` points at sibling hooks, and `queue.inbox` ↔ `inbox.queues` form a pair. These use the existing two-phase handling deploy already relies on: create the objects first (refs that can't yet resolve are deferred), then a second pass PATCHes the cross-link (`run_after`, queue↔inbox wiring) once both ends exist. (Confirm deploy's current exact handling during planning; preserve it.)

### 3.5 Backreferences

Server-computed reverse lists (`queue.hooks/webhooks/rules/users/workflows`, `schema.queues`, `workspace.queues`, `workflow.steps`) currently live on disk and feed the hash (order-normalized by `sort_url_arrays`, see the prior identity/drift fixes). They convert to `rdc://` uniformly through the same string-walk (they point at internal kinds), remain **stripped from every outbound body** (`create_body`), and stay order-normalized for hash stability (the existing sort extends from URL arrays to `rdc://` arrays — same predicate, ref-shaped strings). They are derivable from forward refs (as `index.rs` already does), so dropping them from disk is a *future* simplification, explicitly **out of scope** here to keep the change focused.

### 3.6 Lockfile alignment: portable identity, derived URLs

The lockfile (`.rdc/state/<env>.lock.json`, one per env) holds **no cross-object references** — only each object's own `id`, self-`url`, `content_hash`, `modified_at`, `secrets_hash`, keyed by `objects[kind][slug]`. The slug key already *is* the portable identity (`rdc://<kind>/<slug>`); the stored `url` is the lone env-coupled string and is **redundant** — it equals `<env api_base>/<segment(kind)>/<id>`.

To carry the `rdc://` philosophy through the lockfile (no stored env-specific URLs anywhere), **drop the stored `url`** and derive it on demand:

- `url_for_slug(kind, slug)` (push resolution, §3.4) → `format!("{api_base}/{segment(kind)}/{id}")` from the entry's `id`.
- `lookup_url(url)` (pull conversion, §3.3) → parse `…/api/v1/<segment>/<id>`, map `segment→kind`, then `slug_for_id(kind, id)`. External URLs (users, hook_templates) parse to a kind not in the lockfile → `slug_for_id` returns `None` → left untouched, exactly as required.
- `segment(kind)` is identity for every kind except `organization → organizations`.
- The env `api_base` is threaded into these methods from the env config (already available at every call site).

**Verified empirically** (dev-mtr lockfile, 100 entries): 99 satisfy `url == api_base + "/" + kind + "/" + id` exactly, with a single host; the only exception is `organization` (lockfile key singular, URL path `organizations/` plural — absorbed by `segment()`); MDH (`mdh_indexes`) entries carry no `/api/v1/` URL and no `rdc://` refs, so they are unaffected. Derivation is therefore reliable.

This is a lockfile **format change, v2 → v3**: `load` ignores any legacy `url`; `save` omits it. The existing version gate then makes an old (v2-only) rdc refuse a v3 lockfile with a *"run `rdc upgrade`"* message rather than silently mis-resolving references — the correct behavior, since URL resolution now lives in the binary, not the file.

### 3.7 Mapping alignment: one unified `rdc://` substitution dictionary

The env-pair mapping (`.rdc/map/<src>-to-<tgt>.toml`) records the src↔tgt slug correspondence for every deployable object (today as per-kind TOML sections — `[queues]`, `[hooks]`, … — with composite `<parent>/<child>` keys for `engine_fields`/`email_templates`), plus a `[hook_templates]` section of cross-cluster URL pairs. Unify the internal-object correspondences to a single flat table of `rdc://<kind>/<slug>` → `rdc://<kind>/<tgt_slug>` pairs:

```toml
version = 2

[refs]
"rdc://engines/mtr-training"            = "rdc://engines/mtr-training"
"rdc://engine_fields/mtr-training/code" = "rdc://engine_fields/mtr-training/code"
"rdc://queues/exceptions"               = "rdc://queues/exceptions-renamed"
"rdc://workspaces/ferguson-mtr"         = "rdc://workspaces/ferguson-mtr"

[hook_templates]   # external (hook_template is not a deployable kind → no slug)
"https://src.rossum.app/api/v1/hook_templates/33" = "https://tgt.rossum.app/api/v1/hook_templates/33"
```

**The unifying invariant:** `rdc://<kind>/<slug>` is exactly `rdc://` prepended to the lockfile coordinate `objects[kind][slug]` — flat for `queues`/`schemas`/…, composite for `engine_fields` (`engine/field`) and `email_templates` (`ws/queue/template`). The *same* notation, parser, and printer now serve all three surfaces: snapshot references, the lockfile identity, and the mapping. (Parse rule: kind = first path segment after `rdc://`, slug = the remainder.)

**Payoff — the mapping *is* migrate's substitution dictionary.** `migrate` (§4.2) walks every string in the copied snapshot and replaces any whole-string value that appears as a `[refs]` key with its value — one dict lookup, no per-kind code in the hot path. (Whole-string match only: refs are standalone field values, never substrings of templates/formulas, so embedded text is never touched.) The same `[refs]` table also places each src object under its tgt slug in the tgt tree. Composite-key kinds (`engine_fields`, `email_templates`) are never reference *targets* (nothing points at them), so their entries are used only for placement and never matched by the string-walk — but unifying their notation keeps one consistent file.

**In-memory & API:** `Mapping` collapses to a single `refs: BTreeMap<String, String>` (rdc:// → rdc://) plus `hook_templates: BTreeMap<String, String>` (URLs). `lookup_tgt_slug(kind, src_slug)` becomes a thin wrapper (`refs.get("rdc://{kind}/{src_slug}")` → strip prefix); auto-match inserts rdc:// pairs. **Format change: mapping v1 → v2** — `load` migrates legacy per-kind sections (absorbing the existing `migrate_legacy_nested_keys` composite-key cleanup) into `[refs]`; `save` writes v2. Hand-edited files convert on first run. The flat keys sort alphabetically by kind, so the file stays grouped and readable.

## 4. The two commands

### 4.1 `rdc sync <env>` (enhanced)

Unchanged surface and pull/conflict UX. The **push** phase is upgraded to the §3.4 dependency-ordered, ref-resolving loop (the former `deploy/create.rs` + `deploy/apply.rs` logic, minus `rewrite_urls`). This single push path serves both:
- ordinary within-env sync (local edits → remote), and
- pushing a migrated snapshot (the migrated objects are simply "local changes" sync creates/patches on tgt).

`--only` selectors (today deploy-scoped) become available to sync's push, reusing `deploy/selection.rs` unchanged (it is pure filesystem/lockfile dep-checking).

### 4.2 `rdc migrate <src> <tgt>` (new, pure-local)

A snapshot→snapshot transform with **zero remote calls**. Produces a tgt snapshot byte-identical in *form* to a pulled tgt snapshot, which `rdc sync tgt` then pushes.

1. **Auto-match + mapping** (`deploy/map.rs`, moved as-is): build/load the `Mapping` (src_slug ↔ tgt_slug per kind); same-slug pairs auto-match, hand-curated renames honored.
2. **Validate sources** (`map::validate_mapping_sources`, moved as-is): every mapping entry's *source* slug must exist on disk, else error (the upfront guard added earlier this session — see the prior identity/drift fixes).
3. **Copy + slug-rename**: for each src object, write it into the tgt snapshot location under its **tgt** slug (file path + lockfile key renamed via the `[refs]` map), and rewrite references by walking every string and substituting any whole-string `rdc://…` value found as a `[refs]` key (§3.7). This is pure rdc://→rdc:// dictionary substitution — **no URLs, no lockfile URL lookups, no cluster hosts** (the radical simplification vs `rewrite_urls`).
4. **Overlay**: apply tgt overlays (per-kind field overrides) exactly as deploy does.
5. **External refs**: drop/neutralize per-env externals so the tgt snapshot is clean — `organization`/`modified_by` left for push-strip + pull-rewrite; `token_owner` sourced from tgt overlay/prompt; `hook_template` resolved via the cluster-pair `Mapping.hook_templates` (these stay because they cross clusters, not orgs).

`migrate` is **idempotent** and offline: re-running reproduces the same tgt snapshot. The user reviews the diff (`git`) before `rdc sync tgt`.

### 4.3 `rdc deploy` — removed

The `Deploy` clap subcommand is removed (no alias). Invoking `rdc deploy …` emits a guiding error:

> `rdc deploy` has been replaced. Run `rdc migrate <src> <tgt>` to produce the target snapshot locally, review the diff, then `rdc sync <tgt>` to push it.

## 5. What moves where (deploy machinery fates — grounded)

| file | fate | where it goes |
|---|---|---|
| `deploy/map.rs` (auto_match, list_* enumerators, `validate_mapping_sources`) | move | **migrate** (pure filesystem) |
| `deploy/create.rs` (`shape_create_body`, per-kind `create_*`, write-back) | move | **sync push**, minus `rewrite_urls` (→ ref resolution) |
| `deploy/apply.rs` (PATCH loop, drift check, dry-run diff, token_owner) | move | **sync push**, minus `rewrite_urls` |
| `deploy/store_extensions.rs` (template URL resolve, orphan adopt, install body) | move | **sync push** create_hook (cross-cluster, remote) |
| `deploy/hook_secrets.rs` (secret precheck + value injection) | move | **sync push** create/apply |
| `deploy/mdh.rs` (cross-env index reconcile) | split | copy `indexes.json` in **migrate**; reconcile in **sync push** via `push/mdh::{diff_indexes,apply_diff}` (§8) |
| `deploy/selection.rs` (`--only` dep-check) | keep | reused by **sync push** |
| `deploy/realign.rs` (within-env re-slug) | keep | orthogonal; stays under `doctor` |
| `deploy/common.rs::rewrite_urls`, `walk_strings_mut` | **delete** | replaced by §3.4 resolution + §3.3 walk |
| `deploy/common.rs::{normalize_for_cross_env_compare, bytes_equal_after_strip, tgt_drift_status}` | move | **sync push** (cross-env idempotency/drift still needed) |
| `deploy/run.rs` (orchestrator) | split | local-plan parts → migrate; remote create/apply/delete/MDH → sync push |
| `deploy/mod.rs` | delete | replaced by `migrate/mod.rs` + sync push modules |
| `mapping.rs` (`Mapping`, load/save, `lookup_tgt_slug`, `hook_templates`) | reform | unify to a single `[refs]` rdc://→rdc:// dict + `[hook_templates]` URL pairs (v1→v2, §3.7); `lookup_tgt_slug` becomes a wrapper; doubles as migrate's substitution dict |
| `Lockfile::slug_for_id` | keep | id-pinning + the new derivation-based `lookup_url` |
| `Lockfile::lookup_url`, `url_for_slug`, `slug_for_url` | reimplement | derive from `id` + env `api_base` (§3.6); stored `url` dropped (v3) |

**CLI wiring** (`src/cli/mod.rs`): the `Deploy { src, tgt, mirror, dry_run, force_overwrite_drift, only }` arm is replaced by `Migrate { src, tgt, … }`; the dispatch (`run`) routes `migrate` to the new module and `deploy` to the guiding-error stub. `--mirror`/`--dry-run`/`--force-overwrite-drift`/`--only` semantics move onto `sync`'s push (and `migrate` where local, e.g. `--mirror` prune of tgt-only objects becomes a migrate-time decision + a sync-push delete).

## 6. Robustness to remote renames (the guards)

The concern: *what if someone renames objects in the remote console?* Each case:

1. **Remote `name` change, id unchanged** — `slug_for_id` keeps the slug; `rdc://` refs are by slug → **refs unaffected**. `name` is just a value that syncs like any field. (The common case, fully handled.)
2. **Cosmetic slug↔name drift** — a pinned slug (`invoices`) no longer matches a renamed remote (`AP Invoices`). Purely cosmetic; `rdc doctor` realign (kept, §5) offers an opt-in re-slug that cascades through directories and refs.
3. **Lost / `--rebuild-lock` lockfile** — slugs are re-derived from names (no id→slug table). A full rebuild is **internally consistent** (refs and their targets are re-derived in the same pass over the same remote, so `rdc://` links still resolve), but a *cross-env mapping* curated against old slugs may need re-alignment. Documented; `migrate`'s `validate_mapping_sources` (§4.2) catches a mapping that now points at a vanished source slug and errors loudly.
4. **Cross-env slug divergence** — src renamed an object's slug but the mapping still references the old one → `validate_mapping_sources` errors before any write (already live; it caught a real stale mapping entry this session, see the prior identity/drift fixes).
5. **Delete + recreate remotely (new id)** — yields a new slug; a referer pointing at the old slug breaks transiently until the next pull aligns both. Unavoidable and identical to today's URL-based behavior; surfaced by `doctor`/sync as an unresolvable ref, never silently mis-resolved.

## 7. One-time hash re-baseline & snapshot migration

Storing refs as `rdc://` changes `disk_bytes`, hence `combined_hash`/`content_hash`, for every object with at least one internal ref. This is the same converge-on-first-sync pattern already used twice this session (URL-array sort, redaction; see the prior identity/drift fixes).

- **Mechanism:** on the first pull after upgrade, the API still returns URL bodies; the new pull post-pass (§3.3) writes `rdc://` to disk and computes the new hash. The lockfile's recorded hash was URL-based, so it differs by exactly the ref-form change. The pull self-heal (`pull/common.rs` `contains_hidden_fields`-style benign-rewrite path) is extended to recognize "delta is solely URL→`rdc://` re-encoding" and **rebase the lockfile entry silently** (no conflict, no spurious `RemoteEdit`).
- **Cleaner alternative (preferred), evaluated in planning:** a one-shot local rewrite of existing on-disk URL refs → `rdc://` using the lockfile's current URL→slug map, run on upgrade detection (or as the first step of `migrate`/`sync`). This makes the local hash match the new remote hash *immediately*, avoiding any three-way-merge ambiguity. Either path converges in one cycle; pick during planning.
- **Lockfile bumps v2 → v3** (§3.6): the redundant `url` is dropped (derived from `id` thereafter). `load` tolerates a legacy `url`; entries re-hash in place on first run. Orthogonal to the ref re-baseline; lands in the same first cycle.

## 8. MDH subsumption

MDH index propagation follows the same split:
- **migrate**: copy each dataset's `indexes.json` (regular + Atlas Search) into the tgt snapshot (pure file copy; datasets match by slug = slugified collection name).
- **sync push**: reconcile against the tgt's live collections via the existing `push/mdh::{diff_indexes, apply_diff}` (mirror-aware prune; changed defs drop+recreate; missing tgt collection → warn+skip; MDH-not-enabled 404 → skip). This is precisely the logic added in `deploy/mdh.rs` this session (the prior identity/drift fixes), relocated. MDH carries no `rdc://` refs (index definitions are collection-local), so no ref conversion applies. Dataset rows are never deployed.

## 9. Testing strategy

1. **Ref round-trip invariant (keystone):** for a representative multi-ref Value per kind, `url→rdc://` (pull pass) then `rdc://→url` (push resolve, against a tgt lockfile) reproduces the original target URLs; and `disk_bytes` containing `rdc://` is stable across re-encode. Property test over all internal-ref fields incl. nested (`rule.actions[].payload.queue`) and arrays (`hook.queues`, `engine.training_queues`).
2. **Pull conversion completeness:** assert no residual deployable-kind URL survives in a pulled snapshot (catches a missed field), and external URLs (user/org/hook_template/triggers) are preserved verbatim.
3. **migrate (pure-local):** given src snapshot + mapping + overlay, asserts the tgt snapshot's slugs and `rdc://` refs are renamed per mapping, overlays applied, externals neutralized, **and no network call is made** (no HTTP client constructed). Includes `validate_mapping_sources` error-on-missing-source.
4. **sync push dependency order:** new-workspace-with-queues within-env sync creates the workspace first (the wart fix); intra-kind `hook.run_after` and queue↔inbox pairing resolve via the two-phase pass.
5. **Re-baseline migration:** a legacy URL-form snapshot + URL-based lockfile converges to `rdc://` form on first pull with no conflict and a single hash rebase (extend the existing legacy-converges tests).
6. **Rewrite of affected deploy tests:** `tests/cli_deploy.rs` (`deploy_bootstraps_empty_target_with_url_rewriting`, `map_plan_apply_full_flow`, `deploy_queue_and_schema`, the `rewrite_urls_*` unit tests) are re-pointed at the migrate+sync push path and assert `rdc://`/slug resolution instead of URL-string rewriting. `tests/codec_invariant.rs::hash_consistent_with_disk_bytes` updated for slug-bearing bytes.
7. **Guiding error:** `rdc deploy` exits non-zero with the migrate/sync guidance.
8. Full `cargo test` + `cargo clippy -D warnings` (incl. `dead_code = "deny"` — orphaned `rewrite_urls` helpers become hard errors) + `cargo fmt --check`. **TDD throughout** (RED before GREEN); **live-verify** non-destructively against the test snapshot (temp copy), reproducing real behavior empirically before claiming a fix.

## 10. Rollout & risks

**Approach:** staged, each stage shippable and green.
- **S1 — Portable refs + lockfile v3:** reimplement `lookup_url`/`url_for_slug` to derive from `id` + env `api_base` and drop the stored `url` (lockfile v2→v3, §3.6); pull post-pass URL→`rdc://`; push resolve `rdc://`→URL inside the *existing* sync push (no command changes yet); re-baseline self-heal; full suite green. (Snapshots + lockfile become env-portable; deploy still works via its own path, now consuming derived URLs.)
- **S2 — Dependency-ordered sync push:** fold deploy's create order + create/apply logic into sync push; delete `rewrite_urls`.
- **S3 — `migrate` command + unified mapping:** reform `Mapping` to the single `[refs]` rdc://→rdc:// dict + `[hook_templates]` (v1→v2, §3.7); move map/overlay/local-plan; migrate does rdc://→rdc:// dictionary substitution; pure-local; tests.
- **S4 — Remove `deploy`:** guiding-error stub; relocate store_extensions/hook_secrets/mdh; rewrite affected tests.

**Risks & mitigations**
- *Missed internal ref field* → kind-qualified `rdc://` + walk-all-strings resolution (no per-field schema) + completeness test #2 + the inventory table §3.2. The whole point of kind-qualification is that resolution can't miss a nested ref.
- *Forward-ref ordering on pull* → conversion is a post-pass after all slugs are known (§3.3), not inline.
- *Cyclic/intra-kind refs on push* → two-phase create-then-wire (§3.4), inherited from deploy; verified by test #4.
- *Re-baseline churn* → §7 self-heal/one-shot rewrite; converges in one cycle; matches two prior precedents.
- *Engines unverifiable live* (403 on TEST tokens) → grounded in code + wiremock, as before (the prior identity/drift fixes).
- *Large review surface* → staged S1–S4, each green; per-kind logic stays in small modules.

## 11. Decisions (settled)

- Reference representation: **`rdc://<kind>/<slug>`** (kind-qualified, self-describing, schema-free resolution).
- Snapshot model: **environment-portable** (option b from brainstorming); internal refs are slugs, externals stay per-env.
- Commands: **`rdc migrate` (pure-local) + `rdc sync` (push upgraded)**; **`rdc deploy` removed, no alias**, guiding error.
- `rewrite_urls` (URL→URL): **deleted**; migrate does slug→slug, sync push does slug→URL.
- Backrefs: convert uniformly, keep stripping on push; **disk-drop deferred** (out of scope).
- MDH: **subsumed** into migrate(copy)/sync(push).
- Re-baseline: **one-time, transparent**, converges in the first cycle.
- Lockfile: **drop the stored `url`, derive from `id`** (v2→v3); slug key stays the portable identity (§3.6).
- Mapping: **unify to one `rdc://`→`rdc://` `[refs]` dict + `[hook_templates]` URL pairs** (v1→v2, §3.7); same notation as snapshot refs + lockfile; doubles as migrate's substitution dictionary.
- Unifying invariant: **`rdc://<kind>/<slug>` = `rdc://` + the lockfile coordinate `objects[kind][slug]`** — one notation/parser across snapshots, lockfile, and mapping.

## 12. Open questions (resolve during planning)

- Re-baseline mechanism: pull self-heal vs one-shot local rewrite (§7) — pick the lower-ambiguity path after prototyping S1.
- Exact two-phase handling deploy uses today for `hook.run_after` and queue↔inbox — confirm and preserve verbatim (§3.4).
- Whether `--mirror` prune is a `migrate`-time local deletion (objects absent from src removed from tgt snapshot) feeding an ordinary sync-push delete, vs a sync-push-time mirror flag — lean migrate-time for "snapshot is the source of truth," confirm in S3.
- Module home for the shared push (`sync/push/*` vs a neutral `push/*`) and the migrate module layout — confirm in S2/S3.
- `token_owner`/`hook_template` ergonomics in `migrate` (prompt at migrate time vs defer to sync push) — confirm in S3.
