//! Drive the `rdc` binary through a pseudo-terminal, so a test can answer its
//! prompts the way a person does.
//!
//! Piping stdin is not enough: `resolve::is_interactive` asks for a terminal on
//! BOTH stdin and stderr, and without one `rdc sync` never prompts at all — it
//! parks conflicts as shadows and refuses deletes. Every end-to-end test that
//! only pipes therefore exercises the non-interactive path and nothing else.
//!
//! Unix only, because it calls `openpty(3)` directly rather than pulling in a
//! crate for the thirty lines that needs. Output is captured with the
//! terminal's own echo, so a transcript reads like the screen did.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Long enough for a debug-build sync against the fake org on a loaded CI
/// runner; short enough that a prompt rdc never shows fails the test instead
/// of hanging it.
const TIMEOUT: Duration = Duration::from_secs(60);

type Buf = Arc<(Mutex<Vec<u8>>, Condvar)>;

pub struct PtySession {
    child: Child,
    master: File,
    out: Buf,
    /// Byte offset up to which [`Self::expect`] has consumed the output.
    seen: usize,
    reader: Option<JoinHandle<()>>,
}

fn cloexec(fd: i32) {
    // SAFETY: plain fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

impl PtySession {
    /// Spawn `rdc <args>` in `cwd` with stdin, stdout and stderr all on the
    /// slave side of a fresh PTY. `NO_COLOR` is set so transcripts carry no
    /// escape codes.
    pub fn spawn(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> PtySession {
        let (mut m, mut s) = (0, 0);
        // Wide, so no prompt line wraps and a needle never straddles a break.
        let mut ws = libc::winsize { ws_row: 60, ws_col: 240, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: out-pointers are valid; a null name and termios are allowed.
        // `&raw mut`, not `&mut`: glibc takes `*const winsize` and macOS
        // `*mut`, and clippy rejects `&mut ws` where only `*const` is needed.
        let rc = unsafe {
            libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut ws)
        };
        assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
        // Neither end may leak into the child beyond the three dup'd slaves:
        // an inherited master or slave keeps the PTY open after rdc exits, and
        // the reader below would then never see end-of-file.
        cloexec(m);
        cloexec(s);
        // SAFETY: openpty just returned these descriptors and nothing else owns them.
        let (master, slave) = unsafe { (File::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };

        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("rdc"));
        cmd.current_dir(cwd)
            .args(args)
            .env("NO_COLOR", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawning rdc under a pty");
        // Drop the parent's copy, so the child's exit is the last close.
        drop(slave);

        let out: Buf = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let mut reader_end = master.try_clone().unwrap();
        let sink = out.clone();
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            // Linux answers EIO once the last slave closes; macOS answers 0.
            while let Ok(n) = reader_end.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let (lock, cv) = &*sink;
                lock.lock().unwrap().extend_from_slice(&chunk[..n]);
                cv.notify_all();
            }
        });
        PtySession { child, master, out, seen: 0, reader: Some(reader) }
    }

    /// Everything rdc has written so far, as text, with the PTY's `\r\n`
    /// line endings folded to `\n`.
    pub fn transcript(&self) -> String {
        let bytes = self.out.0.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).replace("\r\n", "\n")
    }

    /// Block until `needle` appears in output not yet consumed by an earlier
    /// `expect`, and consume through it. A needle must not contain a newline:
    /// the terminal writes `\r\n`. Panics with the transcript on timeout or
    /// exit, so a prompt that never came says what came instead.
    pub fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        let mut exited_at: Option<Instant> = None;
        loop {
            {
                let (lock, cv) = &*self.out;
                let buf = lock.lock().unwrap();
                if let Some(pos) = find_bytes(&buf[self.seen..], needle.as_bytes()) {
                    self.seen += pos + needle.len();
                    return;
                }
                drop(cv.wait_timeout(buf, Duration::from_millis(100)).unwrap());
            }
            // After the child exits, give the reader a moment to drain what
            // it wrote last before calling the needle missing.
            if exited_at.is_none() && self.child.try_wait().ok().flatten().is_some() {
                exited_at = Some(Instant::now());
            }
            let drained = exited_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500));
            if drained || Instant::now() >= deadline {
                panic!("never saw {needle:?} on the terminal. Transcript:\n{}", self.transcript());
            }
        }
    }

    /// Type `line` and press Enter.
    pub fn send_line(&mut self, line: &str) {
        self.master.write_all(line.as_bytes()).unwrap();
        self.master.write_all(b"\r").unwrap();
        self.master.flush().unwrap();
    }

    /// Press Ctrl-D on an empty line: end-of-file for a canonical-mode read.
    pub fn send_eof(&mut self) {
        self.master.write_all(&[0x04]).unwrap();
        self.master.flush().unwrap();
    }

    /// Wait for rdc to exit and return its status and the full transcript.
    pub fn wait(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                break st;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("rdc did not exit. Transcript:\n{}", self.transcript());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        (status, self.transcript())
    }
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
