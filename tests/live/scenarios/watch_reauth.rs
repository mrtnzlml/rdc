//! Issue #2: under `rdc sync --watch` on a terminal, the 401 token prompt
//! used to read fd 0 itself (inquire) and race the watch loop's
//! always-running stdin reader, which could take the whole pasted line.
//!
//! Unix only: the PTY is `openpty(3)`.

#![cfg(unix)]

use std::time::{Duration, Instant};

use super::cli_matrix::LabelFx;
use crate::support::pty::PtySession;

const PROMPT: &str = "New API token";
const SAVED: &str = "saved token to";
const REJECTED: &str = "rejected by server (401)";

/// Wait until the transcript, from byte `from`, shows one of `needles`.
fn wait_any(s: &PtySession, from: usize, needles: &[&'static str], t: Duration) -> Option<&'static str> {
    let deadline = Instant::now() + t;
    while Instant::now() < deadline {
        let tr = s.transcript();
        let tail = tr.get(from..).unwrap_or("");
        if let Some(n) = needles.iter().find(|n| tail.contains(**n)) {
            return Some(n);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_401_prompt_receives_the_pasted_token() {
    let fx = LabelFx::new().await;
    let mut s = PtySession::spawn(fx.project.path(), &["sync", "test", "--watch", "--no-poll", "--no-bell"], &[]);
    s.expect("polling disabled");
    // Let the stdin reader task start.
    std::thread::sleep(Duration::from_secs(1));

    // The token "expires" mid-watch.
    fx.project.write_json("secrets/test.secrets.json", &serde_json::json!({ "api_token": "expired" }));
    // Enter on an idle watch starts a cycle; that cycle gets a 401.
    s.send_line("");
    s.expect(PROMPT);
    s.expect("empty line cancels): ");

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(300));
        let mark = s.transcript().len();
        s.send_line("fake-token");
        let got = wait_any(&s, mark, &[SAVED, REJECTED], Duration::from_secs(5));
        outcomes.push(got.unwrap_or("lost"));
        if got == Some(SAVED) {
            break;
        }
    }

    let transcript = s.kill();
    assert!(!transcript.contains("fake-token"), "the token was echoed:\n{transcript}");
    assert_eq!(
        outcomes.first(),
        Some(&SAVED),
        "pasting the valid token did not succeed first time; outcomes: {outcomes:?}\n--- terminal ---\n{transcript}"
    );
}
