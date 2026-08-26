# Sync parallelization — what is left

Companion to `2026-08-24-sync-parallelization.md`. Everything below was found during
execution or by the final whole-branch review, triaged, and deliberately **not** fixed.
None of it blocks keeping the work; all of it is real.

## Not yet measured — do this first

The design spec's **Expected results** table still says "projected, not yet measured", and
that is accurate. Nothing in this change set has been timed against a real organization,
and the central safety property — that per-command HTTP request counts are unchanged on
the success path — has been verified only by mocked per-driver tests, never end to end.

`2026-08-24-sync-parallelization-measurement-runbook.md` is the procedure. Run it before
quoting any figure from the spec, and treat the first live `sync` against a real project as
the actual verification rather than a formality. The recorded pre-change baseline to compare
against is **58 requests** for a steady `sync --no-push` (15 core, 43 Data Storage) and the
same 58 for `sync --dry-run`.

Two things are labelled projected on purpose and stay that way until someone has a manual
dataset to measure: the **MDH row-pull fan-out**, and the **dry-run row forecast**, which
remains deliberately sequential because batching it would either add a request per dataset
or fork the shared helper.

## Known, accepted, and documented in the code

- **The push error path is wider than the sequential loop's.** `prepare_all` collects the
  whole stream, so once a batch starts, a failure does not stop it — every remaining item is
  still prepared and sent. Accepted: no completed PATCH is left unrecorded, and one bad local
  file no longer blocks every other edit in the push.
- **The read-only stop widens the *success* path too.** `engine_fields` / `engines` map a
  405 to a read-only outcome after the run has already been dispatched, so N rejected PATCHes
  are issued where the old `break` issued one. Bounded and harmless; documented at both sites.
- **MDH row pulls hold every manual dataset's rows in memory at once.** Peak memory scales
  with the manual-dataset count rather than being capped at one. This is the only place in
  the change set where the fan-out has an unbounded resource cost. Manual datasets are opt-in
  and rare today; if that changes, chunk the fetch.
- **The MDH index prefetch is gated on the cycle performing no writes.** A writing cycle
  forfeits the overlap — including a cycle that pushes one unrelated object. Finer
  granularity would need to know at listing time which datasets the push will touch, which
  the MDH classifier bypass hides.

## The next refactor, if there is one

`Prepared::NeedsPrompt` carries only a slug, so nothing prepared can travel to the prompt
path. Two consequences, both live in the tree:

- each driver duplicates its `read → resolve_value → deserialize` prologue between the
  concurrent stage and `push_one_drifted` (~20 lines × 10 drivers);
- `workspaces` / `inboxes` park the drift body in a batch-local `Mutex<HashMap>` instead.

**Two independent reviewers reached this same pressure point**, one predicting it two tasks
before it bit. A `NeedsPrompt { slug, prepared: T }` variant is the fix. It was declined
during execution because it changes the shared primitive that all ten drivers use, and doing
so at the end would have forced re-verification of every settled driver.

If it is ever done: `prepare_all` has **no `Send` bound on `Fut`**, so a future edit holding
that mutex guard across an `.await` compiles today. Adding the bound turns a comment into a
compile error and is worth doing at the same time.

## Deliberately declined

**A context struct for `push_update_batch`'s 8–11 arguments.** The identical argument prefix
across ten drivers is what makes them comparable by eye, and that comparability is what
caught several defects during execution — the final reviewer used it to confirm in a single
pass that all ten flush their trailing batch, share one predicate between both guards, and
route apply errors through the same accumulator, and that exactly four deviate for stated
reasons. A context struct would have hidden those four. If it is ever introduced, do all ten
at once.

## Smaller items

- Test coverage is uneven: `email_templates` and `queues` / `engines` / `engine_fields` lack
  a barrier or zero-request pin that `rules`, `hooks` and `labels` have.
- `tests/cli_sync.rs`'s `ds_index < 6` assertion is coupled to `PULL_FANOUT = 5` — commented,
  not decoupled.
- The same 11-endpoint core-list array is inlined by two pre-existing helpers in
  `tests/cli_sync.rs` besides the shared const this work introduced.
- `schemas` prefetches drift GETs before reading local files, so a run containing an
  unparseable local file issues GETs where the old loop bailed first. Error path only.
- One commit body (`85da5a1`) claims the 405 path leaves "the transcript and the stop
  unchanged". It slightly overstates: a later skipped item in the same run still emits its
  skip line. Not amended — this tree is shared, so its history is not rewritten.
- `cargo clippy -- -D warnings` turns this repo's three pre-existing baseline errors into
  hard build failures, so `--all-targets` never reaches the bin or integration-test targets.
  Add `-A clippy::collapsible_if -A clippy::field_reassign_with_default` to lint everything.
  With those allowed, the tree is clean.
