# Field-limit pre-flight: hoist above the network, and close the sidecar gaps

Date: 2026-08-12
Status: approved, ready for implementation planning

## Problem

`rdc` already refuses to push a field longer than the Rossum API's declared
`max_length` (`src/snapshot/limits.rs`, wired into `sync` and `doctor` by
commit `3c8f347`). Two things are still wrong with it.

**It speaks too late.** `sync::run_cycle` resolves the token, then lists the
whole remote catalog (13 endpoints), then scans locally, then classifies, and
only then reports the violation. An error that is knowable from local bytes
alone — with no token and no network — arrives after every remote call has
already been paid for.

**It does not see values stored in sidecars, and one of its table entries can
never fire.** `rules.trigger_condition` is listed with a 4000-char limit, but
the rules codec extracts that field into a `<slug>.py` sidecar and
`src/snapshot/codec/rules.rs` has a test asserting it is *not* in the JSON —
while `check_field_limits` only inspects top-level JSON keys. Schema formulas
have the same shape and are not covered at all.

Both failures share the original incident's character: an over-length value is
a *permanent* push failure. No retry can succeed while the bytes stay long, and
because push precedes pull, the project stops converging entirely.

## Verified facts

Everything below was confirmed against a live Rossum deployment on 2026-08-12,
not taken from documentation.

**The API self-describes its limits.** `OPTIONS /v1/<kind>` →
`actions.POST.<field>.max_length`. A harvest across all ten pushable kinds
confirms every number currently in `field_limits()` is correct, and surfaces
these uncovered ones:

| Kind | Location | Limit | Where it lives on disk |
|---|---|---|---|
| `rules` | `trigger_condition` | 4000 | `rules/<slug>.py` sidecar |
| `schemas` | datapoint `formula` | 2000 | `formulas/<id>.py` sidecar |
| `schemas` | datapoint `prompt` | 5000 | nested in `schema.json` |
| `schemas` | datapoint `memory.index_formula` | 2000 | nested in `schema.json` |
| `rules` | `actions[].payload.content` | 4096 | nested in `<slug>.json` |

**The server enforces all of them.** `POST` probes returned `400` for a 2001-char
formula, a 5001-char prompt, and a 4001-char `trigger_condition`. Each probe was
rejected outright, so nothing was created.

**The server's own error is close to unactionable for nested fields.** An
over-length formula answers with:

```json
{"content":[{"children":{"0":{"formula":["Ensure this field has no more than 2000 characters."]}}}]}
```

There is no datapoint id anywhere in that payload — only a positional index into
a nested structure. This is materially worse than the top-level hook
`description` case that motivated the original fix, which at least named its
field.

**The server trims surrounding whitespace before validating.** A single request
carrying both a 2001-char formula and a separate 2000-char-plus-newline formula
came back with an error for the first datapoint only. Counting raw bytes would
therefore reject a value the server accepts, and would do so for the very common
case of an editor adding a final newline to a sidecar.

**`OPTIONS` metadata does not describe the wire shape of `rules.actions`.** The
metadata nests limits under a polymorphic wrapper
(`actions.child.show_message.payload.content`), but real rules serialize each
action flat with a `type` discriminator:

```json
{"id": "…", "enabled": true, "type": "show_message", "event": "validation",
 "payload": {"type": "warning", "content": "…", "schema_id": "…"}}
```

A walker written from the metadata alone would match nothing. Hooks nest their
metadata the same way (`actions.POST.{function,webhook,job}.children.<field>`),
which is already documented in the module.

**Hoisting the scan is safe.** `list_remote` does not mutate `lockfile.objects`
— its only `upsert` call sits in the separate `record_object` helper — and
`push::scan::scan` reads nothing but local files and each entry's
`content_hash`. Neither depends on the other.

**`resolve_token` can perform a network login and write `secrets/<env>.secrets.json`.**
Running the pre-flight ahead of it means an over-length field is reported without
a login round-trip and without touching the secrets file.

## Design

### A. Hoist the offline validations above all network work

In `run_cycle`, move the local scan and the offline validations to run
immediately after the lockfile is loaded — before `resolve_token`, before the
client is constructed, and before `list_remote`.

The hoisted `changes` and `tombstones` are reused by the existing classify
phase, so the tree is still walked and hashed exactly once. This is a
reordering, not additional work.

`json_parse_errors` moves with `field_limit_violations`. Both are local,
permanent, and offline-detectable; splitting them across two points in the cycle
has no justification once one of them moves.

Refusal semantics are unchanged:

- refuse only when `!no_push` — `--no-push` is an audit mode and still proceeds
- `--dry-run` still prints the full plan rather than bailing early, preserving
  its role as a complete preview
- the message text and the `progress.event(Action::Plan, "field limit errors")`
  surface stay as they are

The observable win: a real `rdc sync` with an over-length field fails in well
under a second, with zero network calls, and without needing a valid token.

### B. Extend coverage to sidecars and nested authored text

Validate the five locations in the table above. Two distinct mechanisms:

**Sidecar-backed values** (`rules/<slug>.py`, `formulas/<id>.py`) are read from
disk and checked directly. This gives a better error than any JSON path could:
it names the exact file the user opens in an editor.

**Nested JSON values** (`prompt`, `memory.index_formula`,
`actions[].payload.content`) are found by walking the parsed body. The schema
walk reuses the recursion shape that `extract_formulas` and `merge_formulas`
already implement — array `children` for sections and tuples, a single object
`children` for a multivalue — so line-item column fields are covered, not just
top-level datapoints. The rules walk iterates `actions[]` flat, per the verified
wire shape.

The existing top-level checks are untouched.

### C. Reporting and counting

`FieldLimitViolation.field` becomes an owned `String` describing the location,
and `path` points at the file the user must edit — the sidecar when the value
lives in one. Violations name the object a human recognizes, because the server
cannot: the datapoint id for schema fields, the action `type` and index for rule
actions.

Illustrative output:

```
rules/example-rule -- envs/dev/rules/example-rule.py: trigger_condition is 4210 characters, the API allows 4000 (shorten it by 210)
schemas/invoices -- envs/dev/workspaces/main/queues/invoices/formulas/total_amount.py: formula is 2431 characters, the API allows 2000 (shorten it by 431)
schemas/invoices -- envs/dev/workspaces/main/queues/invoices/schema.json: prompt on datapoint 'invoice_id' is 5104 characters, the API allows 5000 (shorten it by 104)
rules/example-rule -- envs/dev/rules/example-rule.json: actions[1] (show_message) payload.content is 4200 characters, the API allows 4096 (shorten it by 104)
```

Length is counted as Unicode code points (Django's `MaxLengthValidator` measures
Python `len(str)`), after trimming whitespace from both ends of the value.

What was actually verified is narrower than that rule: a *trailing newline* is
demonstrably not counted. Trimming both ends is nonetheless the correct
implementation, because the two directions are not symmetric. If the server
trims less than we do, we under-report and the server rejects the value exactly
as it does today; if it trims more, over-reporting would block a valid push.
Trimming at least as much as the server is the only safe side of that trade.

Trimming is applied uniformly, including to the existing top-level fields —
the server's validation does not vary by field, and leaving the old fields
untrimmed would keep a latent false positive for any value carrying trailing
whitespace.

The module's governing rule is unchanged and extends to every new check: **never
over-report**. A missed violation degrades to today's behavior — the server
rejects it exactly as it does now. A false positive blocks a legitimate push,
which is strictly worse than the error it would replace.

### D. Tests

- Unit tests per new location, each with boundaries: exactly at the limit
  passes, one over fails, and limit-plus-trailing-newline passes.
- A schema fixture exercising a formula on a line-item column, not just a
  top-level datapoint — the multivalue recursion is the part most likely to be
  written wrong.
- An integration test asserting that a real `sync` with an over-length rule
  `description` fails having issued **zero** requests to the mock server. Today
  the same scenario issues 13 list calls first, so this test pins the actual
  behavior change rather than restating the existing refusal.
- The `no_validated_field_is_stripped_before_push` invariant extended to cover
  the nested and sidecar locations, so a future strip rule cannot silently turn
  a check into a false positive.

## Backward compatibility

**Projects that sync cleanly today keep syncing cleanly.** Every newly validated
location is one the server already rejects, so the only snapshots newly refused
are ones whose push was already impossible. The change moves the failure earlier
and makes it legible; it does not make anything fail that would have succeeded.

**Risk of the opposite — a false positive blocking a valid push — is the one to
guard.** Three mitigations: only fields empirically confirmed enforced are
validated; lengths are counted with the server's whitespace trimming; and the
strip invariant test keeps the checker from validating a field the push path
removes.

**`FieldLimitViolation.field` changes type** from `&'static str` to `String`.
This is a `pub` item in the `rdc` library crate. No in-repo consumer outside
`sync` and `doctor` uses it — the desktop bridge does not reference it — so the
blast radius is those two call sites.

**Reordering changes which error appears first** when a project has both an
over-length field and a broken token: the offline field error now wins. This is
the better outcome (it is actionable without credentials and avoids writing a
refreshed token for a cycle that cannot proceed), but it is a visible change in
behavior and should be called out in the commit message.

**`--no-push` and `--dry-run` behavior is deliberately preserved**, so existing
audit and preview workflows — including anything in `templates/` — are
unaffected.

## Out of scope

Nested limits on machine-set values: hooks `webhook.config.url` (2048),
`config.secret` (255), `config.app.url` (255), `job.config.actor_name` (255),
`sideload[]` (100); rules `actions[].id` (50, a server-generated UUID) and
`payload.schema_id` (255); queues `settings.columns[].schema_id` (50) and the
`ui_edit_values` / `enum_options` label and value fields (255). These are not
free text a human grows over time, and each one added is additional surface for
a false positive.

Validation classes other than `max_length` (choices, slug patterns, required
fields, uniqueness) are not addressed here.

Surfacing the check anywhere other than `sync` — a standalone `validate`
command, a `migrate`-time check, or widening `doctor` beyond unpushed changes —
was considered and explicitly declined. `doctor` already reports violations
offline for anyone who wants an on-demand check.
