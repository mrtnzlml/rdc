//! Process-global stdin coordination for `rdc sync --watch`.
//!
//! Watch mode has two would-be stdin consumers: the Enter-trigger reader
//! (fires a sync early when the user presses Enter) and the cycle's
//! interactive prompts (conflict / remote-delete / destructive-delete
//! resolvers). They cannot both own the terminal — the previous design had
//! them fight over the process-global stdin lock, deadlocking each cycle
//! until the user pressed Enter.
//!
//! This coordinator makes the watch reader the SOLE stdin owner. It reads
//! lines and routes each one: to an interactive prompt if one is waiting
//! (via [`StdinCoordinator::try_deliver`]), otherwise back to the reader's
//! Enter-trigger logic. Prompts never touch the real stdin in watch mode;
//! they call [`read_line_coordinated`] (directly or through
//! [`CoordinatorStdin`]), which blocks until the owner hands them a line.
//!
//! Outside watch mode the coordinator is never [`activate`]d, and
//! `read_line_coordinated` falls back to reading the real stdin directly,
//! so non-watch `rdc sync` / `deploy` behave exactly as before.

use std::cell::RefCell;
use std::future::Future;
use std::io::{self, BufRead};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};

/// One answerable choice in a prompt: the character the resolver matches on
/// and the words the terminal shows beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptKey {
    pub key: char,
    pub label: String,
}

impl PromptKey {
    pub fn new(key: char, label: &str) -> Self {
        Self {
            key,
            label: label.to_string(),
        }
    }
}

/// Which decision is being asked. A non-terminal consumer uses this to
/// title its dialog; the resolvers do not branch on it.
///
/// `Unknown` is the only variant this task's own code constructs
/// (`Prompt::unknown`, the fallback for a read whose site never
/// announced). The push-drift prompts (`resolve_push_drift`, and the
/// mid-cycle drift check in `pull/common.rs`) route through the shared
/// conflict resolver and are announced as `Conflict` — same decision, same
/// keys — so there is no separate push-drift variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Conflict,
    RemoteDelete,
    BulkConfirm,
    DeleteGate,
    DeleteDrift,
    MdhIndexDrop,
    MdhRowDelete,
    /// A coordinated read whose site never called `announce`. Should be
    /// unreachable in a correctly wired build — every resolver announces
    /// before it reads — so it exists to make a missed announce loud (a
    /// visibly wrong dialog) rather than silently disguised as one of the
    /// real decisions above.
    Unknown,
}

/// What a blocked prompt is asking, in machine-readable form.
///
/// `question` is the choices on ONE line, trailing `"> "` and all
/// (`change_view::menu_one_line`) — for a consumer that renders a question
/// plus its own buttons. The terminal shows the same choices wrapped over
/// several lines (`change_view::render_menu`); both are built from `keys`,
/// so they cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub question: String,
    pub keys: Vec<PromptKey>,
}

impl Prompt {
    /// Fallback for a coordinated read whose site has not announced —
    /// a free-text answer with no offered keys, labelled
    /// [`PromptKind::Unknown`] rather than a real decision so a missed
    /// `announce` call surfaces as visibly wrong instead of being
    /// disguised as a legitimate one. Keeps an un-announced read working
    /// rather than panicking.
    fn unknown() -> Self {
        Self {
            kind: PromptKind::Unknown,
            question: String::new(),
            keys: Vec::new(),
        }
    }
}

/// Where a blocked prompt's question goes and where its answer comes from,
/// when the consumer is not a terminal. Scoped to the current async TASK
/// (via [`with_route`] / `tokio::task_local!`), not the OS thread: a
/// thread-local route is silently wrong under a multi-thread runtime,
/// because tokio's work-stealing scheduler is free to resume a task's
/// `.await` continuation on a different worker thread than the one that
/// started it — a watch's route would then either vanish or, worse, read
/// back whatever OTHER task's route happened to be installed on the
/// resuming thread. A task-local value travels WITH the task across such
/// moves, so it stays attached to the one watch that installed it
/// regardless of which OS thread ends up polling it. The process-global
/// `COORD` (below) has a single waiting slot for the same reason two
/// concurrent watches can't share it — but a route is scoped per-watch, so
/// task-local is the right granularity, not process-global.
pub trait PromptRoute: Send + Sync {
    /// Resolve once the consumer answers. `None` means end of input; every
    /// resolver already degrades safely on that (conflicts skip, gates
    /// read as `N`).
    fn ask<'a>(&'a self, prompt: &'a Prompt) -> crate::rt::BoxFuture<'a, Option<String>>;
}

tokio::task_local! {
    static ROUTE: Arc<dyn PromptRoute>;
    // Scoped alongside `ROUTE` (see `with_route`), not separately: an
    // earlier version kept this thread-local on the theory that `announce`
    // is always immediately followed by the read that consumes it, with no
    // intervening `.await` for another task to interleave on the same OS
    // thread. That theory does not need to hold for every future caller —
    // and empirically it took only a deliberately adversarial test
    // (`routes_do_not_cross_talk_between_concurrent_tasks`, which yields
    // between `announce` and the read specifically to force a reschedule)
    // to falsify it: thread-local `PENDING` handed one task's announced
    // question to the OTHER task's route after a work-stealing move. Task-
    // local removes the assumption entirely.
    static PENDING: RefCell<Option<Prompt>>;
}

/// Run `f` with `route` installed as the [`PromptRoute`] for every
/// coordinated prompt `f` (or anything it calls, including across
/// `.await` points) issues, via `announce` + `read_line_coordinated`.
///
/// Nothing in the CLI calls this — it is the entry point a non-terminal
/// consumer (the desktop bridge, `cli::sync::embed::watch_logged`) uses to
/// scope one watch's prompts to its own route. Backed by `tokio::task_local!`
/// (see [`PromptRoute`]'s doc for why), so the route (and its pending
/// announcement — see `PENDING`) is visible for exactly the lifetime of
/// `f` and disappears the instant `f` resolves — no guard to drop, no risk
/// of a stale route outliving its watch.
pub async fn with_route<F: Future>(route: Arc<dyn PromptRoute>, f: F) -> F::Output {
    ROUTE
        .scope(route, PENDING.scope(RefCell::new(None), f))
        .await
}

/// Declare what the next coordinated read is asking. Call immediately
/// before writing the question. A no-op when no route is installed, which
/// is every CLI invocation — `PENDING` is only in scope while `ROUTE` is
/// (both are scoped together by [`with_route`]), so `try_with` fails
/// exactly when there is no route to announce to.
pub fn announce(p: Prompt) {
    let _ = PENDING.try_with(|slot| *slot.borrow_mut() = Some(p));
}

/// Routing state shared between the stdin owner and interactive prompts.
pub struct StdinCoordinator {
    /// Sender for the prompt currently blocked waiting for a line, if any.
    /// Set by [`StdinCoordinator::recv_line`] for the duration of one read
    /// and cleared immediately after, so a line that arrives while no
    /// prompt is reading falls through to the Enter-trigger path.
    waiting: Mutex<Option<mpsc::Sender<String>>>,
}

static COORD: OnceLock<StdinCoordinator> = OnceLock::new();

/// Activate coordination and return the global coordinator. Called once by
/// the watch reader on startup, before any cycle can run a prompt.
/// Idempotent.
pub fn activate() -> &'static StdinCoordinator {
    COORD.get_or_init(StdinCoordinator::new)
}

/// Whether the watch reader owns stdin. A prompt that would read the
/// terminal itself (inquire) must go through [`read_line_coordinated`]
/// instead while this is true, or it races the reader for the input.
pub fn is_active() -> bool {
    COORD.get().is_some()
}

impl StdinCoordinator {
    fn new() -> Self {
        Self {
            waiting: Mutex::new(None),
        }
    }

    /// Hand `line` to a prompt that is currently waiting for input.
    /// Returns `Err(line)` (the line back) if no prompt is waiting, so the
    /// owner can route it elsewhere (Enter-trigger / drop).
    pub fn try_deliver(&self, line: String) -> Result<(), String> {
        let guard = self.waiting.lock().unwrap();
        match guard.as_ref() {
            // `send` only errors if the prompt's receiver was dropped (it
            // gave up between registering and our send); treat that as "not
            // delivered" so the line still routes sensibly.
            Some(tx) => tx.send(line).map_err(|e| e.0),
            None => Err(line),
        }
    }

    /// Register as the waiting prompt and block until the owner delivers a
    /// line. Returns `None` if every sender is dropped (end of input).
    fn recv_line(&self) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        *self.waiting.lock().unwrap() = Some(tx);
        let line = rx.recv().ok();
        *self.waiting.lock().unwrap() = None;
        line
    }
}

/// Watch-mode attention bell state. Armed once per watch cycle by the watch
/// loop (via [`arm_bell`]); consumed and emitted by [`maybe_ring_bell`] the
/// moment a prompt blocks for user input, so an away-from-keyboard user is
/// pulled back. A process-global flag (like [`COORD`]) because watch is a
/// single foreground process and the bell is a cross-cutting UI nudge.
static BELL_ARMED: AtomicBool = AtomicBool::new(false);

/// Arm the attention bell. The next [`maybe_ring_bell`] on a TTY emits a BEL
/// (0x07) to stderr, then disarms. Only the watch loop calls this, so the bell
/// never fires outside `--watch`.
pub fn arm_bell() {
    BELL_ARMED.store(true, Ordering::Relaxed);
}

/// Decide whether the bell should ring now, disarming if so: armed AND
/// `is_tty`. Split from the I/O so the arm / debounce / re-arm / TTY-gate
/// logic is unit-testable. The `&&` short-circuit means a non-TTY never
/// consumes the armed flag.
fn take_bell(is_tty: bool) -> bool {
    is_tty && BELL_ARMED.swap(false, Ordering::Relaxed)
}

/// Emit one terminal BEL (0x07) to stderr if armed and stderr is a TTY, then
/// disarm. Called the moment a prompt blocks for input. No-op off a TTY (keeps
/// CI / piped output clean) or when disarmed (so it rings once per arm).
pub fn maybe_ring_bell() {
    use std::io::{IsTerminal, Write};
    if take_bell(std::io::stderr().is_terminal()) {
        let mut err = std::io::stderr();
        let _ = err.write_all(b"\x07");
        let _ = err.flush();
    }
}

/// Read one logical line for an interactive prompt. Returns `Ok(None)` at
/// end of input. The returned string never includes the trailing newline.
///
/// Resolution order, highest priority first:
///
/// 1. A task-local [`PromptRoute`] ([`with_route`]), for a non-terminal
///    consumer such as the desktop app. Scoped to the current async task
///    (not the OS thread) so it stays attached to the one watch that
///    installed it even when tokio's scheduler resumes that task on a
///    different worker thread — see [`PromptRoute`]'s doc for why a
///    thread-local (or a process-global slot) can't make that guarantee.
/// 2. The process-global watch coordinator (coordinator [`activate`]d):
///    registers as the waiting prompt and blocks until the owner
///    delivers a line — it never touches the real stdin, so it cannot
///    deadlock against the owner.
/// 3. The real stdin, read directly.
///
/// The CLI never installs a route, so its path through steps 2 and 3 is
/// unchanged.
pub async fn read_line_coordinated() -> io::Result<Option<String>> {
    // Ring the watch attention bell the moment a prompt blocks for input.
    // EVERY coordinated prompt (conflict / remote-delete / destructive-delete
    // / delete-drift / MDH resolvers) funnels through here — directly or via
    // `CoordinatorStdin` — so this single call covers them all. No-op outside
    // watch (never armed) or off a TTY.
    maybe_ring_bell();
    // A task-local route (the desktop app) outranks the process-global
    // coordinator (`rdc sync --watch` on a TTY), which outranks real stdin.
    // The CLI never installs a route, so its path is unchanged.
    if let Ok(route) = ROUTE.try_with(|r| r.clone()) {
        let prompt = PENDING
            .try_with(|p| p.borrow_mut().take())
            .ok()
            .flatten()
            .unwrap_or_else(Prompt::unknown);
        return Ok(route.ask(&prompt).await);
    }
    if let Some(coord) = COORD.get() {
        return Ok(coord.recv_line());
    }
    let mut s = String::new();
    if io::stdin().read_line(&mut s)? == 0 {
        return Ok(None);
    }
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    Ok(Some(s))
}

/// An async source of answer lines for the resolvers. Any [`BufRead`]
/// is one (tests pass a `Cursor`); [`CoordinatorStdin`] is the live one.
/// `Ok(None)` is end of input; the line has no trailing newline.
pub trait LineSource {
    fn read_line(&mut self) -> crate::rt::BoxFuture<'_, io::Result<Option<String>>>;
}

impl<R: BufRead + Send> LineSource for R {
    fn read_line(&mut self) -> crate::rt::BoxFuture<'_, io::Result<Option<String>>> {
        Box::pin(async move {
            let mut s = String::new();
            if BufRead::read_line(self, &mut s)? == 0 {
                return Ok(None);
            }
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            Ok(Some(s))
        })
    }
}

/// A borrowed [`LineSource`], for passing `&mut R` to a resolver that takes
/// its source by value. (`&mut R` is not itself a `LineSource`: the blanket
/// impl over `BufRead` rules out a second one.)
pub struct Borrowed<'a, R: ?Sized>(pub &'a mut R);

impl<R: LineSource + ?Sized> LineSource for Borrowed<'_, R> {
    fn read_line(&mut self) -> crate::rt::BoxFuture<'_, io::Result<Option<String>>> {
        self.0.read_line()
    }
}

/// A [`LineSource`] over [`read_line_coordinated`], for the conflict /
/// remote-delete resolvers. Construction is cheap and acquires nothing —
/// the first read is what blocks/locks, so a conflict-free cycle that
/// never reads also never touches stdin.
pub struct CoordinatorStdin {
    /// Latched on the first end of input, so a resolver that reuses this
    /// source for every remaining item does not re-ask (a route would emit
    /// a fresh prompt each time; the coordinator could re-block).
    eof: bool,
}

impl CoordinatorStdin {
    pub fn new() -> Self {
        Self { eof: false }
    }
}

impl LineSource for CoordinatorStdin {
    fn read_line(&mut self) -> crate::rt::BoxFuture<'_, io::Result<Option<String>>> {
        Box::pin(async move {
            if self.eof {
                return Ok(None);
            }
            let line = read_line_coordinated().await?;
            self.eof = line.is_none();
            Ok(line)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn try_deliver_returns_line_when_no_prompt_waiting() {
        let coord = StdinCoordinator::new();
        assert_eq!(coord.try_deliver("hello".into()), Err("hello".into()));
    }

    #[test]
    fn delivers_to_waiting_prompt() {
        let coord = Arc::new(StdinCoordinator::new());

        // A prompt thread registers and blocks for one line.
        let c2 = coord.clone();
        let handle = std::thread::spawn(move || c2.recv_line());

        // Wait until the prompt has registered as the waiting sink.
        loop {
            if coord.waiting.lock().unwrap().is_some() {
                break;
            }
            std::thread::yield_now();
        }

        assert!(coord.try_deliver("answer".into()).is_ok());
        assert_eq!(handle.join().unwrap(), Some("answer".to_string()));

        // After the prompt consumed its line it unregistered, so a further
        // line falls through to the Enter-trigger path.
        assert_eq!(coord.try_deliver("next".into()), Err("next".into()));
    }

    #[test]
    fn bell_arm_take_debounce_rearm_and_tty_gate() {
        // This is the only test that touches the process-global BELL_ARMED,
        // so the sequence below is race-free against the rest of the suite.
        // Off-TTY must never consume the armed flag (so a later TTY read still
        // rings).
        arm_bell();
        assert!(!take_bell(false), "off-TTY must not ring");
        assert!(take_bell(true), "armed + TTY rings once");
        assert!(!take_bell(true), "debounced after the first ring");
        // Re-arming rings again.
        arm_bell();
        assert!(take_bell(true), "re-arm rings");
        assert!(!take_bell(true), "debounced again");
        // Unarmed is silent.
        assert!(!take_bell(true), "unarmed is silent");
    }

    struct Canned {
        answer: String,
        seen: Mutex<Vec<Prompt>>,
    }
    impl PromptRoute for Canned {
        fn ask<'a>(&'a self, prompt: &'a Prompt) -> crate::rt::BoxFuture<'a, Option<String>> {
            self.seen.lock().unwrap().push(prompt.clone());
            Box::pin(async move { Some(self.answer.clone()) })
        }
    }

    struct SlowYes;
    impl PromptRoute for SlowYes {
        fn ask<'a>(&'a self, _prompt: &'a Prompt) -> crate::rt::BoxFuture<'a, Option<String>> {
            Box::pin(async {
                crate::rt::sleep(std::time::Duration::from_millis(1)).await;
                Some("y".to_string())
            })
        }
    }

    #[tokio::test]
    async fn async_route_answers_through_await() {
        let got = with_route(Arc::new(SlowYes), read_line_coordinated()).await;
        assert_eq!(got.unwrap(), Some("y".to_string()));
    }

    struct CountingNone(std::sync::atomic::AtomicUsize);
    impl PromptRoute for CountingNone {
        fn ask<'a>(&'a self, _prompt: &'a Prompt) -> crate::rt::BoxFuture<'a, Option<String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        }
    }

    #[tokio::test]
    async fn coordinator_stdin_latches_end_of_input() {
        let route = Arc::new(CountingNone(std::sync::atomic::AtomicUsize::new(0)));
        with_route(route.clone(), async {
            let mut src = CoordinatorStdin::new();
            assert_eq!(LineSource::read_line(&mut src).await.unwrap(), None);
            assert_eq!(LineSource::read_line(&mut src).await.unwrap(), None);
        })
        .await;
        assert_eq!(route.0.load(Ordering::SeqCst), 1, "route asked again after EOF");
    }

    #[tokio::test]
    async fn a_buf_read_is_a_line_source() {
        let mut src = std::io::Cursor::new(b"k\r\nlast".to_vec());
        assert_eq!(LineSource::read_line(&mut src).await.unwrap(), Some("k".into()));
        assert_eq!(LineSource::read_line(&mut src).await.unwrap(), Some("last".into()));
        assert_eq!(LineSource::read_line(&mut src).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_installed_route_answers_and_sees_the_announced_prompt() {
        let route = Arc::new(Canned {
            answer: "k".into(),
            seen: Mutex::new(Vec::new()),
        });
        with_route(route.clone(), async {
            announce(Prompt {
                kind: PromptKind::DeleteGate,
                question: "Proceed with deletion? [y/N] ".into(),
                keys: vec![PromptKey::new('y', "yes"), PromptKey::new('n', "no")],
            });
            assert_eq!(read_line_coordinated().await.unwrap(), Some("k".to_string()));
        })
        .await;
        let seen = route.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, PromptKind::DeleteGate);
        assert_eq!(seen[0].keys.len(), 2);
    }

    #[tokio::test]
    async fn a_pending_prompt_is_consumed_once() {
        let route = Arc::new(Canned {
            answer: "s".into(),
            seen: Mutex::new(Vec::new()),
        });
        with_route(route.clone(), async {
            announce(Prompt {
                kind: PromptKind::Conflict,
                question: "q".into(),
                keys: vec![PromptKey::new('s', "skip")],
            });
            let _ = read_line_coordinated().await.unwrap();
            let _ = read_line_coordinated().await.unwrap();
        })
        .await;
        let seen = route.seen.lock().unwrap();
        // Second read saw the fallback, not a stale copy of the first.
        assert_eq!(seen[0].question, "q");
        assert_eq!(seen[1].question, "");
        assert_eq!(seen[1].kind, PromptKind::Unknown);
    }

    /// The constraint that ruled out the process-global coordinator, now
    /// proven against concurrent TASKS rather than OS threads: N watches
    /// must be able to prompt at the same time, surviving tokio's
    /// work-stealing scheduler moving any of them to a different worker
    /// thread mid-await, without an answer ever landing on the wrong watch.
    ///
    /// Multi-thread runtime with 2 workers is deliberate — a
    /// current-thread runtime can never reschedule a task onto a different
    /// OS thread, so it would pass trivially and prove nothing. 6 tasks
    /// (more than the 2 workers) on 2 workers is also deliberate: with
    /// exactly as many tasks as workers, each worker can just keep running
    /// its own task locally forever and never needs to steal — no
    /// migration occurs, and the test passes even against the OLD
    /// `thread_local!` storage this is supposed to falsify (verified: see
    /// the task 8 fix-round-1 report). Oversubscribing forces real
    /// work-stealing pressure. Each task sleeps (not just `yield_now`, which
    /// tokio's LIFO slot can satisfy without ever touching another worker's
    /// queue) between `announce` and the read specifically to give the
    /// scheduler a chance to migrate it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn routes_do_not_cross_talk_between_concurrent_tasks() {
        const N: usize = 6;
        let routes: Vec<Arc<Canned>> = (0..N)
            .map(|i| {
                Arc::new(Canned {
                    answer: i.to_string(),
                    seen: Mutex::new(Vec::new()),
                })
            })
            .collect();

        let handles: Vec<_> = routes
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, route)| {
                tokio::spawn(with_route(route, async move {
                    announce(Prompt {
                        kind: PromptKind::Conflict,
                        question: format!("from-{i}"),
                        keys: vec![],
                    });
                    for _ in 0..20 {
                        tokio::time::sleep(std::time::Duration::from_micros(50)).await;
                    }
                    read_line_coordinated().await.unwrap()
                }))
            })
            .collect();

        for (i, h) in handles.into_iter().enumerate() {
            assert_eq!(
                h.await.unwrap(),
                Some(i.to_string()),
                "task {i} got the wrong answer"
            );
        }
        for (i, route) in routes.iter().enumerate() {
            assert_eq!(
                route.seen.lock().unwrap()[0].question,
                format!("from-{i}"),
                "route {i} was asked the wrong question"
            );
        }
    }
}
