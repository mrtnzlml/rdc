# Live coverage for create ordering, and for engines / engine fields

## Problem

The live suite has 20 scenarios and none of them asserts the order in which
`rdc sync` creates objects. Two gaps sit behind that.

**1. Creation itself is barely covered, and ordering not at all.** Almost every
scenario seeds the remote with raw API POSTs (`support/seeder.rs`) and then only
*pulls*. Exactly three exercise rdc's own create path — `live_deploy_flow`,
`live_migrate_overlay_and_mirror` (both **skip** without `RDC_LIVE_TGT_*`, so a
single-org run creates nothing) and `live_mdh_create_when_absent`. Where creates
do happen, the assertion is that the command exited zero; the order is left to
whatever the real server happens to reject.

The only explicit ordering assertion anywhere in the repo is hermetic and
mocked: `push_create_dependency_ordered_workspace_schema_queue`
(`tests/cli_sync.rs:10104`, asserted at `:10312`). It pins one edge —
workspace/schema before queue — against wiremock.

So the edges the server does **not** enforce are pinned nowhere:

- `labels → rules`. A rule's `actions` can carry `rdc://labels/<slug>`, resolved
  at rule-create time (`push/mod.rs:147`). Wrong order gives "Invalid hyperlink
  — No URL match", but only for a project that actually uses label actions, and
  no test uses one: `testdata/live/bodies/rules/totals.json` references a queue
  and nothing else.
- The deferred **relink** PATCH landing after both hook POSTs. If it silently
  stopped firing, the create would still succeed and `run_after` would just be
  empty. `assert_converged` would catch it on the next cycle — as drift, not as
  an ordering failure, and with a diff that names a file rather than the cause.

**2. `engines` and `engine_fields` have no live coverage at all.**
`grep -i engine tests/live/` returns nothing. Both kinds are fully registered —
`PUSH_CAPABLE` and `DELETABLE` in `src/kinds.rs`, pull drivers, push drivers,
`Paths::engine_dir` / `engine_fields_dir`, base cache. They were left out of the
harness because the sandbox used to answer `403` to `POST /engines`
(recorded in the project's memory as "engines 403").

**That is no longer true** (verified below). Which matters, because the edge the
engines carry is the one that has actually broken in production. From
`push/mod.rs:88-110`:

> Engines and their fields before schemas and queues. Unlike every other edge in
> this graph, this one is not a reference the payload carries — it is a
> server-side CONTENT check […] Pushed last, as "org-level leaves", the engine
> fields did not exist yet and a promote into a fresh env died on its first
> queue.

That fix (`0d60d3e`) shipped with no test. It is a comment and a line ordering,
and nothing stops the next refactor from moving `engines::push` back down.

## Verified facts

All probed against the live sandbox (org 214757, `api.elis.rossum.ai/v1`) on
2026-08-31. Probe objects were named `rdc-it-probe*`; see *Cleanup debt*.

### Engines and engine fields are creatable now

| Request | Result |
| --- | --- |
| `POST /engines {"name","type":"extractor"}` | `201` |
| `POST /engine_fields {"engine","name","label","type","subtype"}` | `201` |
| `DELETE /engine_fields/<id>` (never bound) | `204` |
| `DELETE /engines/<id>` (never bound) | `204` |
| `OPTIONS /engines` → `type` choices | exactly `extractor`, `splitter` |

The "engines 403" premise is dead. A never-bound engine round-trips cleanly.

### The ordering edge is enforced by the server

Workspace + schema with one extracted datapoint `probe_field`, engine with **no**
fields:

```
POST /queues {"workspace","schema","engine"}
  → 400 {"non_field_errors": ["Engine (id: 2199) restriction: extracted field
     'probe_field' is not present among names of engine fields"]}
```

Then `POST /engine_fields {"name":"probe_field",…}` → `201`, and the identical
queue POST → `201`. The dependency is three deep and real:
`engine → engine_fields → queue.engine`. A push-order regression is a hard 400
from the real API, not a subtle diff.

The back-edge closes the cycle too: `PATCH /engines/<id> {"training_queues":
["<queue url>"]}` → `200`.

### A bound engine cannot be deleted for up to 24 hours

This is the expensive fact, and it constrains teardown:

| Request | Result |
| --- | --- |
| `PATCH /queues/<id> {"engine": null}` | `400` "Queue does not have an engine. Set dedicated_engine, generic_engine or engine." |
| `DELETE /engines/<id>`, queue alive | `400 engine_attached_to_active_queues` |
| `DELETE /engines/<id>`, queue `deletion_requested` | `400 engine_attached_to_queues_waiting_for_deletion` — "Try again after the queue is deleted, after up to 24 hours." |
| `DELETE /engine_fields/<id>`, while bound | `409 conflict_referenced` "Cannot delete engine field used in a schema." |

There is no unbind escape hatch: nulling `engine` is refused outright, so a
queue can only be moved to *another* engine, which stalls that one instead.

**This is NOT an rdc cascade-order defect** — an earlier reading of it as one
was wrong. `push/deletes.rs:16-18` puts `engine_fields → engines` before
`queues`, which looks backwards against the constraint above. Reordering would
change nothing: the server's rule is "after the queue is *deleted*, after up to
24 hours", and `DELETE /queues` returns `202 deletion_requested` — the queue is
merely *draining*, not gone. Probed directly: deleting the queue first and then
the engine still returns `400 engine_attached_to_queues_waiting_for_deletion`.
No in-run ordering can satisfy a constraint on asynchronous purge.

What already handles it correctly is `run_deletes`' documented
**skip-and-continue** (`deletes.rs:181-191`): a refused DELETE warns, increments
`DeleteCounts::failed`, **leaves the lockfile entry intact so a later sync
retries**, and continues the loop so every sibling and parent still gets
deleted. The engine survives the run, is retried on a sync a day later, and
succeeds. That is the right behaviour, and the cascade order needs no change.

### On-disk layout, read off a real pull

`rdc sync test --no-push` against the sandbox pulls `engines (6)` and
`engine_fields (158)`, so the pull path already works and is silently exercised
by every existing scenario.

```
envs/<env>/engines/<engine-slug>/engine.json
envs/<env>/engines/<engine-slug>/fields/<field-slug>.json
```

The field's own url is **compound**: `rdc://engine_fields/<engine>/<field>`, and
it carries `engine: rdc://engines/<engine>`. Three of the five real sandbox
queues bind an engine as `engine: rdc://engines/<slug>`, so the fixture shape
below is what the kind actually looks like in the wild.

### `RDC_TRACE_HTTP` is a usable ordering oracle

`src/api/retry.rs:38` already ships an opt-in per-attempt CSV trace, written from
the single `send_once` chokepoint both the core and Data Storage clients funnel
through:

```
epoch_ms,limiter_wait_ms,duration_ms,status,desc
1788174659806.4,0.0,371.1,200,GET https://api.elis.rossum.ai/v1/workspaces?…
```

`desc` is last and unquoted (it contains commas) — split on the first four. Two
practical notes: the file also captures `POST …/svc/data-storage/…` from MDH, so
a consumer must filter on the core API base; and it records *attempts*, so a
retried request appears twice.

No production code needs to change to use it.

## Design

### Approach

The ordering scenario **hand-authors a local snapshot** and syncs it into a
single org. Rejected alternatives: extending the shared `manifest.toml` and
`deploy_flow` (creates engines in *both* orgs, doubling the orphan cost, and only
runs with `RDC_LIVE_TGT_*`); and a same-org `migrate test → test2` (both envs see
each other's objects in a whole-org pull — exactly why `deploy_flow` moved to two
orgs).

Hand-authoring wins on three counts: it runs on **every** live run, it is the
only test anywhere proving a hand-written snapshot — what a user commits to git —
deploys against a real API, and it reads side by side with the hermetic keystone
test it mirrors.

### 1. `tests/live/support/trace.rs` (new)

Parses the CSV into `Vec<TraceLine { status, method, url }>`, filtering to the
core API base so data-storage POSTs never match.

```rust
impl Trace {
    fn first(&self, method: &str, path: &str) -> Option<usize>;
    fn last(&self, method: &str, path: &str) -> Option<usize>;
    fn assert_before(&self, a: (&str, &str), b: (&str, &str), why: &str);
}
```

`assert_before` compares **last-of-A against first-of-B**. That is sound because
`push_classified` awaits its per-kind drivers sequentially: within-driver
concurrency (`push/concurrent.rs`) can interleave requests of one kind, never
across kinds. Min/max indices are also immune to a retried attempt appearing
twice. A failure prints both indices and the surrounding window of the trace, so
the message says *what ran instead*.

Hermetic unit tests over a literal CSV string, running under a plain
`cargo test`, as every other support module does.

### 2. `ProjectFixture::run_rdc_traced`

Returns `(Output, Trace)`, setting `RDC_TRACE_HTTP` to a per-invocation path
inside the tempdir. `run_rdc` is untouched.

### 3. `testdata/live/snapshot/` (new)

The create fixture as a literal env tree, with `{{RUN}}` placeholders in both
paths and file contents; `support/snapshot.rs` copies it into `envs/test/` with
the run id substituted. Objects are authored the way a create is authored —
`"id": 0, "url": ""`, cross-refs as `rdc://` (the shape
`tests/cli_sync.rs:10240` already uses).

```
engines/{{RUN}}-engine/engine.json
engines/{{RUN}}-engine/fields/probe-field.json
labels/{{RUN}}-priority.json
workspaces/{{RUN}}-ws/workspace.json
workspaces/{{RUN}}-ws/queues/{{RUN}}-invoices/queue.json    engine → rdc://engines/{{RUN}}-engine
workspaces/{{RUN}}-ws/queues/{{RUN}}-invoices/schema.json   one extracted datapoint: probe_field
workspaces/{{RUN}}-ws/queues/{{RUN}}-invoices/inbox.json
workspaces/{{RUN}}-ws/queues/{{RUN}}-invoices/email_templates/{{RUN}}-notice.json
hooks/{{RUN}}-validator.json + .py
hooks/{{RUN}}-post-validator.json                           run_after → rdc://hooks/{{RUN}}-validator
rules/{{RUN}}-totals.json                                   action    → rdc://labels/{{RUN}}-priority
saved_views/{{RUN}}-view.json                               filter    → the queue
```

The schema carries exactly **one** extracted datapoint, so one engine field
covers it. That keeps the unavoidable orphan to a single engine plus a single
field, and keeps the 400 message — should it ever fire — down to one line.

Every body is verified against the API as it is written, not assumed. The rule
action, saved-view filter and email-template shapes are the ones to check.

### 4. `scenarios/ordering.rs` — `live_push_create_ordering`

Author the snapshot, `sync test` under trace. Success on its own proves the
engine fields preceded the queue create; the trace then pins each edge:

| Edge | Enforced by |
| --- | --- |
| `engines → engine_fields → schemas → queues` | server (400) + trace |
| `workspaces → queues` | server + trace |
| `queues → inboxes / email_templates / saved_views` | server + trace |
| `labels → rules` | **trace only** |
| both hook POSTs → the `run_after` relink PATCH | **trace only** |

Then remote truth — `queue.engine` resolved to a real URL, post-validator's
`run_after` naming the validator, the rule's label ref resolved — and
`assert_converged`: a hand-written snapshot must deploy in **one** cycle.

Finally the graph is tombstoned and deleted with `sync test --allow-deletes`,
again under trace. Two things are asserted, and neither is a defect pin:

- **The DELETE sequence** for the kinds that can go — children before parents,
  `rules`/`saved_views`/`hooks`/`email_templates`/`inboxes` all before `queues`,
  and `queues` before `schemas` before `workspaces`.
- **Skip-and-continue against a real refusal.** The bound engine's DELETE is
  refused (`400 engine_attached_to_active_queues`), and the run must still
  delete every other object, warn, tally the failure, and **keep the engine's
  lockfile entry** so a later sync retries it. Nothing in the suite pins that
  contract against a real server today — the existing coverage for it is a
  unique-typed email template, which is refused *permanently* rather than
  temporarily. The engine and its field are then left for the janitor.

### 5. `scenarios/engines.rs` — `live_engines_round_trip`

Deliberately **never binds a queue**, so it cleans up completely and can run on
every live run without accumulating anything.

Seed an engine + two fields through the raw client → pull → assert the layout and
the compound `rdc://engine_fields/<engine>/<field>` ref → edit the engine
`description` and a field `label` locally → push → assert the remote → add a
field file → push creates it → remove a field file → `--allow-deletes` deletes it
→ delete the engine. `assert_converged` after every write.

### 6. Support plumbing

`LiveClient`'s kind map gains `engine` / `engine_field`. `teardown_by_prefix` and
the janitor sweep `engine_field` then `engine`, placed **after** queues and
schemas. The two halves have different reasons, and only one of them is about
ordering:

- The **field** sweep genuinely benefits: `409 Cannot delete engine field used in
  a schema` clears once the schema is gone, so running after the schema sweep
  turns a guaranteed failure into a likely success.
- The **engine** sweep cannot be helped by any ordering (see above), so it is
  best-effort: tolerate `engine_attached_to_active_queues` and
  `…waiting_for_deletion` by logging and leaving the object for a later run's
  janitor, exactly as `delete_schema_with_retry` already tolerates a draining
  queue — except the window is a day, not fifteen seconds, so there is no retry
  loop, just a log line.

## Testing

The two new scenarios are `#[ignore]` live tests like the rest. The support
modules (`trace.rs`, `snapshot.rs`) get hermetic unit tests that run under a
plain `cargo test`, matching the harness's existing split.

## Costs and risks

- **~1 orphan engine + 1 field per ordering run**, undeletable for up to 24h,
  swept by the janitor on a later run. Accepted deliberately: binding is what
  makes the server enforce the order, and an unbound engine would reduce the
  test to restating what the code does.
- Suite grows by roughly 60–90s (two more full sync cycles).
- `deploy_flow`'s manifest is left alone, so the 20 existing scenarios are
  unaffected.

## Cleanup debt

The probes behind this spec left two engines (`2199`, `2200`), two engine fields
and two schemas stranded behind draining queues in the sandbox. All are
`rdc-it-probe*`-named and within the janitor's reach once the queues purge.
