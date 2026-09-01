# Desktop app — drop in-app promote, add two-way sync and watch

**Status:** design (brainstorming complete, awaiting review)
**Date:** 2026-09-01
**Supersedes (in part):** `2026-08-03-desktop-multi-env-and-promote-design.md`
— its multi-env foundation stands; its Promote panel is removed here.
**Scope:** the Flutter app (`desktop/`), its bridge crate (`desktop/rust`,
`rdc_bridge`), **and** the `rdc` core (`src/cli/sync/watch.rs`,
`src/cli/stdin_coord.rs`, `src/log.rs`, `src/cli/sync/embed.rs`, the seven
prompt sites). Unlike the 2026-08-03 spec, this one **does** modify the core.

## 1. Problem

Two problems, one change.

**Promotion belongs in CI, not in the app.** `templates/gitlab-ci.yml`'s
`.rdc-deploy` job runs `rdc migrate "$RDC_SRC" "$RDC_ENV" --mirror --yes`
followed by `rdc sync "$RDC_ENV" --allow-deletes --yes`, gated on
`needs: ["pytest"]`. The desktop's Promote panel
(`desktop/rust/src/api/rdc.rs:591` `prepare_promotion`, `:639`
`push_promotion`) performs the *identical* pair of operations with **no test
gate at all**. Two paths to the same destructive outcome, one of them
unverified, is one too many — and the unverified one is the one a human can
press by accident.

**The app cannot follow a working session.** Its only sync is
`embed::sync_no_push_logged` (`rdc.rs:491`), a one-shot pull. Under `no_push`
every `LocalEdit` / `LocalCreate` / `LocalDelete` is suppressed
(`execute.rs:3423`, `:3501`), so the app can show you an org but never help you
change one. The CLI has had `rdc sync <env> --watch` since 2026-05 —
file-triggered push plus a remote drift poll — and the app has no equivalent.

The two are connected: removing the cross-env write path is what makes a
within-env write path safe to add. Releases go through CI, where the tests are;
the app becomes the inner loop for the one env you are working in.

## 2. Decisions (fixed during brainstorming)

Each of these was chosen explicitly; none is a default that fell out.

- **Remove promote completely** — both halves (offline `migrate` and the gated
  push), not just the push.
- **Watch is bidirectional** — full `rdc sync --watch` semantics: `notify(2)`
  on `envs/<env>/` triggers a push cycle, the poll timer triggers a pull.
- **The plain per-env Sync becomes bidirectional too** — one full
  `rdc sync <env>` cycle. Two adjacent buttons must not have opposite blast
  radii, and a watch's initial reconcile is a full cycle regardless.
- **Real prompts in the app for all seven blocking gates**, minus `[e]` and
  `[h]` (see §4.3). Not a pre-declared policy, not a pause-and-defer.
- **Many envs watchable concurrently**, with prompt routing moved from the
  process-global coordinator to a thread-local one.
- **One generalized core watch loop**, parameterised, with `run_watch` (CLI)
  and `embed::watch_logged` (app) as its two callers — rather than a second
  loop in the bridge crate or subprocessing the `rdc` binary.
- **Keep the one-time two-way confirmation** (§8) rather than let Sync silently
  change blast radius under existing users.
- **Document `--watch` in the root `README.md`**, which currently never
  mentions it.

## 3. Goals / non-goals

**Goals**

1. No path from the desktop app to a cross-environment write.
2. `rdc sync <env>` and `rdc sync <env> --watch` are both available in the app,
   with the same semantics they have in the terminal.
3. Every gate that blocks a cycle is answerable in the app. None silently
   skips, none hard-bails and kills a watch.
4. The CLI is **byte-for-byte unchanged**, with exactly one deliberate
   exception: under `--watch`, a prompt now clears the in-place countdown line
   before drawing instead of tearing against it (§6.2). Every other byte of
   every other prompt stays identical, pinned by tests.
5. A project folder stays interchangeable between the CLI and the app, in both
   directions, with no migration.

**Non-goals**

- `[e]` (external `$EDITOR`) and `[h]` (per-hunk walk) in the app.
- Auto-resuming watches after an app restart.
- A "watch every env in this project" action.
- Any desktop route back to cross-env promotion.
- OS notifications when a prompt blocks while the window is unfocused — the
  analogue of the CLI's attention bell. Worth a follow-up; not this change.

## 4. Verified facts (grounding)

Everything below was read out of the tree at `ee7c850`, not recalled.

### 4.1 What promote is, and what CI already does

| | Desktop Promote | CI `.rdc-deploy` |
|---|---|---|
| stage 1 | `rdc::cli::migrate::run_at(folder, src, tgt, mirror, …)` (`rdc.rs:606`) | `rdc migrate "$RDC_SRC" "$RDC_ENV" --mirror --yes` |
| stage 2 | `embed::sync_push_logged(..., dry_run=false)` (`rdc.rs:670`) | `rdc sync "$RDC_ENV" --allow-deletes --yes` |
| test gate | none | `needs: ["pytest"]` |

Promote is the app's **only** tenant write path: the per-env sync is
`sync_no_push_logged`, and `no_push` suppresses every outbound mutation
including deletes (`execute.rs:3423`, `:3501`).

### 4.2 The seven blocking prompts

| # | Prompt | Keys | Body currently goes to |
|---|---|---|---|
| 1 | `BothDiverged` conflict | `k/r/e/s/a` (+`h` if ≥2 hunks, +`K/R` bulk) | raw stderr (`execute.rs:1527`) |
| 2 | Remote-delete / double-conflict | `k/r/s/a` (+bulk) | raw stderr (`execute.rs:1611`) |
| 3 | Mid-cycle push drift | `k/r/e/s/a` | raw stderr (`pull/common.rs:954`) |
| 4 | Destructive delete gate | `y/N` | list → `Log` ✓, question raw (`deletes.rs:148`) |
| 5 | Delete drift | `k/r/s/a` | question raw (`deletes.rs:368`) |
| 6 | MDH index-drop gate | `y/N` | list → `Log` ✓, question raw (`mdh.rs:767`) |
| 7 | MDH row-delete gate | `y/N` | list → `Log` ✓, question raw (`mdh_data.rs:330`) |

`7e96b89` routed the *lists* of 4/6/7 through the `Log`. The conflict **diff
bodies** (1/2/3) still write to `std::io::stderr().lock()` (`execute.rs:349`,
`pull/common.rs:954`), so an embedder consuming `Log::for_sink` never sees
them — a dialog rendered today would have no diff in it.

`[e]` shells out to `$EDITOR` on a temp file (`resolve.rs:694`). `[h]` is a
stateful per-hunk sub-walk (`resolve.rs:797`).

### 4.3 What a non-interactive cycle does at each gate

- **Pending local deletion, `allow_deletes: false`, non-interactive → hard
  `bail!`** (`deletes.rs:138`). In `event_loop` a non-401, non-transient error
  is `return Err(e)` — the watch **dies**, it does not skip.
- **`BothDiverged`, no strategy, non-interactive → silent shadow-file skip**,
  and the object stays skipped while the shadow sits under
  `.rdc/conflicts/<env>/`. The CI archive job passes `--conflict use-remote`
  precisely to avoid this.
- **`allow_deletes: false` + `interactive: true` → prompt** (`deletes.rs:131`
  short-circuits *before* the prompt; `:138` bails only when `!interactive`).

That last line is the load-bearing one: **`interactive: true, allow_deletes:
false, conflict_strategy: None`** turns every destructive act into a dialog and
removes both the silent skip and the hard bail.

- On EOF, resolvers already degrade safely: `prompt_resolve` returns
  `Resolution::Skip` (`resolve.rs:381`); the `y/N` gates read as `N`.

### 4.4 Why the core loop is not reusable as-is

`run_watch` reads `std::env::current_dir()` (`watch.rs:70`), resolves tokens
from on-disk secrets, takes sole ownership of stdin, installs a Ctrl-C handler,
and ends in `std::process::exit(0)` (`watch.rs:236`) — deliberately, because
the stdin reader parks in an uncancellable blocking read. `event_loop`
hardcodes `None, None` for `run_cycle`'s `cwd_override` / `token_override`
(`watch.rs:337`).

But `run_cycle` **already accepts both** (`sync/mod.rs:199-210`), which is what
makes generalising the loop a plumbing job rather than a rewrite.

### 4.5 Concurrency constraints

- `stdin_coord::COORD` is a process-global `OnceLock<StdinCoordinator>` whose
  `waiting` field is a **single** `Mutex<Option<Sender>>` (`stdin_coord.rs:33`).
  Two cycles blocking at once: the second `recv_line` overwrites the first's
  sender and the first hangs forever. Concurrent watches therefore cannot share
  it.
- A cycle never leaves its thread. Engine concurrency is `buffer_unordered` on
  the current task — `api/mod.rs:461` states it outright ("never
  `tokio::spawn`ed") — and the bridge's `block_on` builds a
  `new_current_thread` runtime (`rdc.rs:841`). A **thread-local** route
  therefore scopes exactly one per watch.
- FRB's executor is `SimpleThreadPool` → `threadpool::ThreadPool::default()` →
  `num_cpus::get()` threads. A watch holds one for its whole life.
- `StreamSink::add` returns `Result<(), Rust2DartSendError>` (FRB 2.12.0
  `for_generated/boilerplate.rs:174`), so the Rust side can detect a closed
  Dart stream.
- `EnvLock` is per-env (`paths.env_lock()`), acquired per cycle with a 30s
  timeout — two watches on different envs never contend.

### 4.6 Token refresh

`refresh_token_for_401` is CWD-based and only re-logins silently from
`RDC_USER_<ENV>` / `RDC_PASS_<ENV>` env vars (`auth.rs:198-230`); it never
reads the `username`/`password` the desktop persists in
`secrets/<env>.secrets.json`. And `resolve_token` hands back the *same* token
while it is still clock-valid (`secrets.rs:216`), so re-calling it after a 401
changes nothing. An embedded watch needs its own forced re-login.

### 4.7 Renderer

`Log::for_sink` sets `is_tty = false` (`log.rs:309`), and `tick_status` is a
verified no-op off a TTY (`log.rs` test `tick_status_is_noop_on_non_tty`). The
watch countdown's `▰▱` bar therefore never reaches an embedder — the app must
render its own from a structured event.

`Log`'s inner sink is `Box<dyn Write + Send>` behind a mutex, and `Log::new`
sets it to stderr. Routing prompt bodies through the `Log` instead of raw
stderr is therefore byte-identical for the CLI.

### 4.8 Toolchain

`flutter_rust_bridge_codegen 2.12.0` is installed and matches the repo's pin,
so bindings can be regenerated.

## 5. Removal

**`desktop/rust/src/api/rdc.rs`** — delete `ConflictPolicy`,
`policy_to_strategy` and its unit test
(`conflict_policy_maps_inverted_for_promote_push`), `PromotionPreview`,
`prepare_promotion`, `push_promotion`, `LineCollector`, `env_api_base`. This
removes the last `rdc::cli::migrate::run_at` call from the app.

**`desktop/lib/src/app_state.dart`** — delete `PromoteStage` and every
`promote*` field and method (`setPromoteDir`, `swapPromoteDir`,
`setPromoteMirror`, `setPromotePolicy`, `setPromoteAllowDeletes`,
`restorePromoteDefaults`, `savePromoteDefaults`, `resetPromote`,
`preparePromote`, `applyPromotePreview`, `pushPromote`, `_pushSub`), and
`revealMapping` / `revealOverlay` — reachable only from the promote ⚙ menu.
`Reveal` on the project still opens the folder, from which `.rdc/` is one
keystroke away.

**`desktop/lib/src/home_page.dart`** — delete `_PromotePanel`,
`_PromotePickerRow`, `_ConfigureMenu`, `_PromotePreparing`,
`_PromotePreviewPanel`, `_PromotePushPanel`, and the first-two-envs post-frame
default at `:875`.

**Tests** — delete `test/promote_test.dart`, `test/promote_defaults_test.dart`;
regenerate `test/goldens/mdh_project_light.png`.

**Core** — `embed::sync_push_logged` loses its only caller.
`sync_no_push_logged` and `sync_push_logged` collapse into one
`embed::sync_logged(cwd, env, token, flags, log_sink)`; `embed::sync_no_push`
stays (`tests/embed_sync.rs` uses it). This is a breaking change to a `pub` lib
signature — acceptable because the crate is private, unpublished, and has only
in-tree consumers. Stated rather than assumed.

**Docs** — `desktop/README.md` loses its Promote section (including the "Known
limitation" paragraph about Prepare overwriting the target snapshot, which
stops being true) and gains a Watch section.

## 6. Core changes

### 6.1 Generalized watch loop

`event_loop` takes a config struct instead of positional flags:

```rust
pub struct WatchConfig<'a> {
    pub env: &'a str,
    pub cwd: Option<&'a Path>,        // None → current_dir() (CLI)
    pub token: Option<String>,        // None → on-disk secrets (CLI)
    pub interactive: bool,
    pub allow_deletes: bool,
    pub no_push: bool,
    pub no_pull: bool,
    pub poll: Option<Duration>,
    pub verbose: bool,
    pub no_bell: bool,
}
```

`cwd` and `token` thread into both `Paths::for_env` and `run_cycle`'s existing
`cwd_override` / `token_override`. The Ctrl-C oneshot becomes a generic
`CancelToken`. 401 handling becomes a `TokenRefresher` supplied by the caller.

Two callers:

- **`run_watch` (CLI)** — supplies `None`/`None`, activates the stdin
  coordinator, wires Ctrl-C to the cancel token, uses
  `refresh_token_for_401`, and keeps its `process::exit(0)`. Behaviour
  unchanged.
- **`embed::watch_logged(cwd, env, token, opts, log_sink, route, cancel)`** —
  owns no stdin, installs a thread-local prompt route, returns `Result<()>`
  normally, and refreshes 401s via a new
  `secrets::force_relogin(root, env, api_base)` that uses the secrets file's
  persisted `username`/`password`. In token-auth mode it fails with a directed
  message ("token rejected — update it in Edit").

### 6.2 Prompt bodies reach embedders

Add `Log::writer()` → an RAII guard implementing `io::Write` that clears any
active in-place status line once, then writes verbatim into the `Log`'s inner
sink.

Replace `std::io::stderr().lock()` at `execute.rs:349` and `pull/common.rs:954`
with it, and move the four raw `eprint!` question lines
(`deletes.rs:148`, `:368`, `mdh.rs:767`, `mdh_data.rs:330`) onto it.

For `Log::new` the inner sink *is* stderr, so CLI output is byte-identical.
Under `--watch` it is a small improvement: the prompt now clears the countdown
line instead of tearing against it.

### 6.3 A structured question

```rust
pub struct PromptKey { pub key: char, pub label: String }
pub enum PromptKind { Conflict, RemoteDelete, PushDrift, BulkConfirm,
                      DeleteGate, DeleteDrift, MdhIndexDrop, MdhRowDelete }
pub struct Prompt { pub kind: PromptKind, pub question: String, pub keys: Vec<PromptKey> }

pub fn announce(p: Prompt);
```

Each site declares what it is asking immediately before writing the question,
then writes and reads exactly as it does today. `question` is the same string
the terminal shows; `keys` is the machine-readable half.

**Why `announce` and not a combined `ask(w, &prompt)` that both writes and
reads.** The three big resolvers take a generic `R: BufRead` input precisely so
their unit tests can drive them with a `Cursor`. Folding the read into `ask`
would route those tests through the coordinator and break every one of them.
Splitting the two halves leaves the read path untouched: a test supplying its
own `Cursor` never reaches `read_line_coordinated`, and only production, which
uses `CoordinatorStdin`, sees the route.

Eight sites announce, not seven — `confirm_bulk` is a nested read inside the
conflict prompt, and without its own announce it would inherit an
already-consumed slot and show the app a stale question.

### 6.4 Thread-local prompt routing

```rust
pub trait PromptRoute: Send + Sync {
    fn ask(&self, prompt: &Prompt) -> Option<String>;   // None = EOF/cancel
}
thread_local! { static ROUTE: RefCell<Option<Arc<dyn PromptRoute>>> = … }
```

Resolution order inside `read_line_coordinated`: **thread-local route →
global `COORD` → real stdin.** The CLI never installs a thread-local, so its
path is provably untouched, and the global coordinator keeps working for
`--watch` on a TTY.

The embed route emits `SyncPhase::Prompt` on the stream and blocks on an mpsc
receiver that the bridge's `answer_prompt` feeds. Dropping it (stop / app quit)
yields `None`, which the resolvers already treat as EOF: `Skip` for conflicts,
`N` for the gates.

**The app's cycle flags are `interactive: true`, `allow_deletes: false`,
`conflict_strategy: None`.** Per §4.3 that makes every destructive act a
dialog, with no silent skip and no watch-killing bail.

## 7. Bridge surface

```rust
pub fn sync_env(folder, env, api_base, org_id, sink: StreamSink<SyncPhase>) -> Result<()>;
pub fn watch_env(folder, env, api_base, org_id, poll_secs: Option<u64>,
                 sink: StreamSink<SyncPhase>) -> Result<()>;
pub fn stop_watch(folder: String, env: String) -> Result<()>;
pub fn answer_prompt(folder: String, env: String, prompt_id: u64, answer: String) -> Result<()>;
```

`sync_env` keeps its name and signature and becomes a full cycle. Watches live
in a global registry keyed by `(folder, env)` holding the cancel token and the
answer sender; each occupies one FRB pool thread (§4.5).

`SyncPhase` gains:

- `Prompt { id: u64, question: String, keys: Vec<PromptChoice> }`
- `PromptResolved { id: u64 }` — so a dialog closes if the cycle moved on or
  the watch was stopped
- `Idle { next_poll_secs: Option<u64> }` — the app's own countdown, needed
  because `tick_status` never reaches a sink (§4.7)
- `Stopped`

`Started` / `Log` / `Done` / `Error` are unchanged. Adding variants to a
bridged sealed class is source-breaking for exhaustive Dart switches; the only
consumer is this app, updated in the same change.

Bindings regenerate with the pinned `flutter_rust_bridge_codegen 2.12.0`
(`lib/src/rust/`, `rust/src/frb_generated.rs`, both committed).

## 8. UI

**`_ConnBar`** — `[Sync] [Watch] [Edit] [Reveal] [Remove]`. `Watch` flips to
`Stop` while active. `Sync` disables during a running cycle of that env, as
today.

**`_EnvTableRow`** — a `visibility` / `visibility_off` icon button beside the
sync icon; `_badgeFor` gains a `watching` badge. `_St` gains `_St.watching`,
ranked above `synced` and below `error`.

**Sidebar `_EnvRow`** — the 7px dot gains the watching state; the trailing
`org N · <sub>` shows `watching · 42s`, counted down from `Idle`.

**`PromptDialog`** (new in `dialogs.dart`, following the `_Frame` /
`RemoveDialog` idiom) — the log tail rendered through the existing `ansiSpans`
(this is what §6.2 unlocks), the question line, one button per offered key.
The **route filters `e` and `h` out of `keys`** before they reach Dart; if an
unoffered key somehow arrives, the core's re-prompt loop handles it.

**`AppState`** — `Map<String, WatchState>` keyed by the existing `envKey`,
holding `{running, nextPollSecs, pendingPrompt, log}`. Prompts from two watched
envs queue: one dialog, plus a "1 more waiting" affordance.

**One-time two-way confirmation** — on the first two-way sync of a project, a
one-sentence dialog explains that Sync now pushes local edits, persisted as
`ackTwoWay` per project. This is the only guard against the escalation in §9.

**Not built** — a "Watch all envs" button. `Sync all envs` stays and is now
bidirectional.

## 9. Backward compatibility

- **On-disk rdc contract unchanged.** `rdc.toml`, `secrets/`, `envs/`,
  `.rdc/state` are untouched; a folder stays interchangeable between CLI and
  app in both directions, with no migration step.
- **`~/.rdc-desktop/settings.json`** — `promoteDefaults` stops being read.
  `Settings` starts **preserving unrecognised keys** across a save (today
  `toJson` drops them, `settings.dart:69`), so a downgrade to an older build
  still finds its promote directions. New `watch` key holds per-env
  `pollSecs`.
- **`rdc sync --watch` on the CLI: no user-visible change.** A hard constraint,
  pinned by tests written *before* the `stderr`→`Log` move (§10), not asserted
  after.
- **The one real escalation:** per-env Sync was `--no-push` and will push.
  Anyone using the app as a read-only archiver starts writing to a tenant, and
  a plain `LocalEdit` push has no gate. Mitigated by the one-time confirmation
  in §8, not by silence.
- **`embed`'s public signature changes** (§5) — private crate, in-tree
  consumers only.

## 10. Testing

1. **Before** the `stderr`→`Log` move: capture-stderr tests pinning the
   conflict prompt, the delete gate and both MDH gates byte-for-byte. The
   refactor is only safe because these land first.
2. `event_loop`: the existing watch tests stay green **unmodified**; new tests
   for cwd/token injection, cancel-token shutdown, and **two threads with two
   routes and no cross-talk** — testing the exact constraint (§4.5) that ruled
   out the global coordinator.
3. Bridge: `stop_watch` cancels mid-poll; `answer_prompt` delivers; a dropped
   route degrades to EOF behaviour (`Skip` / `N`).
4. Dart: watch lifecycle, cross-env prompt queueing, and that `e`/`h` are never
   offered.
5. Goldens: `mdh_project_light` regenerated (Promote gone), plus
   `mdh_watching_light` and `mdh_prompt_light`.

## 11. Risks / deferred

- **The `stderr`→`Log` move is the highest-risk step.** Ordering between
  prompt bodies and log events changes from "two writers to one fd" to "one
  mutex". Mitigated by step 1 of §10 and by the fact that the CLI's inner sink
  *is* stderr.
- **Pool starvation.** Watching ~`num_cpus` envs at once would block other
  bridge calls (`list_projects`, the Files tab). Accepted for now; running
  watches off dedicated OS threads is the escape hatch, and needs one FRB
  behaviour proven first (that a stream survives its exported fn returning).
- **Prompt fatigue under watch.** A perpetually diverged object will re-ask
  every cycle. The bulk `[K]`/`[R]` keys already exist and carry across phases
  via `bulk_sticky`; whether they should also persist *across cycles* within
  one watch is deferred.
- **Root `README.md` has zero mentions of `--watch`.** Documented as part of
  this change.

## 12. Suggested build order

1. Pin CLI prompt output (§10.1). No behaviour change.
2. `Log::writer()`; move the three stderr sinks and four `eprint!`s onto it.
3. `Prompt` / `announce` / thread-local `PromptRoute`; CLI unchanged throughout.
4. `WatchConfig` + generalized `event_loop`; `run_watch` becomes a thin caller.
5. `embed::sync_logged` + `embed::watch_logged` + `secrets::force_relogin`.
6. Remove promote from the bridge; add `watch_env` / `stop_watch` /
   `answer_prompt`; regenerate bindings.
7. Remove promote from Dart (state, UI, settings, tests, golden).
8. Watch UI, `PromptDialog`, new goldens.
9. Docs: `desktop/README.md`, root `README.md`.
