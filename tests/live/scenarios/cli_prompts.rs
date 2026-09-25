//! Every answer to every `rdc sync` prompt, typed into a real terminal.
//!
//! The prompt functions are unit-tested with scripted readers, and their bytes
//! are pinned (`testdata/prompt_pins/`). Neither shows what a KEY DOES once the
//! whole binary runs: which file changes, what reaches the env, what the next
//! sync sees. Each case here starts rdc on a PTY (`support::pty`), waits for a
//! prompt, types the answer, and checks the project and the fake org after.
//!
//! Unix only: the PTY is `openpty(3)`.

#![cfg(unix)]

use super::cli_matrix::{LabelFx, State, LOCAL, RED, REMOTE};
use crate::support::pty::PtySession;

/// The last line of every prompt menu this file answers.
const MENU_END: &str = "[a] abort the sync";
/// The last words of the delete-gate question.
const GATE_END: &str = "(default)";

/// A key to type after a needle appears. `EOF` presses Ctrl-D instead.
type Script = &'static [(&'static str, &'static str)];

/// What must be true once rdc exits. `None` = not checked.
#[derive(Default, Clone)]
struct After {
    ok: Option<bool>,
    remote: Option<Option<&'static str>>,
    local: Option<Option<&'static str>>,
    shadow: Option<bool>,
    marker: Option<bool>,
    /// Text the transcript must contain.
    says: &'static [&'static str],
    /// Text the transcript must not contain.
    never_says: &'static [&'static str],
}

/// An editor that takes the local side of the conflict buffer and changes
/// its colour, so an `[e]` result is distinguishable from `[k]`.
fn fake_editor(dir: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-editor.sh");
    std::fs::write(
        &p,
        "#!/bin/sh\n\
         sed -e '/^=======/,/^>>>>>>>/d' -e '/^<<<<<<</d' -e 's/#111111/#444444/' \"$1\" > \"$1.tmp\" \
         && mv \"$1.tmp\" \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

/// Run `rdc sync test <flags>` on a terminal, answer per `script`, and return
/// the fixture (for its state) with the exit status and transcript.
async fn drive(fx: &LabelFx, flags: &[&str], script: Script) -> (bool, String) {
    let editor = fake_editor(fx.project.path());
    let mut args = vec!["sync", "test"];
    args.extend_from_slice(flags);
    let mut s = PtySession::spawn(fx.project.path(), &args, &[("EDITOR", &editor)]);
    for (needle, answer) in script {
        s.expect(needle);
        s.expect("> ");
        if *answer == "EOF" {
            s.send_eof();
        } else {
            s.send_line(answer);
        }
    }
    let (status, transcript) = s.wait();
    (status.success(), transcript)
}

async fn check(state: State, flags: &[&str], script: Script, after: After) -> Option<String> {
    let fx = LabelFx::new().await;
    fx.apply(state).await;
    let (ok, t) = drive(&fx, flags, script).await;
    let mut problems = Vec::new();
    if let Some(want) = after.ok
        && want != ok
    {
        problems.push(format!("exit ok: want {want}, got {ok}"));
    }
    if let Some(want) = after.remote {
        let got = fx.remote().await;
        if want != got.as_deref() {
            problems.push(format!("remote: want {want:?}, got {got:?}"));
        }
    }
    if let Some(want) = after.local {
        let got = fx.local();
        if want != got.as_deref() {
            problems.push(format!("local: want {want:?}, got {got:?}"));
        }
    }
    if let Some(want) = after.shadow {
        let got = fx.project.exists(&fx.shadow);
        if want != got {
            problems.push(format!("shadow: want {want}, got {got}"));
        }
    }
    if let Some(want) = after.marker {
        let got = fx.project.exists(&fx.marker);
        if want != got {
            problems.push(format!("marker: want {want}, got {got}"));
        }
    }
    for s in after.says {
        if !t.contains(s) {
            problems.push(format!("transcript lacks {s:?}"));
        }
    }
    for s in after.never_says {
        if t.contains(s) {
            problems.push(format!("transcript shows {s:?}"));
        }
    }
    if problems.is_empty() {
        return None;
    }
    let shown: Vec<&str> = t.lines().filter(|l| !l.contains(" list ")).collect();
    Some(format!(
        "{state:?} {flags:?} answering {script:?}\n    {}\n    --- terminal ---\n{}",
        problems.join("\n    "),
        shown.join("\n")
    ))
}

fn report(failures: Vec<Option<String>>) {
    let failures: Vec<String> = failures.into_iter().flatten().collect();
    assert!(failures.is_empty(), "{} case(s) wrong:\n\n{}", failures.len(), failures.join("\n\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflict_prompt_answers() {
    // The conflict waits: local kept, env untouched, env's copy shadowed.
    let parked = || After {
        ok: Some(true),
        local: Some(Some(LOCAL)),
        remote: Some(Some(REMOTE)),
        shadow: Some(true),
        ..After::default()
    };
    report(vec![
        check(State::Conflict, &[], &[(MENU_END, "k")], After {
            ok: Some(true),
            local: Some(Some(LOCAL)),
            remote: Some(Some(LOCAL)),
            shadow: Some(false),
            ..After::default()
        })
        .await,
        check(State::Conflict, &[], &[(MENU_END, "r")], After {
            ok: Some(true),
            local: Some(Some(REMOTE)),
            remote: Some(Some(REMOTE)),
            shadow: Some(false),
            ..After::default()
        })
        .await,
        check(State::Conflict, &[], &[(MENU_END, "e")], After {
            ok: Some(true),
            local: Some(Some("#444444")),
            remote: Some(Some("#444444")),
            shadow: Some(false),
            ..After::default()
        })
        .await,
        check(State::Conflict, &[], &[(MENU_END, "s")], parked()).await,
        // Ctrl-D is "decide later", never a silent pick.
        check(State::Conflict, &[], &[(MENU_END, "EOF")], parked()).await,
        // An unknown key and a bare Enter both ask again.
        check(State::Conflict, &[], &[(MENU_END, "z"), ("unrecognized", "s")], parked()).await,
        check(State::Conflict, &[], &[(MENU_END, ""), ("unrecognized", "s")], parked()).await,
        check(State::Conflict, &[], &[(MENU_END, "a")], After {
            ok: Some(false),
            local: Some(Some(LOCAL)),
            remote: Some(Some(REMOTE)),
            shadow: Some(false),
            says: &["aborted"],
            ..After::default()
        })
        .await,
        // With one conflict there is no bulk choice, so `K` is plain `k`.
        check(State::Conflict, &[], &[(MENU_END, "K")], After {
            ok: Some(true),
            remote: Some(Some(LOCAL)),
            never_says: &["keep ALL local"],
            ..After::default()
        })
        .await,
        // --no-push: the answer is taken, the push is not made.
        check(State::Conflict, &["--no-push"], &[(MENU_END, "k")], After {
            ok: Some(true),
            local: Some(Some(LOCAL)),
            remote: Some(Some(REMOTE)),
            ..After::default()
        })
        .await,
        // --conflict overrides the resolver even on a terminal.
        check(State::Conflict, &["--conflict", "skip"], &[], After {
            never_says: &[MENU_END],
            ..parked()
        })
        .await,
        check(State::Conflict, &["--conflict", "use-remote"], &[], After {
            ok: Some(true),
            local: Some(Some(REMOTE)),
            never_says: &[MENU_END],
            ..After::default()
        })
        .await,
        // --yes means "no terminal", so it parks rather than prompting.
        check(State::Conflict, &["--yes"], &[], After { never_says: &[MENU_END], ..parked() }).await,
    ]);
}

/// Two labels in conflict, answered with the bulk keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bulk_conflict_answers() {
    let cases: &[(Script, &str, &str)] = &[
        // (answers, first label's final colour everywhere, second's)
        (&[(MENU_END, "K"), ("[n] no", "y")], LOCAL, LOCAL),
        (&[(MENU_END, "R"), ("[n] no", "y")], REMOTE, REMOTE),
        // Declining the bulk confirmation falls back to one at a time.
        (&[(MENU_END, "K"), ("[n] no", "n"), (MENU_END, "k"), (MENU_END, "r")], LOCAL, REMOTE),
        (&[(MENU_END, "R"), ("[n] no", ""), (MENU_END, "r"), (MENU_END, "k")], REMOTE, LOCAL),
    ];
    let mut failures = Vec::new();
    for (script, first, second) in cases {
        let fx = LabelFx::with_labels(&["Alpha", "Beta"]).await;
        let lf = crate::support::assert_local::load_lockfile(fx.project.path(), "test").unwrap();
        let beta_id = lf.objects["labels"]["beta"].id;
        let beta_rel = "envs/test/labels/beta.json";
        fx.set_local(LOCAL);
        fx.set_local_at(beta_rel, LOCAL);
        fx.set_remote(REMOTE).await;
        fx.set_remote_id(beta_id, REMOTE).await;

        let (ok, t) = drive(&fx, &[], script).await;
        let got = (
            ok,
            fx.local(),
            fx.remote().await,
            fx.local_at(beta_rel),
            fx.remote_id(beta_id).await,
        );
        let want = (
            true,
            Some(first.to_string()),
            Some(first.to_string()),
            Some(second.to_string()),
            Some(second.to_string()),
        );
        if got != want {
            failures.push(format!("answering {script:?}\n    want {want:?}\n    got  {got:?}\n{t}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Local edited, env deleted it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_vs_remote_delete_answers() {
    let s = State::LocalEditRemoteDelete;
    let says = &["deleted on test", "[k] keep local (restore it on test)", "[r] use test (delete local)"];
    report(vec![
        // Restore on the env: the old object is gone, so it comes back new.
        check(s, &[], &[(MENU_END, "k")], After {
            ok: Some(true),
            local: Some(Some(LOCAL)),
            marker: Some(false),
            says: &["post   label/priority"],
            ..After::default()
        })
        .await,
        check(s, &[], &[(MENU_END, "r")], After {
            ok: Some(true),
            local: Some(None),
            remote: Some(None),
            marker: Some(false),
            says,
            ..After::default()
        })
        .await,
        check(s, &[], &[(MENU_END, "s")], After {
            ok: Some(true),
            local: Some(Some(LOCAL)),
            remote: Some(None),
            marker: Some(true),
            ..After::default()
        })
        .await,
        check(s, &[], &[(MENU_END, "a")], After {
            ok: Some(false),
            local: Some(Some(LOCAL)),
            marker: Some(false),
            ..After::default()
        })
        .await,
        // --no-push: nothing is re-created on the env this run.
        check(s, &["--no-push"], &[(MENU_END, "k")], After {
            ok: Some(true),
            local: Some(Some(LOCAL)),
            never_says: &["post   label"],
            ..After::default()
        })
        .await,
    ]);
}

/// Local deleted, env edited it. The prompt must describe THIS case, not
/// reuse the remote-delete wording: here the env still has the object and
/// `[k]` keeps the LOCAL deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_delete_vs_remote_edit_answers() {
    let s = State::LocalDeleteRemoteEdit;
    let wording = || After {
        says: &[
            "changed on test, deleted locally",
            "[k] keep local (delete it on test)",
            "[r] use test (restore local)",
        ],
        never_says: &["deleted on test", "restore it on test", "(delete local)"],
        ..After::default()
    };
    report(vec![
        check(s, &[], &[(MENU_END, "r")], After {
            ok: Some(true),
            local: Some(Some(REMOTE)),
            remote: Some(Some(REMOTE)),
            marker: Some(false),
            ..wording()
        })
        .await,
        check(s, &[], &[(MENU_END, "s")], After {
            ok: Some(true),
            remote: Some(Some(REMOTE)),
            marker: Some(true),
            ..wording()
        })
        .await,
        // The tombstone stands; the env keeps its copy until a delete is
        // authorised.
        check(s, &[], &[(MENU_END, "k")], After {
            ok: Some(true),
            local: Some(None),
            remote: Some(Some(REMOTE)),
            ..wording()
        })
        .await,
        check(s, &[], &[(MENU_END, "a")], After { ok: Some(false), remote: Some(Some(REMOTE)), ..wording() })
            .await,
    ]);
}

/// The one `[y/N]` question before remote DELETEs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_gate_answers() {
    let deleted = || After { ok: Some(true), remote: Some(None), local: Some(None), ..After::default() };
    let kept = || After {
        ok: Some(true),
        remote: Some(Some(RED)),
        local: Some(None),
        says: &["remote unchanged"],
        ..After::default()
    };
    let s = State::LocalDelete;
    report(vec![
        check(s, &[], &[(GATE_END, "y")], deleted()).await,
        check(s, &[], &[(GATE_END, "Y")], deleted()).await,
        check(s, &[], &[(GATE_END, "yes")], deleted()).await,
        // Anything but yes is no: Enter is the stated default.
        check(s, &[], &[(GATE_END, "n")], kept()).await,
        check(s, &[], &[(GATE_END, "")], kept()).await,
        check(s, &[], &[(GATE_END, "EOF")], kept()).await,
        check(s, &[], &[(GATE_END, "sure")], kept()).await,
        // --allow-deletes answers it in advance.
        check(s, &["--allow-deletes"], &[], After { never_says: &[GATE_END], ..deleted() }).await,
        // --no-push has nothing to ask about.
        check(s, &["--no-push"], &[], After {
            ok: Some(true),
            remote: Some(Some(RED)),
            never_says: &[GATE_END],
            ..After::default()
        })
        .await,
        // --yes is not consent to delete.
        check(s, &["--yes"], &[], After {
            ok: Some(false),
            remote: Some(Some(RED)),
            says: &["--allow-deletes"],
            never_says: &[GATE_END],
            ..After::default()
        })
        .await,
    ]);
}

/// A clean remote delete never asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_remote_delete_does_not_prompt() {
    report(vec![
        check(State::RemoteDelete, &[], &[], After {
            ok: Some(true),
            local: Some(None),
            never_says: &[MENU_END],
            ..After::default()
        })
        .await,
    ]);
}
