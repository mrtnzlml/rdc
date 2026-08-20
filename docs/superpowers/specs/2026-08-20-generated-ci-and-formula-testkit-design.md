# Env-driven GitLab CI + a distributed formula testkit

**Status:** design, awaiting review
**Date:** 2026-08-20

## Problem

Two connected gaps in what `rdc init` hands a new project.

**The pipeline does not describe the project.** `templates/gitlab-ci.yml` is a
fixed file, embedded with `include_str!` and written verbatim (R1). It archives
a placeholder env named `dev` and ships two placeholder deploy buttons
(`dev` → `test` → `prod`) with `# TODO` markers, whatever the project actually
defines. Every user edits the same three places by hand, and adding an env later
means remembering to edit them again — nothing tells them to.

**The pipeline cannot be green on a fresh project.** Its `pytest` job runs
`pip install -r requirements-dev.txt`, and `rdc init` scaffolds no such file
(R7). Fix that, and `pytest -q` still exits **5** because no tests were
collected (R8) — the job is red either way. Worse, there is no shared way to
test the Python that rdc snapshots. Schema formulas and hook code are the most
behaviour-carrying files in a project, and each project either reinvents a
harness or tests nothing.

**Goal.** Generate the env-shaped parts of the pipeline from `rdc.toml`, and
ship a formula/hook harness that runs the *real* txscript runtime, so a fresh
project's pipeline is green out of the box and its formulas are testable.

**Non-goal.** Modelling a promotion chain. `rdc.toml` gains nothing (D1).

## Verified facts

Nothing below is inferred. Runtime facts come from probes run 2026-08-20 against
txscript 1.1.0 and 1.2.0 installed from public PyPI; code facts cite the tree.

### txscript runtime

| # | Fact | Consequence |
|---|---|---|
| T1 | txscript is on **public** PyPI: `1.0.1` (2024-10-16), `1.1.0` (2025-05-28), `1.2.0` (2026-07-31). | CI needs no private index and no credentials to install it. |
| T2 | The candidate harness's 4 self-tests pass under `txscript==1.1.0` + `pytest 8.4.2`, run in a copy isolated from any project. | The starting point works, for exactly one runtime version. |
| T3 | 3 of those 4 **fail** under `1.2.0`: `TxScript.from_payload` dereferences `payload["document"]` unconditionally (`txscript/txscript.py:91`), and `Annotation.__init__` now reads `id`, `automated`, `automatically_rejected`, `einvoice`, `metadata` and nine `*_at` timestamps (`txscript/annotation.py`). | The `txscript==1.1.0` pin is load-bearing, not incidental. A distributed harness must not inherit it silently. |
| T4 | `Formula.evaluate()` returns an `EvalResult` (fields `value`, `options`, `struct`) in `1.2.0`; it returned the raw value in `1.1.0`. | Callers must unwrap. |
| T5 | **One** payload — carrying `document` plus the full annotation key set — together with `getattr(result, "value", result)`, passes on `1.1.0` *and* `1.2.0`. Verified by running the identical script under both interpreters. | Version tolerance costs one payload shape and one unwrap. No branching on version. |
| T6 | `Fields._readonly_context()` is unchanged between `1.1.0` and `1.2.0` (`txscript/fields.py`); `Formula.__init__` and `.dependencies` stay compatible. | The one private API the harness touches is stable across the versions we support. |
| T7 | `1.2.0`'s `Annotation` sets `self.id`; `1.1.0`'s does not. The live Rossum formula runtime **does** expose `annotation.id`. | `annotation.id` must be injected on `1.1.0` and left alone on `1.2.0`; `1.2.0` is the version closer to live. |
| T8 | A field's Python type comes from its schema node (`txscript/datapoint.py:120-147`): `number` → `NumberValue(float(v))`; `boolean` → `BooleanValue`; `date` → `datetime.strptime(v, "%Y-%m-%d").date()`, and **any parse failure yields `DateValue(None)`**; `enum` → `enum_value_type` (default `string`); otherwise `StringValue`. The value read is `normalized_value or value`. | Supplying the real schema is the whole fix for type fidelity. A date must arrive as ISO or it reads empty. |
| T9 | `Field.from_data` raises `AttributeError("Field '<id>' is not defined")` for a schema_id absent from the flat data. | A real tenant errors exactly where a synthesized schema invents the field. |
| T10 | Column formulas evaluate **per row**: `eval_strings` detects a `MultivalueDatapointField` parent, builds `Formula(schema_id, code, mv_schema_id)`, and evaluates inside `row._row_formula_context(t)` + `row._field_context(field)` (`txscript/eval.py:134-177`). | This is the only faithful way to evaluate a table-column formula, and it is three calls. |
| T11 | Driving txscript's own `eval_strings` over a real queue snapshot works: 19 formulas evaluated in real dependency order, per-row table columns computed, cross-formula dependencies resolved, 0 exceptions. | Recorded as proven-feasible; **out of scope** here (see Out of scope). |

### Fidelity of the all-string harness

The candidate harness synthesizes every field as `type: "string"`. Measured
consequences:

| # | Fact | Consequence |
|---|---|---|
| F1 | `field.amount * 2` with `"10"` returns `'1010'` (string repetition); `field.amount + 1` with `1.5` raises `TypeError`. Real number typing gives `20.0` / `2.5`. | Arithmetic on a number field is either wrong or impossible. |
| F2 | `sum(field.col.all_values)` raises `AttributeError`; `[row.x for row in field.table]` returns `[]`. | Tables are unreachable, and the second case is a **silent false green**. |
| F3 | An unknown keyword argument is materialized as an empty field rather than rejected. | A typo'd input, or a formula referencing a field since deleted from the schema, still passes. |
| F4 | Static scan of a real six-env project's suite: 6 files, 29 `evaluate_formula` call sites, 79 keyword-argument occurrences → **28 `number`, 28 `date`, 12 `string`, 11 `enum`** (two of them table columns). | Only 23 of 79 inputs are modelled faithfully today. |
| F5 | Every `enum` field in that project carries `enum_value_type: "string"` (654) or omits it (38). | Per T8, enums already behave correctly. No migration cost there. |
| F6 | A real numeric formula returns an identical result under both harnesses — the formulas defensively cast with `float(...)`. | In practice `number` inputs also survive the switch unchanged. |
| F7 | `date` is the real divergence. A date node's `format` (e.g. `"M/D/YYYY"`) is **display only**; T8 parses strictly `%Y-%m-%d`. An existing test in that project feeds a display-form date and asserts the same string back — behaviour the tenant does not have, since the tenant would read that field as empty. | The suite currently certifies a fiction for date fields. This is the defect the change exists to close. |
| F8 | **0** keyword arguments in that suite name a field absent from every `schema.json`. | Turning F3 into a hard error breaks nothing that exists today. |

### rdc

| # | Fact | Cite |
|---|---|---|
| R1 | `templates/gitlab-ci.yml` is embedded with `include_str!` and written verbatim; a test asserts byte-identity between the written file and the template. | `src/cli/init.rs:767`, `tests/cli_init.rs:668` |
| R2 | `ProjectConfig` is `{envs: BTreeMap<String, EnvConfig{api_base, org_id}>}` — no chain, no CI section, alphabetical iteration only. | `src/config/mod.rs:6-16` |
| R3 | `ProjectConfig::save` round-trips through the struct and drops any key this version doesn't model; init already refuses to save in regenerate-only mode for that reason. | `src/cli/init.rs:106-111` |
| R4 | `write_readme` already generates from `cfg.envs`, including a promote section gated on `len() >= 2`. | `src/cli/init.rs:659` |
| R5 | Scaffold contract: absent → `Created`; present without `--force` → `Unchanged`; present with `--force` and differing bytes → `Rewritten`; byte-equal → `Unchanged`. | `src/cli/init.rs:608` |
| R6 | `.gitignore` / `.gitattributes` append only their missing canonical lines instead of rewriting. | `src/cli/init.rs:507,581` |
| R7 | `rdc init` scaffolds no Python and no requirements file, so the template's `pip install -r requirements-dev.txt` fails on a fresh project. | `src/cli/init.rs:113-118` |
| R8 | `pytest -q` with zero tests collected exits **5**. Verified. | — |
| R9 | `parse_env_spec` performs **no** validation of the env name; only the interactive prompt restricts it to `[A-Za-z0-9_-]`. A hand-written `rdc.toml` can hold any name. | `src/cli/init.rs:426-455,483` |
| R10 | GitLab CI: "You can't use YAML anchors across multiple files when using the `include` keyword. Anchors are only valid in the file they were defined in." | GitLab docs, *YAML optimization* |
| R11 | `secrets::env_var_for` uppercases ASCII alphanumerics and maps every other character to `_` (`dev-us` → `RDC_TOKEN_DEV_US`). The shell equivalent (`tr -c '[:alnum:]' '_'`) is locale-dependent for non-ASCII. | `src/secrets.rs:103-116` |
| R12 | rdc's GitHub CI has no test job — only tag-triggered release builds and a manual desktop probe. | `.github/workflows/` |
| R13 | rdc supports per-env hook secrets at `secrets/<env>.hook-secrets.json`, which the shipped template never materializes. | `src/secrets.rs:435` |

## Decisions

| # | Decision | Because |
|---|---|---|
| D1 | **No promotion chain anywhere.** Deploy jobs are *drafted*, not derived. `rdc.toml` is untouched. | Sidesteps R3 entirely, and nothing has to guess. |
| D2 | **One file with named marker regions**, not split `include:` files. | R10 — splitting would break every `&anchor` in the template and force a rewrite to `!reference`. |
| D3 | Deploy drafts leave `RDC_SRC` **empty** with a fail-fast guard, rather than guessing from env names. | A plausible-but-wrong source feeding `migrate --mirror` + `sync --allow-deletes` is worse than a blank. |
| D4 | Archive covers **every** env and self-skips when that env's credentials are absent. | A project that wired two of six envs still gets a green scheduled pipeline. |
| D5 | The testkit is **scaffolded from embedded templates**, not published to PyPI. | Version-locked to the binary, consistent with `templates/gitlab-ci.yml`, no new release channel, works with a private repo. |
| D6 | The harness reads the queue's real `schema.json` when one is adjacent, and falls back to the synthesized all-string schema when not. | F1–F7 for real formulas; the fallback keeps `tmp_path` tests and any existing suite working. |
| D7 | One code path supports txscript `1.1.0` and `1.2.0`; `requirements-dev.txt` pins `1.2.0`. | T5 makes it free; T7 makes `1.2.0` the version closer to live. |
| D8 | The generator emits credential-variable suffixes computed in Rust by `env_var_for`, rather than re-deriving them in shell. | R11 — the shell form is locale-dependent, and the generator already knows the name. |

## Design

### A. Two named regions in `templates/gitlab-ci.yml`

The template stays a complete, readable, embedded pipeline. Two regions inside
it become generated; everything else is static and user-owned.

**Region `rdc:archive-envs`**, inside the archive job's matrix. Each entry pairs
the env with its Rust-computed credential suffix (D8):

```yaml
  parallel:
    matrix:
      # >>> rdc:archive-envs  (generated from rdc.toml — `rdc init --force` to refresh)
      - RDC_ENV: "dev"
        RDC_VAR_SUFFIX: "DEV"
      - RDC_ENV: "test"
        RDC_VAR_SUFFIX: "TEST"
      - RDC_ENV: "prod"
        RDC_VAR_SUFFIX: "PROD"
      # <<< rdc:archive-envs
```

A `parallel:matrix` entry with single-valued keys generates exactly one job, so
this is one archive job per env.

**Region `rdc:deploy-jobs`**, at the end of the file:

```yaml
# >>> rdc:deploy-jobs  (generated from rdc.toml — `rdc init --force` to refresh)
# DRAFTS. Each button needs its RDC_SRC filled in before it can be pressed, and
# the jobs for hand-authored source envs (a dev env people edit) should be
# deleted -- an archive records those, a deploy would overwrite them.
"deploy:test":
  extends: .rdc-deploy
  resource_group: "test"
  environment:
    name: "test"
  variables:
    RDC_ENV: "test"
    RDC_SRC: ""   # TODO: env to promote from
# <<< rdc:deploy-jobs
```

The snippet shows one draft of three: the committed template's regions hold
the canonical `dev` / `test` / `prod` example in full, which is what the
generator-accuracy test in F pins them to. A draft for the source env
(`deploy:dev`) is emitted like any other and deleted by the reader — the
region's comment says so, and D3's guard means an undeleted one cannot fire.

Env names and job names are **always quoted**: per R9 an env name can be
anything a hand-written `rdc.toml` holds, and an unquoted `deploy:my env` or
`[dev, test]` flow sequence would be invalid or mis-parsed YAML.

Both regions emit envs in `BTreeMap` order (alphabetical, R2), so a given
`rdc.toml` renders byte-identically on every run.

Drafts are emitted only when the project has **≥2 envs**, mirroring R4's
existing rule for the README's promote section. A single-env project gets the
region with a comment explaining that promotion needs a second env.

### B. Static template changes

Three edits outside the regions, which is what makes A safe and the pipeline
green out of the box.

1. **`.rdc-deploy` fail-fast guard.** Its `script` opens with:

   ```sh
   : "${RDC_SRC:?this deploy button is still a draft -- set RDC_SRC to the env to promote from}"
   ```

   `${VAR:?}` fires on unset *or* empty, so an unfinished draft dies before
   `rdc migrate` (offline anyway) and long before `rdc sync` can write.

2. **Archive credential skip.** Before the `rdc sync`, using the suffix from A:

   ```sh
   if [ -z "$(printenv "RDC_TOKEN_$RDC_VAR_SUFFIX" || true)" ] \
   && [ -z "$(printenv "RDC_PASS_$RDC_VAR_SUFFIX" || true)" ]; then
     echo "$RDC_ENV: no RDC_TOKEN_$RDC_VAR_SUFFIX or RDC_PASS_$RDC_VAR_SUFFIX; skipping."
     exit 0
   fi
   ```

   GitLab concatenates a job's `script` entries into one shell script, so
   `exit 0` skips the remainder and the job is green.

3. **`pytest` job stops being an example.** It keeps `pip install --no-cache-dir
   -r requirements-dev.txt` and `pytest -q`, both of which now work because D5
   scaffolds the requirements file and the testkit's own self-tests guarantee a
   non-empty collection (R8).

### C. Generator and splicer — `src/cli/gitlab_ci.rs`

A new module rather than more of `init.rs`, which is already 1052 lines. Pure
functions, unit-testable without a filesystem:

```rust
/// Region name -> rendered body (no marker lines, no trailing newline).
fn render_regions(envs: &BTreeMap<String, EnvConfig>) -> BTreeMap<&'static str, String>;

/// Replace each region's body in `existing`. `Ok(None)` when the file carries
/// no rdc markers at all. Errors on malformed markers; never partially writes.
fn splice(existing: &str, regions: &BTreeMap<&str, String>) -> Result<Option<String>>;

/// The create / `--force` path: splice into the embedded template.
fn generate(template: &str, envs: &BTreeMap<String, EnvConfig>) -> Result<String>;
```

`write_gitlab_ci(root, cfg, force)` — it takes `&ProjectConfig` now, exactly as
`write_readme` already does (R4):

| File state | Action | Reported |
|---|---|---|
| absent | write `generate(TEMPLATE, envs)` | `Created` |
| has markers | splice; bytes unchanged | `Unchanged` |
| has markers | splice; bytes differ | `Merged` ("updated") |
| no markers, no `--force` | leave alone | `Unchanged` |
| no markers, `--force` | regenerate whole file | `Rewritten` |

The `Merged` label already means "rdc-owned lines refreshed, user lines kept"
for `.gitignore` (R6), which is precisely the semantics of a splice.

**Splicing runs on `rdc init --env <new>` too.** Adding an env adds its archive
entry and drafts its button, touching nothing outside the regions. That is the
whole reason for markers over whole-file regeneration.

Region body indentation follows the indentation of its `>>>` marker line, so the
archive region stays valid YAML inside the matrix list.

**Malformed markers are a hard error** naming the file and the region — a `>>>`
with no `<<<`, a `<<<` before its `>>>`, or a duplicated region. The file is
never written in that case; a half-spliced pipeline is worse than a diagnosed
one.

### D. Scaffolded Python

New embedded templates, written under the R5 contract and listed in the
`--force` summary:

| Written to | From |
|---|---|
| `testkit/__init__.py` | `templates/testkit/__init__.py` |
| `testkit/txscript_eval.py` | `templates/testkit/txscript_eval.py` |
| `testkit/test_txscript_eval.py` | `templates/testkit/test_txscript_eval.py` |
| `conftest.py` | `templates/conftest.py` |
| `pytest.ini` | `templates/pytest.ini` |
| `requirements-dev.txt` | `templates/requirements-dev.txt` |

`txscript_eval.py` keeps its name so a project already carrying this harness
adopts the shipped one by deleting its copy.

`conftest.py` puts the project root on `sys.path` so `from testkit import ...`
resolves regardless of where pytest is invoked. `pytest.ini` sets
`testpaths = envs testkit` and `python_files = test_*.py`. `requirements-dev.txt`
pins `pytest>=8,<9` and `txscript==1.2.0`, with a comment recording that the
harness also supports `1.1.0` (T5) and why `1.2.0` is the default (T7).

`.gitignore` gains `__pycache__/` and `/.pytest_cache` through the existing
additive merge (R6).

`write_scaffold_files` — the entry point the desktop app uses to make a folder
look init-produced — writes the new files too. Its signature is unchanged.

### E. Harness behaviour

**Public surface is unchanged**, so existing tests keep compiling:

```python
evaluate_formula(formula_path, *, annotation_id=1, rows=None, **field_values)
load_hook(hook_path)
```

**Schema discovery.** `formula_path.parents[1] / "schema.json"` — in rdc's layout
a formula lives at `<queue>/formulas/<field_id>.py`, so the queue's schema is one
level up. Found → real types, real tree, strict checking. Absent → today's
synthesized all-string schema (D6), which is what keeps `tmp_path` tests and
pre-existing suites working.

**Version tolerance (T5).** The payload always carries a `document` object and
the full annotation key set. `annotation.id` is injected only when the runtime
did not already set it (T7). The result is unwrapped with
`getattr(result, "value", result)` (T4).

**Values** are written to `content.value` with `normalized_value: None`, so
txscript's `normalized_value or value` (T8) reads what the caller passed. The
caller therefore supplies the *normalized* form — ISO `YYYY-MM-DD` for dates —
which is exactly the correction F7 buys. A `datetime.date` is accepted and
serialized to ISO; anything a date field cannot parse reads empty, as it does in
the tenant.

**Column formulas** go through txscript's own row path (T10): detect a
`MultivalueDatapointField` parent, build `Formula(schema_id, source,
mv_schema_id)`, and evaluate inside `row._row_formula_context(t)` +
`row._field_context(field)`. Rows come from `rows={"line_items": [{...}, {...}]}`;
for a column formula under test, bare column keyword arguments are routed into a
single implicit row, so the common one-row case needs no `rows=`.

**Strictness (F3, F8).** With a real schema, a keyword argument naming no field
in it, or a formula dependency absent from it, raises `AssertionError` naming the
field. Under the fallback schema, neither check is possible and neither runs.

**`load_hook`** is unchanged — it is version-independent and already correct.

**Self-tests** keep the existing 4 and add schema-aware ones built on a small
fixture `schema.json` written to `tmp_path`: number arithmetic, ISO date
round-trip, a date that fails to parse reading empty, enum-as-string, a
two-row table column, an unknown keyword argument, and a formula referencing an
absent field. Beyond covering the new behaviour these guarantee a non-empty
collection, which is what makes a fresh project's `pytest -q` exit 0 (R8).

### F. Testing

**`src/cli/gitlab_ci.rs` unit tests.** Region rendering for 1 / 2 / 6 envs;
indentation preserved; awkward env names quoted (per R9: a space, a `:`, a `#`);
malformed markers rejected with no write; a markerless file returning `None`; and
a hand-edited job *outside* the regions surviving a splice byte-for-byte.

**`tests/cli_init.rs`.** The R1 byte-identity assertion is replaced by an
equivalent that survives generation, in two halves:

1. everything outside the markers in the written file is byte-identical to the
   template, and
2. the committed template's own region bodies equal what `render_regions`
   produces for the canonical `dev` / `test` / `prod` example.

Together these preserve the "the file users read on GitHub and the file the
binary writes cannot drift" guarantee that R1 exists to enforce — and add a new
one, that the committed example is itself generator-accurate. Plus: each
scaffolded Python file byte-identical to its template; the archive region listing
exactly the project's envs; the draft count matching; `init --env` on an existing
markered file updating only the regions; `init --env` on a markerless file
changing nothing.

**One `#[test]` runs the shipped testkit.** It writes the embedded templates to a
temp dir and runs `python3 -m pytest -q`, asserting success. It **skips** (does
not fail) when `python3` or `txscript` is unavailable. Per R12 rdc has no CI test
job at all, so this guards `cargo test` on a developer machine rather than
inventing a CI dependency the project doesn't have.

**CLAUDE.md** is updated: the "edit the template, never a copy — tests compare
the two byte-for-byte" rule becomes the two-half rule above, and the
`RDC_VERSION`-stays-pinned rule is untouched.

## Backward compatibility

- **Existing projects' pipelines are untouched.** Their `.gitlab-ci.yml` has no
  markers, so it falls into the "no markers" rows of C: `Unchanged` without
  `--force`, whole-file `Rewritten` with it — exactly today's contract (R5).
  Adopting generation is opt-in: paste the two marker pairs, or take a fresh
  file from `--force`.
- **`rdc.toml` format is unchanged.** No new keys, so R3's round-trip hazard is
  not touched.
- **Existing testkit users** keep their call sites: the signature is unchanged
  and the no-schema fallback preserves current behaviour for formulas with no
  adjacent schema. For formulas that *do* have one, the ~28 `date`-typed inputs
  measured in F4 must become ISO and the assertions that expected a display
  string back must change — about two files, and per F7 that is the defect being
  fixed, not collateral. `string`, `enum` (F5) and `number` (F6) call sites are
  unaffected, and F8 says the new strictness breaks nothing.
- **`write_scaffold_files`** keeps its signature; embedders get the new files
  automatically.

## Failure modes

| Situation | Behaviour |
|---|---|
| Unterminated / reversed / duplicated marker | Hard error naming file + region. Nothing written. |
| A deploy draft is pressed with `RDC_SRC` unset | Dies in `before_script` (B1). No tenant write, no repo write. |
| An archived env has no credentials | That matrix job prints a skip line and exits 0 (B2). Other envs still archive. |
| A project has one env | Archive covers it; `rdc:deploy-jobs` is an explanatory comment. |
| `python3` or `txscript` missing when `cargo test` runs | The testkit test skips with a printed reason. |
| Formula has no adjacent `schema.json` | Fallback all-string schema; no strictness. Documented in the harness docstring. |
| A `date` input is not ISO | Reads empty, as the tenant does (T8). |
| txscript releases a `1.3.0` that breaks again | `requirements-dev.txt` pins `1.2.0`, so nothing moves until someone bumps it. The harness's own self-tests are the detector. |

## Out of scope

Each of these is a real gap; none is this change.

- **Hook-secret materialization** (`RDC_HOOK_SECRETS_<ENV>` →
  `secrets/<env>.hook-secrets.json`, R13) and a **backport job**. Both are
  static template work, orthogonal to env-driven generation. D8's
  `RDC_VAR_SUFFIX` is the piece the first one would need, and it lands here.
- **`evaluate_queue()` on `eval_strings`** (T11) — proven feasible, materially
  higher fidelity, and a separate change with its own test surface.
- **Publishing the testkit to PyPI** (D5).
- **A promotion chain in `rdc.toml`** (D1).
- **A GitHub Actions test job for rdc itself** (R12) — worth doing, unrelated.
