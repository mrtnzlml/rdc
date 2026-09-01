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
use std::io::{self, BufRead, Read};
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
/// announced). `PushDrift` is named ahead of its site: the push-drift
/// prompts (`resolve_push_drift`, and the mid-cycle drift check in
/// `pull/common.rs`) both route through the shared conflict resolver and
/// are announced as `Conflict` — same decision, same keys — so nothing
/// constructs `PushDrift` yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Conflict,
    RemoteDelete,
    #[allow(dead_code)]
    PushDrift,
    BulkConfirm,
    DeleteGate,
    DeleteDrift,
    MdhIndexDrop,
    MdhRowDelete,
    /// A coordinated read whose site never called [`announce`]. Should be
    /// unreachable in a correctly wired build — every resolver announces
    /// before it reads — so it exists to make a missed announce loud (a
    /// visibly wrong dialog) rather than silently disguised as one of the
    /// real decisions above.
    Unknown,
}

/// What a blocked prompt is asking, in machine-readable form. `question` is
/// the same string the terminal shows, trailing `"> "` and all.
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
/// when the consumer is not a terminal. Installed per thread, because a
/// cycle never leaves its thread (engine concurrency is `buffer_unordered`
/// on the current task, never `tokio::spawn` — see `api::mod`) and the
/// process-global [`COORD`] has a single waiting slot, so two concurrent
/// watches sharing it would hang the first one.
pub trait PromptRoute: Send + Sync {
    /// Block until the consumer answers. `None` means end of input; every
    /// resolver already degrades safely on that (conflicts skip, gates
    /// read as `N`).
    fn ask(&self, prompt: &Prompt) -> Option<String>;
}

thread_local! {
    static ROUTE: RefCell<Option<Arc<dyn PromptRoute>>> = const { RefCell::new(None) };
    static PENDING: RefCell<Option<Prompt>> = const { RefCell::new(None) };
}

/// Install `route` for the calling thread until the returned guard drops.
///
/// Nothing in the CLI calls this — it is installed by a non-terminal
/// consumer (the desktop bridge), which lands in a later task of this
/// plan. `dead_code` is denied workspace-wide, hence the explicit allow
/// rather than leaving this half-wired route unbuildable in the meantime.
#[allow(dead_code)]
#[must_use = "the route is uninstalled when the guard drops"]
pub fn install_route(route: Arc<dyn PromptRoute>) -> RouteGuard {
    ROUTE.with(|r| *r.borrow_mut() = Some(route));
    RouteGuard(())
}

/// Uninstalls the calling thread's route (and drops any pending
/// announcement) when this drops, so a route's lifetime bounds exactly
/// one watch's prompts.
///
/// Nothing in the CLI constructs one yet — Task 8 wires the desktop
/// bridge to hold this guard for the lifetime of a watch. `dead_code` is
/// denied workspace-wide, hence the explicit allow.
#[allow(dead_code)]
pub struct RouteGuard(());

impl Drop for RouteGuard {
    fn drop(&mut self) {
        ROUTE.with(|r| *r.borrow_mut() = None);
        PENDING.with(|p| *p.borrow_mut() = None);
    }
}

/// Declare what the next coordinated read is asking. Call immediately
/// before writing the question. A no-op when no route is installed, which
/// is every CLI invocation.
pub fn announce(p: Prompt) {
    if ROUTE.with(|r| r.borrow().is_some()) {
        PENDING.with(|slot| *slot.borrow_mut() = Some(p));
    }
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
/// 1. A thread-local [`PromptRoute`] ([`install_route`]), for a
///    non-terminal consumer such as the desktop app. Per-thread rather
///    than process-global because a cycle never leaves its thread, so
///    this is exactly one route per watch — see [`PromptRoute`]'s doc for
///    why a process-global slot can't make that guarantee.
/// 2. The process-global watch coordinator (coordinator [`activate`]d):
///    registers as the waiting prompt and blocks until the owner
///    delivers a line — it never touches the real stdin, so it cannot
///    deadlock against the owner.
/// 3. The real stdin, read directly.
///
/// The CLI never installs a route, so its path through steps 2 and 3 is
/// unchanged.
pub fn read_line_coordinated() -> io::Result<Option<String>> {
    // Ring the watch attention bell the moment a prompt blocks for input.
    // EVERY coordinated prompt (conflict / remote-delete / destructive-delete
    // / delete-drift / MDH resolvers) funnels through here — directly or via
    // `CoordinatorStdin` — so this single call covers them all. No-op outside
    // watch (never armed) or off a TTY.
    maybe_ring_bell();
    // A thread-local route (the desktop app) outranks the process-global
    // coordinator (`rdc sync --watch` on a TTY), which outranks real stdin.
    // The CLI never installs a route, so its path is unchanged.
    if let Some(route) = ROUTE.with(|r| r.borrow().clone()) {
        let prompt = PENDING
            .with(|p| p.borrow_mut().take())
            .unwrap_or_else(Prompt::unknown);
        return Ok(route.ask(&prompt));
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

/// A [`BufRead`] adapter over [`read_line_coordinated`], for the conflict /
/// remote-delete resolvers (which take a generic `BufRead`). Each delivered
/// line is re-terminated with `\n` so `BufRead::read_line` sees normal line
/// semantics. Construction is cheap and acquires nothing — the first read
/// is what blocks/locks, so a conflict-free cycle that never reads also
/// never touches stdin.
pub struct CoordinatorStdin {
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl CoordinatorStdin {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            eof: false,
        }
    }
}

impl Read for CoordinatorStdin {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = {
            let avail = self.fill_buf()?;
            let n = avail.len().min(out.len());
            out[..n].copy_from_slice(&avail[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for CoordinatorStdin {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.buf.len() && !self.eof {
            match read_line_coordinated()? {
                Some(mut line) => {
                    line.push('\n');
                    self.buf = line.into_bytes();
                    self.pos = 0;
                }
                None => {
                    self.eof = true;
                    self.buf.clear();
                    self.pos = 0;
                }
            }
        }
        Ok(&self.buf[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.buf.len());
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

    #[test]
    fn coordinator_stdin_buffers_one_delivered_line_with_newline() {
        // Drive `CoordinatorStdin` purely through its buffer by pre-seeding
        // it (the global path is exercised in integration). Verifies the
        // BufRead line semantics the resolvers rely on.
        let mut cs = CoordinatorStdin::new();
        cs.buf = b"k\n".to_vec();
        let mut line = String::new();
        let n = cs.read_line(&mut line).unwrap();
        assert_eq!(n, 2);
        assert_eq!(line, "k\n");
        // Buffer exhausted; without a global owner this would read real
        // stdin, so we don't call read_line again here.
        assert_eq!(cs.pos, cs.buf.len());
    }

    struct Canned {
        answer: String,
        seen: Mutex<Vec<Prompt>>,
    }
    impl PromptRoute for Canned {
        fn ask(&self, prompt: &Prompt) -> Option<String> {
            self.seen.lock().unwrap().push(prompt.clone());
            Some(self.answer.clone())
        }
    }

    #[test]
    fn an_installed_route_answers_and_sees_the_announced_prompt() {
        let route = Arc::new(Canned {
            answer: "k".into(),
            seen: Mutex::new(Vec::new()),
        });
        let guard = install_route(route.clone());
        announce(Prompt {
            kind: PromptKind::DeleteGate,
            question: "Proceed with deletion? [y/N] ".into(),
            keys: vec![PromptKey::new('y', "yes"), PromptKey::new('n', "no")],
        });
        assert_eq!(read_line_coordinated().unwrap(), Some("k".to_string()));
        let seen = route.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, PromptKind::DeleteGate);
        assert_eq!(seen[0].keys.len(), 2);
        drop(guard);
    }

    #[test]
    fn a_pending_prompt_is_consumed_once() {
        let route = Arc::new(Canned {
            answer: "s".into(),
            seen: Mutex::new(Vec::new()),
        });
        let guard = install_route(route.clone());
        announce(Prompt {
            kind: PromptKind::Conflict,
            question: "q".into(),
            keys: vec![PromptKey::new('s', "skip")],
        });
        let _ = read_line_coordinated().unwrap();
        let _ = read_line_coordinated().unwrap();
        let seen = route.seen.lock().unwrap();
        // Second read saw the fallback, not a stale copy of the first.
        assert_eq!(seen[0].question, "q");
        assert_eq!(seen[1].question, "");
        assert_eq!(seen[1].kind, PromptKind::Unknown);
        drop(guard);
    }

    /// The constraint that ruled out the process-global coordinator: two
    /// watches must be able to prompt at the same time without either
    /// answer landing on the wrong thread.
    #[test]
    fn routes_are_per_thread_and_do_not_cross_talk() {
        let a = Arc::new(Canned {
            answer: "A".into(),
            seen: Mutex::new(Vec::new()),
        });
        let b = Arc::new(Canned {
            answer: "B".into(),
            seen: Mutex::new(Vec::new()),
        });

        let (a2, b2) = (a.clone(), b.clone());
        let ta = std::thread::spawn(move || {
            let _g = install_route(a2);
            announce(Prompt {
                kind: PromptKind::Conflict,
                question: "from-a".into(),
                keys: vec![],
            });
            read_line_coordinated().unwrap()
        });
        let tb = std::thread::spawn(move || {
            let _g = install_route(b2);
            announce(Prompt {
                kind: PromptKind::Conflict,
                question: "from-b".into(),
                keys: vec![],
            });
            read_line_coordinated().unwrap()
        });

        assert_eq!(ta.join().unwrap(), Some("A".to_string()));
        assert_eq!(tb.join().unwrap(), Some("B".to_string()));
        assert_eq!(a.seen.lock().unwrap()[0].question, "from-a");
        assert_eq!(b.seen.lock().unwrap()[0].question, "from-b");
    }
}
