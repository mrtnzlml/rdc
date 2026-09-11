# Queue automation belongs to the target env — and `--carry` replaces the per-field flags

Date: 2026-09-11
Status: designed

## Problem

Two separate things, one design, because the second is the shape the first
should land in.

**1. A queue's automation configuration is promoted today, and should not be.**
`automation_enabled` and `automation_level` decide whether a queue auto-exports
documents without human review. That is a per-organization operational
decision — a team switches a prod queue to `confident` after watching its
accuracy for a month — not solution configuration that travels with a schema
change. `rdc migrate` treats both as ordinary content and carries them
verbatim, so promoting `test` → `prod` silently overwrites the target org's
automation policy with the source org's. `quality_spot_check_percentage`, the
QA sampling rate for automated documents, sits in the same category and has the
same problem.

**2. The opt-in surface is one flag per field and does not scale.** migrate
already owns two such fields behind two booleans — `--migrate-score-thresholds`
and `--migrate-email-prefixes` — and a third field would mean a third boolean.
The fields are all instances of one concept ("the target env owns this; pass a
flag to carry the source's instead"), so they should be values of one option,
not a growing row of flags.

## Verified facts

### Rossum API — official OpenAPI specification

Fetched 2026-09-11 from
`https://elis.rossum.ai/api/docs/openapi/openapi-specs/openapi.json`.

| Probe | Result |
| --- | --- |
| `queue_base.properties.automation_enabled` | `boolean`, **default `false`**, not `readOnly` |
| `queue_base.properties.automation_level` | `string`, enum `never` / `confident` / `always`, **default `"never"`**, not `readOnly` |
| `queue_base.properties` — `readOnly` set | exactly `id`, `url`, `counts`, `status` |
| `POST /api/v1/queues` request body | `$ref queue_base` **with `required: ["name", "schema"]`** — nothing else is mandatory on create |
| `PATCH /api/v1/queues/{id}` request body | `$ref queue_base`, no `required` |
| `queue` schema (`allOf: queue_base + required[…]`) | the long `required` list — which includes `automation_enabled`, `automation_level`, `quality_spot_check_percentage`, `training_enabled` — is referenced **only from responses** (`GET`/`POST`/`PATCH` `200`/`201`). It is the response contract, not a create requirement |
| `quality_spot_check_percentage` | present in that **response**-required list, **absent from `queue_base.properties`** — so the public spec does not document its type, default, or writability |
| `queue_base.properties.settings.automation` | object; `automate_duplicates` (default `true`), `automate_suggested_edit` (default `false`) |
| `queue_base.properties.settings.suggested_edit` | enum `suggest` / `disable`, default `"disable"` |

The consequence that matters: **dropping all three keys on a brand-new target
queue is safe**, and lands on automation OFF. No `email_prefix`-style
"mandatory on create" exception is needed.

### Real snapshots — 240 pulled `queue.json` files across nine local rdc projects (2026-09-11)

| Observation | Result |
| --- | --- |
| `automation_enabled`, `automation_level`, `quality_spot_check_percentage`, `default_score_threshold` | present in **240/240**, always **top-level** — a top-level key swap suffices; none of the three new keys needs the position-agnostic search `default_score_threshold` has |
| `settings.suggested_edit` | present in 240/240 (93 `disable`, 147 `suggest`) |
| `settings.automation` | present in 39/240 |
| `settings.autopilot` | present in **0**/240 |
| automation actually differs per env | yes — e.g. one project runs eight prod queues at `confident`/`always` while its other envs are all `never`; another has automation on in a dev env and off in prod |
| `quality_spot_check_percentage` values | `0.0` (151) and `0.02` (89) — something sets it per queue |
| `training_enabled` values | `false` (133), `true` (107) |

### rdc codebase

| Probe | Result |
| --- | --- |
| `automation_enabled` / `automation_level` / `quality_spot_check_percentage` in `src/` | no handling at all — ordinary content. `quality_spot_check_percentage` appears nowhere in `src/` |
| `strip_for_create` / `strip_for_cross_env_patch` / `kind_specific_strip` | none of the three is stripped; all are sent on POST and PATCH today |
| `--migrate-score-thresholds` / `--migrate-email-prefixes` | outside historical specs and plans under `docs/`, referenced only in `src/cli/mod.rs`, `src/cli/migrate/mod.rs`, `tests/cli_migrate.rs`, `README.md` |
| desktop app | does not pass either flag (in-app promote was removed — see `2026-09-01-desktop-watch-and-promote-removal-design.md`) |
| `templates/gitlab-ci.yml` | its deploy job runs `rdc migrate "$RDC_SRC" "$RDC_ENV" --mirror --yes` — neither flag appears |
| `init.rs` generated `README.md` / `CLAUDE.md` | never mention either flag, so no markdown region is affected |
| existing precedent | `reconcile_training_enabled` — unconditional, top-level, matched-adopts / new-drops |

### Naming

"Organization settings" is already taken in rdc: README §*Organization settings*
is the organization object's `settings` subtree, which sync pushes and migrate
promotes. A migrate option spelled `--migrate-org-settings` would be genuinely
ambiguous with it. The option is therefore named for the action (`--carry`),
which is the verb README already uses for these fields.

## Semantics

### The groups

| group | fields | today | after |
| --- | --- | --- | --- |
| `score-thresholds` | schema datapoint `score_threshold`, queue `default_score_threshold` | target-owned; `--migrate-score-thresholds` carries | unchanged behavior, new spelling |
| `email-prefixes` | inbox `email_prefix` | target-owned, with a create exception; `--migrate-email-prefixes` carries | unchanged behavior, new spelling |
| `automation` | queue `automation_enabled`, `automation_level`, `quality_spot_check_percentage` | **promoted verbatim** | **target-owned**; `--carry automation` carries |
| *(none)* | queue `training_enabled` | always target-owned, no opt-in | unchanged |

`quality_spot_check_percentage` lives inside `automation` rather than in a group
of its own: it is the sampling rate for automated documents, and a
`quality-spot-check` group is a value nobody would ever pass alone.

`training_enabled` deliberately stays outside the option. Its existing rationale
— there is no case for blindly propagating a training toggle across orgs — is
unchanged by this design, and adding an opt-in nobody asked for is the kind of
surface this design exists to reduce.

### The automation reconcile

Identical in shape to `reconcile_training_enabled`:

- **matched target** (the target snapshot file exists): adopt the target's value
  for each of the three keys; where the target lacks a key, drop it from the
  migrated body.
- **new target** (no target file): drop all three, so the server applies its own
  defaults — `automation_enabled: false`, `automation_level: "never"`.
- **top-level only.** Verified against 240 real snapshots.
- runs with the other env-tuned reconciles in `transform_file`, **before** the
  overlay — so `[queues.<slug>] automation_level = "confident"` in the target's
  `overlay.toml` still wins, per the documented precedence (per-object override
  > kind-wide `"*"` default > reconciled value).
- `--carry automation` skips the reconcile entirely: the source's values carry
  verbatim, which is today's behavior.

### Why `quality_spot_check_percentage` is in the group either way

Its writability is undocumented (see the facts table), and both possible answers
argue for reconciling it:

- **writable** → migrate clobbers the target org's sampling rate today; the
  reconcile stops that.
- **read-only** → migrate writes the source's value into the target snapshot,
  the server ignores it on PATCH, and local and remote disagree forever — a
  phantom diff no sync can converge. The reconcile removes it.

Only the README wording depends on which it is, which is why the live probe
below is a documentation gate and not a design gate.

### Idempotency

A created queue POSTs without the three keys, the server fills its defaults, and
the write-back/pull puts them in the target snapshot. The next migrate then
takes the matched path and adopts exactly those values. Byte-stable from the
second run on, like the score thresholds.

## CLI

```
--carry <GROUP>    repeatable and comma-separated
                   score-thresholds | email-prefixes | automation | all
```

```sh
rdc migrate test prod                                  # target owns all three groups
rdc migrate test prod --carry automation
rdc migrate test prod --carry score-thresholds,automation
rdc migrate test prod --carry all
```

A clap `ValueEnum` with `value_delimiter = ','` and `ArgAction::Append`, so an
unknown value produces clap's own `invalid value … possible values` error with
no hand-written validation. `all` expands to every group; a group added later
widens it, which is the intended reading of "carry everything the target
normally owns".

`--migrate-score-thresholds` and `--migrate-email-prefixes` are **deleted**, not
aliased.

## Backward compatibility

- **Pipelines.** The removed spellings now fail at argument parse —
  `error: unexpected argument '--migrate-score-thresholds' found` — before any
  network call or write. Projects pin `RDC_VERSION` to a release tag, so no
  pipeline changes behavior until someone deliberately bumps it, and the failure
  is loud and immediate rather than silent. A project that passed either flag
  rewrites one line.
- **Snapshots.** No format change. Overlay, lockfile and `mapping.toml` are
  untouched; existing overlays keep working and still win over the reconcile.
- **A limitation to state plainly in the README.** This stops *future*
  clobbering. Where a past migrate + sync already pushed the source's automation
  config into the target org, the target env genuinely holds that value now —
  migrate reads the target snapshot, so it faithfully keeps the clobbered value.
  Recovery is a deliberate edit in the target env (or its `overlay.toml`),
  not something migrate can infer.
- **Release.** Commit as `feat!`, which the weekly release's level derivation
  reads as a minor bump — right for a CLI removal at 0.x.

## Implementation sketch

1. **Generalize the reconcile.** Replace `reconcile_training_enabled(value,
   kind, tgt_path)` with `reconcile_target_owned_keys(value, kind, tgt_path,
   keys: &[&str])` — same body, looping over `keys`. Call it twice from
   `transform_file`: unconditionally for `["training_enabled"]`, and gated on
   `!carry.automation` for `["automation_enabled", "automation_level",
   "quality_spot_check_percentage"]`.
2. **Collapse the flags into a `Carry` struct** (three bools, built from the
   parsed `Vec<CarryGroup>`), threaded `Command::Migrate` → `migrate::run` →
   `run_at` → `transform_file` in place of the two `bool` parameters. This
   shortens `transform_file`'s argument list rather than lengthening it, and
   stops the `/* migrate_score_thresholds = */ true` positional-bool call sites
   from multiplying.
3. **Delete the two `#[arg]` declarations** in `src/cli/mod.rs` and add the
   `--carry` one.
4. **README.** `§Score thresholds` and `§Inbox email prefixes` become siblings
   of a new `§Queue automation`, under a short group table and the `--carry`
   syntax; the precedence sentence that today names `score_threshold` /
   `default_score_threshold` / `training_enabled` gains the three new keys; the
   limitation above is stated in the new section.

## Verification plan

- **Open gate (documentation only): live `OPTIONS /v1/queues`** to establish
  whether `quality_spot_check_percentage` is writable, and its declared type and
  bounds. Needs a sandbox token. The design holds either way (see above); only
  the README sentence depends on the answer.
- **Live promotion scenario** (needs the two-org `RDC_LIVE_TGT_*` setup): a
  source queue with automation on and a target with it off → migrate → the
  target keeps its own values; then `--carry automation` → the source's are
  carried.
- **Offline unit tests** for `reconcile_target_owned_keys`: matched target
  adopts; new target drops; target missing one key drops just that key;
  `training_enabled` behavior unchanged.
- **Integration tests** in `tests/cli_migrate.rs` mirroring the existing
  threshold and prefix tests, plus one asserting the removed flags are rejected
  and one asserting `--carry score-thresholds,automation` parses as two groups.

## Out of scope

- `settings.automation.*` (`automate_duplicates`, `automate_suggested_edit`,
  `automation_assistant_enabled`, `automation_assistant_demo`) and
  `settings.suggested_edit`. The first is a nested sub-object needing a
  different reconcile shape; the second is document-splitting behavior, not
  automation policy. Both keep promoting. Either can become another `--carry`
  value later without touching the option's shape — which is the point of the
  option.
- Any project-level configuration of the default (an `rdc.toml` `[migrate]`
  section). The option stays per-invocation.
