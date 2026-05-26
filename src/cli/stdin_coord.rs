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

use std::io::{self, BufRead, Read};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

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

/// Read one logical line for an interactive prompt. Returns `Ok(None)` at
/// end of input. The returned string never includes the trailing newline.
///
/// In watch mode (coordinator [`activate`]d) this registers as the waiting
/// prompt and blocks until the owner delivers a line — it never touches the
/// real stdin, so it cannot deadlock against the owner. Otherwise it reads
/// the real stdin directly.
pub fn read_line_coordinated() -> io::Result<Option<String>> {
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
}
