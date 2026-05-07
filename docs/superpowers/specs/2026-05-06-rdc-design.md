# rdc — Rossum Deployment as Code (design)

**Status:** draft for review
**Date:** 2026-05-06
**Author:** brainstormed with Claude (Opus 4.7)

## 1. Motivation

Rossum already has `prd2`, a Python CLI that pulls/pushes/deploys configurations across environments. It works, but two needs are unmet:

1. **AI-friendly local representation.** A snapshot of a real implementation runs to thousands of files. Claude (and humans) struggle to navigate it: numeric IDs in filenames, schema fields buried in deeply nested arrays, hook code intertwined with config JSON, no cross-references, no inventory.
2. **Predictable, intelligent conflict handling.** Today's three-way merge is coarse-grained. Real conflicts (same field changed both sides) are mixed with noise (timestamp shuffles, key reordering, server-set defaults). Users learn to fear `pull` and `deploy` because outcomes feel random.

`rdc` is a new tool, written from scratch, optimized for these two needs. It is not a fork of `prd2` — it shares no code, uses a different on-disk format, and ships as a single static Rust binary so the install is trivial.

## 2. Goals

- **Snapshot per environment.** Each Rossum environment (DEV, TEST, PROD, etc.) is a self-contained, declarative snapshot on disk. Reading the snapshot tells you everything about that environment's configuration.
- **Round-trip without loss.** Pull → edit → push preserves every byte the user did not intend to change. Unknown fields survive. Server-set fields are not user concerns.
- **Field-level, semantic, three-way merge.** Conflicts are real conflicts, not artifacts of representation.
- **Plan before apply.** Every operation that modifies a remote shows the user exactly what will change before it changes.
- **Trivial install.** Single binary, distributed via Homebrew tap, GitHub releases, and `cargo install`.
- **Rossum implementation as code.** Every config-bearing object in Rossum is declarative in the snapshot. Code is editable in `.py` files. Schemas, queues, hooks, rules, labels, engines, workflows, email templates, MDH dataset metadata + indexes — all versioned.

## 3. Non-goals

- Pulling MDH dataset rows (data, not config).
- Pulling annotation/document data.
- Real-time sync. The model is explicit pull/push, not a daemon.
- Sub-org-level multi-tenancy beyond what Rossum's API exposes.
- Workflow GUI / web UI. CLI only in v1.
- Plugin system. v1 is monolithic.

## 4. Design principles

These are operating constraints, not aspirations. Every implementation decision is checked against them.

### 4.1 Predictability — "it just works"

- **Determinism.** Pulling the same remote state twice produces byte-identical files. Pushing the same local state twice is a no-op on the second run.
- **Idempotence.** Every command can be re-run safely. Partial failures are recoverable by re-running.
- **Atomicity.** All disk writes use temp-file + rename. No half-written state on Ctrl-C, disk-full, or panic. State files have checksums.
- **No hidden mutations.** A command that the user did not invoke does not modify the snapshot. `pull` writes; `push` writes (lockfile only); `plan` is read-only; `apply` writes remote.
- **Plan before apply.** Mutating remote always shows a plan first. `apply` re-validates the plan before executing — if reality drifted, it aborts and asks the user to re-plan.
- **Versioned state.** Lockfiles, mapping files, and overlays carry a `version` field with a forward-compat policy.

### 4.2 UX is first-class

- **Errors are actionable.** Every error names what failed, why, and the next concrete action. No raw stack traces in default output.
- **Defaults are good.** `rdc pull dev` Just Works without flags.
- **Progress is visible.** Long operations show progress; nothing hangs silently.
- **Output is grep-able.** TTY output is colorized; piped output is plain text. `--json` for machine consumption.
- **Help is complete.** `--help` is exhaustive; man pages installed by Homebrew.
- **Confirmation only when warranted.** Routine work runs without prompts. Mutating remote, deleting local, overwriting changes — these prompt.

### 4.3 AI-friendly snapshot

- **Slug-based filenames.** Numeric IDs do not appear in paths. Slugs are derived from object names; ID↔slug map persisted in the lockfile.
- **Generated indexes.** `_index.md` per directory: inventory, descriptions, cross-references ("hook X attached to queues Y, Z").
- **Co-located code.** Hook Python lives next to its config. Formula Python lives next to its schema. Editing the right file is obvious.
- **Stable layout.** Snapshot structure is documented and small. Cross-environment, cross-customer layouts are identical.

### 4.4 Declarative

- **Snapshot is the spec.** The local snapshot describes the desired state of an environment. The tool reconciles remote to match (push) or local to match (pull).
- **Overlays declare intentional divergence.** Env-specific values (secrets, names, URLs) live in `overlay.toml`, not in the snapshot. Overlays are never propagated by deploy.

## 5. Architecture

### 5.1 Workspace layout

```
my-rossum-project/
  rdc.toml                           # project config: envs, URLs, slugs policy
  .gitignore                         # generated; ignores .rdc/, secrets/, overlay values
  .rdc/                              # tool-managed state
    state/
      dev.lock.json                  # last-pulled snapshot per object + slug↔ID map
      test.lock.json
      prod.lock.json
    map/
      test→prod.toml                 # persisted env-pair mapping
      dev→test.toml
    cache/                           # transient caches
  envs/
    dev/                             # one self-contained env snapshot
      _index.md                      # generated TOC
      organization.json
      overlay.toml                   # legitimate env-specific overrides
      hooks/
        validator-invoices.json
        validator-invoices.py        # extracted hook code
      workspaces/
        dev-workspace/
          workspace.json
          queues/
            cost-invoices/
              queue.json
              schema.json
              inbox.json
              formulas/
                amount_total.py      # extracted formula code
      rules/
        e-invoice-validation.json
      labels/
        priority-high.json
      engines/
        invoice-engine.json
      engine-fields/
        amount-total.json
      workflows/
        approval-flow.json
      workflow-steps/
        step-1.json
      email-templates/
        rejection-notice.json
      mdh/
        vendors/
          collection.json            # name, description, MatchConfig refs
          indexes.json                # regular + Atlas Search indexes (NOT data rows)
    test/  # same shape
    prod/  # same shape
  secrets/                            # gitignored entirely
    dev.secrets.json
    test.secrets.json
    prod.secrets.json
```

### 5.2 Modules

| Module     | Responsibility |
|------------|----------------|
| `api`      | Rossum REST client. Auth, pagination, retries, rate limits, optimistic concurrency. |
| `model`    | Strongly-typed structs with `extra: Map<String, Value>` for forward-compat. |
| `snapshot` | Codec: in-memory model ↔ disk layout. Hook/formula code extraction. Deterministic JSON. |
| `slug`     | Stable slug derivation with collision policy and ID↔slug persistence. |
| `state`    | Lockfile read/write, atomic, checksummed, versioned (carries `version` field). |
| `overlay`  | Apply on push; reverse on pull. Surfaces overlay-managed values separately. |
| `diff`     | Semantic-aware structural diff (schema content by id, hook queues as set, code as text). |
| `merge`    | Three-way merge over `Diff` trees. Conflict detection. |
| `resolve`  | Interactive conflict resolver (TUI). |
| `map`      | Env-pair mapping store and bootstrap wizard. |
| `plan`     | Deploy planner: categorized diff, two-phase execution. |
| `indexer`  | Generates `_index.md`, cross-reference data. |
| `cli`      | Argument parsing (clap), output formatting, progress, prompts, JSON mode. |
| `secrets`  | Secrets file + env-var loader. Never logged. |

### 5.3 Module dependency graph

```
                cli
                 │
        ┌────────┼─────────────────────────┐
        │        │                         │
       plan   resolve                   indexer
        │        │                         │
        └───── merge ── diff ── snapshot ─┘
                 │             │
                 ├── overlay ──┤
                 │             │
                 ├── slug ─────┤
                 │             │
                state         model
                 │             │
                 │            api ── secrets
                 │
                (atomic disk I/O)
```

## 6. CLI surface

| Command | Purpose |
|---------|---------|
| `rdc init` | Interactive project bootstrap. Writes `rdc.toml`, `.gitignore`, env skeletons. |
| `rdc pull <env>` | Sync remote → local snapshot. Three-way merge with last lockfile. |
| `rdc push <env>` | Sync local → remote. Pre-push fetch + three-way merge. |
| `rdc plan --from <a> --to <b>` | Compute deploy plan; read-only. |
| `rdc apply --from <a> --to <b>` | Re-validate and execute plan. |
| `rdc map <a> <b>` | Bootstrap or update env-pair mapping. |
| `rdc diff <env>` | Local snapshot vs remote (read-only). |
| `rdc diff <a> <b>` | Two local snapshots. |
| `rdc status` | Health check across envs (auth, drift, lockfile staleness). |
| `rdc auth <env>` | Set/refresh token for an env. |
| `rdc repair --rebuild-lock <env>` | Re-pull and reconstruct a corrupted lockfile. |

**Global flags:** `--json`, `--no-color`, `--verbose`, `--debug`, `--concurrency=N`, `--yes` (skip confirmations; CI use).

## 7. Data flows

### 7.1 `rdc init`

1. Wizard: project name, env names, org URLs/IDs per env, optional workspace filters.
2. Writes `rdc.toml`, `.gitignore`, empty `envs/<name>/` dirs.
3. Prompts for tokens (write to `secrets/<env>.secrets.json`) or shows env-var instructions.
4. Optionally runs `rdc pull <env>` for each env.

### 7.2 `rdc pull <env>`

1. **Auth** — load token from secrets file or env var. Loud error if missing.
2. **Fetch** — concurrent GET (default 5) of every in-scope object. Pagination handled. Progress bar.
3. **Decode** — API JSON → model. Unknown fields preserved.
4. **Three-way compare** — base (lockfile) | local (disk) | remote (just fetched).
5. **Auto-merge** non-overlapping changes.
6. **Conflict resolver** for overlapping changes (TUI).
7. **Reverse-apply overlay** — strip overlay-managed values from incoming data.
8. **Write snapshot atomically.** For each object whose state changed (auto-merged or resolved), regenerate hook `.py`, formula `.py`, and the JSON file. Untouched objects' files are not rewritten. Regenerate `_index.md` only if the inventory changed.
9. **Update lockfile** with fresh hashes and `modified_at`.
10. **Summary** — counts of changed/new/deleted/conflicted.

**Idempotence:** with no remote changes since last pull, step 8 writes nothing.

### 7.3 `rdc push <env>`

1. **Auth.**
2. **Local diff** — what changed since lockfile.
3. **Re-fetch** touched objects (parallel).
4. **Three-way compare** — base | local | remote-now.
5. **Conflict resolver** if remote drifted under us.
6. **Apply overlay forward** — overlay values merged into outbound payloads.
7. **Inline code** — `.py` → hook JSON `code` field; formula `.py` → schema `formula` properties.
8. **Two-phase send** — pass 1: create/update objects; pass 2: resolve cross-references (e.g., new hook's `queues`).
9. **Verify** — re-fetch each pushed object, confirm server matches sent.
10. **Update lockfile** from authoritative server response.
11. **Summary.**

**Idempotence:** with no local changes, step 2 finds nothing; command exits with `nothing to push`.

### 7.4 `rdc plan` and `rdc apply`

1. Load `<src>` snapshot.
2. Pull fresh `<tgt>` state into memory (read-only, does not modify local snapshot of `<tgt>`).
3. Load `state/.rdc/map/<src>→<tgt>.toml`.
4. **Categorize each object's diff:**
   - **Drift** — `<tgt>` remote differs from local snapshot of `<tgt>`. User decides: accept (refresh local) or revert (push our truth).
   - **Divergence** — `<src>` differs from `<tgt>`; will propagate.
   - **Overlay-managed** — diff exists but `<tgt>` overlay marks it intentional. Excluded.
   - **New** — in `<src>`, no mapping. Will create.
   - **Deleted** — in `<tgt>` mapping, missing from `<src>`. Will delete only with `--prune`.
5. Render plan grouped by category. Stable IDs for `--target` re-runs.
6. **`apply`** re-runs the plan, aborts if changed since, then two-phase execute.
7. After apply: optionally `rdc pull <tgt>` to refresh local snapshot.

### 7.5 `rdc map <a> <b>`

1. For each object in `<a>`:
   - Auto-match same-slug object in `<b>`. Batched confirmation.
   - Multiple candidates → interactive picker.
   - No candidate → ask create / skip.
2. Write `state/.rdc/map/<a>→<b>.toml`. Hand-editable.
3. Re-runnable, idempotent. Adding new objects later updates the file.

## 8. Conflict handling

### 8.1 Inputs

- `base` — last-pulled snapshot per object (from lockfile).
- `local` — current disk state.
- `remote` — just-fetched server state.

### 8.2 Algorithm

1. `local-changes = diff(base, local)`; `remote-changes = diff(base, remote)`.
2. Strip noise from both: `modified_at`, server-set IDs/URLs, key-order, computed slugs, JSON whitespace, server-default values reappearing on previously unset fields.
3. Walk diff trees. For each leaf path:
   - Only one side changed → take that side.
   - Both changed, same value → not a conflict.
   - Both changed, different values → real conflict.
4. Apply semantic rules on known structures:
   - `schema.content[]`: matched by field `id`, not array index. Per-field changes evaluated independently.
   - `hook.queues[]`: treated as a set.
   - `hook.config.code` and formula `.py` files: text-level diff with line-level conflict markers.
   - `hook.secrets`: never compared. Always sourced from local `secrets/`.

### 8.3 Resolver UX

```
[1/3]  hooks/validator-invoices.py — line 42

<<< local                                 >>> remote
if amount > 1000:                         if amount > 999.99:
    flag_for_review()                         flag_for_review()

[k] keep local   [r] keep remote   [e] edit   [s] skip   [a] abort
```

`[e]` opens `$EDITOR` on a temp file with conflict markers; saving resolves.
`[a]` rolls back lockfile; nothing written.

After resolution, lockfile updates to the resolved value as the new `base`.

## 9. Overlay model

### 9.1 Purpose

Some divergence between envs is intentional and must never propagate: production secrets, env-named labels, environment-specific URLs. Overlays declare these.

### 9.2 File format

`envs/<env>/overlay.toml`:

```toml
version = 1

[hook.validator-invoices]
"settings.endpoint_url" = "https://prod-validator.example.com"
"name" = "Validator (PROD)"

[queue.cost-invoices]
"name" = "Cost Invoices (PROD)"
```

Override keys use JMESPath syntax. v1 supports per-object overrides only; wildcard / glob patterns are deferred to v2.

### 9.3 Pull behavior

For every overlay-managed key on an incoming object:

- If the remote value equals the overlay value → strip it from incoming data; snapshot stays canonical.
- If the remote value differs from the overlay value → present the user with two choices: (a) update overlay to the new remote value (env diverged from declared overlay), or (b) accept the value into the snapshot canonically (the key is no longer env-specific) and remove the overlay entry.

This is the only path by which an overlay file changes automatically. Manual hand-edits to overlay files are always allowed.

### 9.4 Push and deploy behavior

Overlay values are merged into outbound payloads. They never live in `*.json` snapshot files. They never propagate via deploy.

## 10. Mapping model

### 10.1 File format

`.rdc/map/test→prod.toml`:

```toml
version = 1

[hooks]
validator-invoices = "validator-invoices"
sftp-import = "sftp-import-prod"

[queues]
cost-invoices = "cost-invoices"

[hooks.skip]
# present in TEST, intentionally not deployed to PROD
debug-logger = true
```

Mapping is slug-to-slug, not ID-to-ID — slugs are stable across re-pulls; IDs are not.

### 10.2 Bootstrap (`rdc map a b`)

- Auto-match same-slug: batched confirmation.
- Ambiguous: interactive picker with object snippets.
- Unmatched: ask create-on-deploy / skip.

### 10.3 Maintenance

- Hand-editable.
- Re-running adds new entries without touching existing ones.
- `rdc plan` errors loudly if mapping is incomplete.

## 11. Object scope (v1)

| Object type | Source |
|-------------|--------|
| Organization (read-only metadata) | `envs/<env>/organization.json` |
| Workspaces | `envs/<env>/workspaces/<slug>/workspace.json` |
| Queues | `envs/<env>/workspaces/<slug>/queues/<slug>/queue.json` |
| Inboxes | `envs/<env>/workspaces/<slug>/queues/<slug>/inbox.json` |
| Schemas | `envs/<env>/workspaces/<slug>/queues/<slug>/schema.json` |
| Formula fields | extracted to `envs/<env>/workspaces/<slug>/queues/<slug>/formulas/<field-id>.py` |
| Hooks | `envs/<env>/hooks/<slug>.json` + `<slug>.py` |
| Rules | `envs/<env>/rules/<slug>.json` |
| Labels | `envs/<env>/labels/<slug>.json` |
| Engines | `envs/<env>/engines/<slug>.json` |
| Engine fields | `envs/<env>/engine-fields/<slug>.json` |
| Workflows | `envs/<env>/workflows/<slug>.json` |
| Workflow steps | `envs/<env>/workflow-steps/<slug>.json` |
| Email templates | `envs/<env>/workspaces/<ws-slug>/queues/<q-slug>/email-templates/<slug>.json` (queue-scoped, M16) |
| MDH dataset metadata | `envs/<env>/mdh/<dataset>/collection.json` |
| MDH indexes (regular + Atlas Search) | `envs/<env>/mdh/<dataset>/indexes.json` |

**Out of scope:** MDH dataset rows, annotation data, document data, user accounts, group definitions.

## 12. Authentication and secrets

### 12.1 API tokens

Per-env token stored in `secrets/<env>.secrets.json`:

```json
{
  "api_token": "rsk_…"
}
```

Override via env var: `RDC_TOKEN_<ENV>` (uppercase env name). Used for CI.

### 12.2 Hook secrets

Same file, additional keys:

```json
{
  "api_token": "rsk_…",
  "hooks": {
    "sftp-import": {
      "ssh_key": "-----BEGIN RSA PRIVATE KEY-----\n…"
    }
  }
}
```

Pulled remote secrets are never written to disk (Rossum API doesn't return secret values). On push, hook secrets are read from this file by slug and uploaded.

### 12.3 Future

OS keychain integration deferred to v2. v1 uses files because it's the simplest install story.

## 13. Error handling

| Failure | User experience |
|---------|-----------------|
| Missing/invalid token | `error: env 'prod' has no token. Set RDC_TOKEN_PROD or write secrets/prod.secrets.json.` |
| Network / 5xx | Retry with exponential backoff (default 3 attempts). Final failure names last status. |
| 429 rate-limit | Honor `Retry-After`, show inline wait. |
| Stale `modified_at` on push | Engages conflict resolver. Not an error. |
| Disk-write failure | Roll back; nothing partial. Error names path. |
| Unknown API field | Preserve via `extra` map; warn once per object type per session. |
| Plan outdated at apply | Abort apply; suggest `rdc plan` re-run. |
| Mapping incomplete | List the unmapped objects; suggest `rdc map`. |
| Ctrl-C | Atomic writes mean partial state is impossible. Network ops abort cleanly. |
| Corrupt lockfile | Detect via checksum; suggest `rdc repair --rebuild-lock <env>`. |

Principles: every error names (a) what failed, (b) why, (c) the next concrete action. Stack traces only with `--debug`.

## 14. Testing

### 14.1 Unit (cargo test)

- `slug` — collision handling, stability, Unicode.
- `snapshot` codec — round-trip property test (random model → write → read → equal).
- `diff`/`merge` — table-driven tests per known semantic structure.
- `overlay` — apply/reverse round-trip preserves canonical form.
- `state` — atomic write and recovery.

### 14.2 Integration (cargo test --test integration)

- Mock Rossum server (`wiremock`) replaying captured fixtures.
- Full pull/push/plan cycles with fixture data.
- "Pull twice produces zero diff" — runs in CI as regression guard.
- Conflict scenarios: ten hand-crafted base/local/remote triples.

### 14.3 End-to-end smoke

- Optional CI job against a sandbox Rossum org.
- `rdc init && rdc pull && rdc push` cycle. Detects API breakage.

### 14.4 Acceptance bar (v0.1)

- All unit tests green.
- Pull-twice-zero-diff passes.
- Pull, push, plan/apply work end-to-end on the sandbox org.
- 200-object real implementation pulls in <30s on a normal connection.

## 15. Distribution

- **GitHub releases** — signed binaries for `darwin-arm64`, `darwin-amd64`, `linux-amd64`, `linux-arm64`, `windows-amd64`.
- **Homebrew tap** — `brew install rossumai/tap/rdc`.
- **`cargo install rdc`** — bonus for Rust users.
- **`curl -fsSL https://… | sh`** — one-liner installer that fetches the right binary.
- **Self-update** — `rdc update` checks GitHub releases.

## 16. Open questions / deferred decisions

1. **Final tool name.** `rdc` is the working name throughout this spec. Alternatives the user may pick before v0.1: `rossum`, `rcp`, `roma`, `rod`. Renaming is a global find-and-replace; the design does not depend on the name.
2. **OS keychain integration** — deferred to v2.
3. **Telemetry / opt-in usage stats** — deferred. v1 is offline-only by default.
4. **Concurrency policy** — default 5 parallel API calls, overridable via `--concurrency` and `RDC_CONCURRENCY`. May need empirical tuning against Rossum's rate limits during implementation.
5. **`rdc revert`** (delete deployed objects from target) — out of v0.1 scope; planned for v0.2.
6. **Watch mode** (`rdc watch <env>` for continuous pull) — out of scope for v1; reconsider if user demand emerges.

## 17. Glossary

- **Env / environment** — a logical Rossum config slice: `(org, optional workspace filter)`. Defined in `rdc.toml`. Examples: dev, test, prod.
- **Snapshot** — the on-disk file tree under `envs/<env>/` representing one env.
- **Overlay** — `envs/<env>/overlay.toml`. Declares legitimate env-specific divergence; never propagates.
- **Mapping** — `.rdc/map/<a>→<b>.toml`. Slug-to-slug correspondence between two envs. Required for `plan`/`apply`.
- **Lockfile** — `.rdc/state/<env>.lock.json`. Last-pulled snapshot hashes + slug↔ID map per object.
- **Drift** — divergence between local snapshot of an env and remote state of the same env.
- **Divergence** — difference between two envs (the thing deploys propagate).
- **Plan** — categorized diff produced by `rdc plan`, executed by `rdc apply`.
