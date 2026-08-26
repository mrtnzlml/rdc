# Measurement runbook: sync-parallelization before/after

Offline half (Task 13, this document) built both binaries and verified the
tree. This runbook is what the live half runs against a real Rossum org to
fill in the spec's Expected results table
(`docs/superpowers/specs/2026-08-24-sync-parallelization-design.md`). It does
**not** touch the org itself — follow it with the token and project the
person running it already has.

## 0. Binaries

This runbook is self-contained: it does not assume any binary from a prior
session still exists (a session scratchpad does not survive past its
session) and it does not pin `after` to a specific commit (a moving target —
`main` keeps advancing, so pinning one here would go stale the moment it
does). Build both fresh, on one machine, one toolchain, same `rdc 0.7.0`
`Cargo.toml` version (the plan didn't bump it, so the two binaries are told
apart by which `$BEFORE`/`$AFTER` variable you invoke, not by `--version`
output — both report `rdc 0.7.0`).

`after` — **current `main`**, built in place in the shared repo checkout:

```bash
cd <path to the rdc repo checkout>
cargo build -p rdc --release --locked
AFTER=$(pwd)/target/release/rdc
```

`before` — this plan's pre-Task-1 base (`05678ee`), built in an isolated
`git worktree` so it never touches the shared main tree's working directory
or `target/`:

```bash
BEFORE_WORKTREE=$(mktemp -d)
git worktree add "$BEFORE_WORKTREE" 05678ee
(cd "$BEFORE_WORKTREE" && cargo build -p rdc --release --locked)
BEFORE="$BEFORE_WORKTREE/target/release/rdc"
```

Confirm the shell variables for the rest of this runbook:

```bash
echo "$BEFORE"
echo "$AFTER"
```

When the runbook is done, remove the worktree so it doesn't linger in the
shared tree's `git worktree list`:

```bash
git worktree remove "$BEFORE_WORKTREE"
```

## 1. Before you time anything: check the build lock

This machine is shared. If another `cargo` process (another worker's build
or test run) is active, wall-clock numbers from this run are meaningless —
CPU/IO contention, not the code, will dominate the delta.

```bash
pgrep -fl cargo
```

If that prints anything, wait until it's empty before measuring. Re-check
before each of the three repeats below, not just once at the start.

## 2. The two required commands

Run from the project directory you normally `rdc sync` (the one with
`rdc.toml`), against a steady-state env — i.e. `git status` on the project
clean, no pending local edits, and the env itself has no real drift to pull.
`<env>` below is whatever env name that project uses.

For **each** binary (`$BEFORE`, then `$AFTER`) and **each** command
(`sync <env> --no-push`, `sync <env> --dry-run`), run **three times**, each
with `RDC_TRACE_HTTP` pointed at its own scratch file so runs never clobber
each other's trace:

```bash
TRACE_DIR=$(mktemp -d)
BIN=$BEFORE   # then repeat the block with BIN=$AFTER

for i in 1 2 3; do
  RDC_TRACE_HTTP="$TRACE_DIR/no-push-$i.csv" \
    /usr/bin/time -p "$BIN" sync <env> --no-push
done

for i in 1 2 3; do
  RDC_TRACE_HTTP="$TRACE_DIR/dry-run-$i.csv" \
    /usr/bin/time -p "$BIN" sync <env> --dry-run
done
```

`/usr/bin/time -p` prints `real`/`user`/`sys` to stderr after the command
exits. Take the **median** of the three `real` values per command — sort the
three and keep the middle one, don't average:

```bash
printf '%s\n' <real1> <real2> <real3> | sort -n | sed -n 2p
```

After each `--no-push` run, confirm the project tree is still clean
(`git status --short` in the project dir). A steady-state pull should write
nothing; if it's not clean, either the env has real drift (invalidating
"steady state") or something regressed — note it, don't paper over it by
resetting and re-running silently.

That's 4 binaries × commands... i.e. 2 binaries × 2 commands × 3 runs = 12
timed invocations, 12 trace CSVs.

## 3. Trace CSV format

One line per HTTP **attempt** (retries included), both the core client and
Data Storage funnel through the same tracer
(`src/api/retry.rs`, `mod trace`):

```
epoch_ms,limiter_wait_ms,duration_ms,status,desc
```

`desc` is **last and unquoted** — it's `"<METHOD> <url>"` and the URL can
itself contain commas (query strings), so always split on the first four
commas, not on every comma. `cut -d, -f5-` does this correctly (it
reconstructs everything from the 5th field onward using the original
delimiter, which reproduces `desc` verbatim including any embedded commas).
A Data Storage call is always `POST` and its URL contains
`/svc/data-storage/`; everything else is a core Rossum API call.

## 4. Computing the figures, per CSV

Run these against each of the 12 trace files. `$F` is the CSV path.

**Total requests:**

```bash
wc -l < "$F" | tr -d ' '
```

**Split core vs Data Storage:**

```bash
ds=$(cut -d, -f5- "$F" | grep -c '/svc/data-storage/')
total=$(wc -l < "$F" | tr -d ' ')
core=$((total - ds))
echo "core=$core ds=$ds total=$total"
```

**Achieved req/s, whole run:**

```bash
awk -F, 'NR==1{first=$1} {last=$1; n++} END{
  dur=(last-first)/1000
  if (dur>0) printf "%.2f req/s (%d requests, %.2fs span)\n", n/dur, n, dur
  else print "single request or zero span, rate undefined"
}' "$F"
```

**Achieved req/s per phase (core-only and Data-Storage-only subsets)** — the
rate a subset of requests achieved on its own timeline, regardless of
whether it overlapped the other subset:

```bash
awk -F, '{
  desc=$5; for (i=6;i<=NF;i++) desc=desc","$i
  is_ds = (desc ~ /\/svc\/data-storage\//)
  if (is_ds) { if (!dsN++) dsFirst=$1; dsLast=$1 }
  else       { if (!coreN++) coreFirst=$1; coreLast=$1 }
}
END {
  if (coreN>0) { d=(coreLast-coreFirst)/1000; printf "core: %d req", coreN
    if (d>0) printf ", %.2f req/s (%.2fs)", coreN/d, d; print "" }
  if (dsN>0)   { d=(dsLast-dsFirst)/1000; printf "data-storage: %d req", dsN
    if (d>0) printf ", %.2f req/s (%.2fs)", dsN/d, d; print "" }
}' "$F"
```

**Total `limiter_wait_ms`, overall and split:**

```bash
awk -F, '{
  desc=$5; for (i=6;i<=NF;i++) desc=desc","$i
  is_ds = (desc ~ /\/svc\/data-storage\//)
  tot += $2
  if (is_ds) ds += $2; else core += $2
}
END { printf "total=%.1fms core=%.1fms ds=%.1fms\n", tot, core, ds }' "$F"
```

B6 (the pre-plan baseline) measured **zero** blocked time on every traced
run. If `ds` here is now non-zero and large, that's D1's 30/s Data Storage
bucket constant to revisit — not the fan-out logic. A non-zero `core` figure
would point at the existing 10/s core bucket instead, which this plan didn't
touch.

Python3 equivalent for anyone who prefers it over awk (same split-on-first-4-commas rule):

```python3
import sys
rows = []
for line in open(sys.argv[1]):
    epoch_ms, wait_ms, dur_ms, status, desc = line.rstrip("\n").split(",", 4)
    rows.append((float(epoch_ms), float(wait_ms), desc))
total = len(rows)
ds = [r for r in rows if "/svc/data-storage/" in r[2]]
core = [r for r in rows if "/svc/data-storage/" not in r[2]]
print(f"total={total} core={len(core)} ds={len(ds)}")
print(f"limiter_wait_ms total={sum(r[1] for r in rows):.1f} "
      f"core={sum(r[1] for r in core):.1f} ds={sum(r[1] for r in ds):.1f}")
for name, subset in [("core", core), ("data-storage", ds)]:
    if len(subset) > 1:
        span = (subset[-1][0] - subset[0][0]) / 1000
        if span > 0:
            print(f"{name}: {len(subset)} req, {len(subset)/span:.2f} req/s ({span:.2f}s)")
```

## 5. The invariant that matters most

Per-command **request counts must be identical** between `$BEFORE` and
`$AFTER` — this plan is a scheduling change, not a request-shape change.
Recorded baseline (pre-plan, B1/B2 in the spec):

- steady `sync --no-push`: **58 requests** (15 core, 43 Data Storage)
- steady `sync --dry-run`: **same 58**

Compare the `total`/`core`/`ds` triple from step 4 across all six
`no-push` CSVs (3 before + 3 after) and all six `dry-run` CSVs. They should
all read identically — the whole point of D11's trace is to make a
divergence here visible instead of guessed at. If `$AFTER` differs from
`$BEFORE` for the same command, that is a bug in Task 7's prefetch scoping
or Task 12's schema dedup, not measurement noise — do not average it away or
report a range; chase it before publishing anything.

## 6. What's still projected, not measured, after this runbook is run

Even a full run of this runbook does not close two known gaps — record them
in the Expected results table's notes, don't bury them in a footnote:

- **D8 (MDH row-pull fan-out) stays projected.** The measurement org has no
  manual datasets, so `fetch_dataset_rows`'s concurrent-fetch path never
  executes against real data in this measurement. It's designed and unit
  tested, not wall-clock verified.
- **The dry-run row forecast stays sequential.** Task 8 deliberately left the
  dry-run index-edit forecast (`plan_mdh_index_edits`) walking datasets one
  at a time rather than fanning out: giving it the rate-limiter guardrail
  would add a request per dataset just to reserve a slot, and giving it a
  guardrail-free variant would fork the helper in two. Recorded as a known
  remaining gap, not silently absorbed into the measured dry-run number.

## 7. Optional: the other three Expected-results rows

The spec's table has five rows; steps 2–5 above cover the two "steady"
rows only, since those are the ones with a clean before/after request-count
invariant to check and don't require setting up specific edit state. The
other three rows (push 14 hooks, push 40 rules, first full pull) are part of
the same table and use the same trace/median procedure — included here for
completeness, not required by the offline half of Task 13:

- **Push 40 rule edits (B4)** and **push 14 hook edits (B5)**: reproduce the
  same edit shapes used for the original baseline (40 rule edits, 14 hook
  edits) in the project's local snapshot, then trace `sync <env>` (a real
  push, not `--dry-run`/`--no-push`). From the CSV, take the span from the
  first to the last `PATCH` line specifically (filter `desc` for `^PATCH `)
  and compute req/s over that span the same way as step 4's phase formula.
- **First full pull (B3)**: trace an `rdc sync <env>` against a project with
  no local snapshot yet (fresh `rdc init` + `sync`). D6 prefetches nothing on
  a tree with no history, so this row is expected to move the least.

## 8. Reporting

For each of the two required commands, report: the three `real` times per
binary, the median, the before→after ratio (do not round — e.g. report
"1.4×", never round up to "roughly 2×" unless the number actually is one),
the request-count triple (total/core/ds) for both binaries confirming the
invariant, achieved req/s per phase, and total `limiter_wait_ms` split
core/ds. Flag anything that regressed. State plainly which of the five
Expected-results rows were measured under this runbook and which (D8, and
any of section 7's rows if skipped) remain projected.
