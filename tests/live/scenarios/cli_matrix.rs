//! Every `rdc sync` flag combination against every state one object can be
//! in, through the real binary and the fake org — plus a table of valid and
//! invalid argument combinations for every command.
//!
//! The matrix exists because flags were only ever tested one at a time, and
//! bugs lived in the combinations: `--no-pull` still applying a remote delete
//! to the snapshot is invisible to a test of `--no-pull` on a remote *edit*.
//! Each cell starts from one freshly pulled label, puts it in a [`State`],
//! runs one `rdc sync test <flags>` with piped stdin (the non-interactive
//! path), and compares what happened against [`want`]. The interactive path
//! is `cli_prompts`.
//!
//! [`want`] is the specification.
//!
//! Fake-only on purpose: none of this is about what the real API does, and
//! 180 syncs against a live org would take the better part of an hour.

use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::converge::combined;
use crate::support::fake::FakeOrg;
use crate::support::project::ProjectFixture;

pub(crate) const RED: &str = "#ff0000";
pub(crate) const LOCAL: &str = "#111111";
pub(crate) const REMOTE: &str = "#222222";

/// The second label [`State::LocalCreate`] / [`State::RemoteCreate`] add.
const URGENT: &str = "Urgent";
const URGENT_REL: &str = "envs/test/labels/urgent.json";

/// Labels seeded `RED` and pulled once, so the lockfile holds a clean base.
/// The helpers without an explicit target act on the first one.
pub(crate) struct LabelFx {
    /// Held so the server outlives every request the fixture makes.
    pub fake: FakeOrg,
    pub client: LiveClient,
    pub project: ProjectFixture,
    pub id: u64,
    pub rel: String,
    pub shadow: String,
    pub marker: String,
}

impl LabelFx {
    pub async fn new() -> LabelFx {
        Self::with_labels(&["Priority"]).await
    }

    pub async fn with_labels(names: &[&str]) -> LabelFx {
        let fake = FakeOrg::start().await;
        let cfg = fake.config();
        let client = LiveClient::connect(&cfg).expect("connect");
        let mut ids = Vec::new();
        for name in names {
            let body = serde_json::json!({ "name": name, "organization": client.org_url, "color": RED });
            ids.push(client.create("label", &body).await.expect("seed label").0);
        }
        let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
        let pull = project.run_rdc(&["sync", "test", "--no-push"]);
        assert!(pull.status.success(), "initial pull: {}", combined(&pull));
        let lf = load_lockfile(project.path(), "test").expect("lockfile");
        let slug = lockfile_keys(&lf, "labels")
            .into_iter()
            .find(|s| *s == names[0].to_lowercase())
            .expect("label slug");
        LabelFx {
            fake,
            client,
            project,
            id: ids[0],
            rel: format!("envs/test/labels/{slug}.json"),
            shadow: format!(".rdc/conflicts/test/labels/{slug}.json"),
            marker: format!(".rdc/conflicts/test/labels/{slug}.json-deleted"),
        }
    }

    pub fn set_local_at(&self, rel: &str, color: &str) {
        let mut v = self.project.read_json(rel);
        v["color"] = color.into();
        self.project.write_json(rel, &v);
    }

    pub fn set_local(&self, color: &str) {
        self.set_local_at(&self.rel, color);
    }

    pub fn local_at(&self, rel: &str) -> Option<String> {
        let raw = self.project.read_to_string(rel)?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        Some(v["color"].as_str().unwrap_or_default().to_string())
    }

    pub fn local(&self) -> Option<String> {
        self.local_at(&self.rel)
    }

    pub fn delete_local(&self) {
        std::fs::remove_file(self.project.path().join(&self.rel)).unwrap();
    }

    pub async fn set_remote_id(&self, id: u64, color: &str) {
        self.client
            .patch_fields("label", id, serde_json::json!({ "color": color }))
            .await
            .expect("patch remote");
    }

    pub async fn set_remote(&self, color: &str) {
        self.set_remote_id(self.id, color).await;
    }

    pub async fn delete_remote(&self) {
        self.client.delete("label", self.id).await.expect("delete remote");
    }

    /// `None` once the label is gone from the org.
    pub async fn remote_id(&self, id: u64) -> Option<String> {
        self.client
            .find_listed_value("label", id)
            .await
            .expect("list labels")
            .map(|v| v["color"].as_str().unwrap_or_default().to_string())
    }

    pub async fn remote(&self) -> Option<String> {
        self.remote_id(self.id).await
    }

    pub async fn remote_names(&self) -> Vec<String> {
        self.client
            .list_ids_by_name_prefix("label", "")
            .await
            .expect("list labels")
            .into_iter()
            .map(|(_, n)| n)
            .collect()
    }

    pub fn lockfile(&self) -> String {
        self.project.read_to_string(".rdc/state/test.lock.json").unwrap_or_default()
    }

    pub async fn apply(&self, s: State) {
        match s {
            State::LocalEdit => self.set_local(LOCAL),
            State::RemoteEdit => self.set_remote(REMOTE).await,
            State::Conflict => {
                self.set_local(LOCAL);
                self.set_remote(REMOTE).await;
            }
            State::LocalDelete => self.delete_local(),
            State::RemoteDelete => self.delete_remote().await,
            State::LocalEditRemoteDelete => {
                self.set_local(LOCAL);
                self.delete_remote().await;
            }
            State::LocalDeleteRemoteEdit => {
                self.delete_local();
                self.set_remote(REMOTE).await;
            }
            State::RemoteCreate => {
                let body = serde_json::json!({ "name": URGENT, "organization": self.client.org_url, "color": REMOTE });
                self.client.create("label", &body).await.expect("seed second label");
            }
            State::LocalCreate => {
                let mut v = self.project.read_json(&self.rel);
                let o = v.as_object_mut().unwrap();
                o.remove("id");
                o.remove("url");
                o.insert("name".into(), URGENT.into());
                o.insert("color".into(), LOCAL.into());
                self.project.write_json(URGENT_REL, &v);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    LocalEdit,
    RemoteEdit,
    /// The same field edited to different values on both sides.
    Conflict,
    /// The local file removed; its lockfile entry makes that a delete.
    LocalDelete,
    RemoteDelete,
    LocalEditRemoteDelete,
    LocalDeleteRemoteEdit,
    RemoteCreate,
    LocalCreate,
}

impl State {
    /// `(remote, local)` colour of the first label right after
    /// [`LabelFx::apply`]; `None` = absent.
    fn after_apply(self) -> (Option<&'static str>, Option<&'static str>) {
        match self {
            State::LocalEdit => (Some(RED), Some(LOCAL)),
            State::RemoteEdit => (Some(REMOTE), Some(RED)),
            State::Conflict => (Some(REMOTE), Some(LOCAL)),
            State::LocalDelete => (Some(RED), None),
            State::RemoteDelete => (None, Some(RED)),
            State::LocalEditRemoteDelete => (None, Some(LOCAL)),
            State::LocalDeleteRemoteEdit => (Some(REMOTE), None),
            State::RemoteCreate | State::LocalCreate => (Some(RED), Some(RED)),
        }
    }
}

/// Every flag set the matrix runs. Parse errors are the argument table's job,
/// so only combinations clap accepts are here.
const FLAGS: &[&[&str]] = &[
    &[],
    &["--yes"],
    &["--no-push"],
    &["--no-pull"],
    &["--dry-run"],
    &["--dry-run", "--no-push"],
    &["--dry-run", "--no-pull"],
    &["--dry-run", "--allow-deletes"],
    &["--dry-run", "--conflict", "keep-local"],
    &["--dry-run", "--conflict", "use-remote"],
    &["--allow-deletes"],
    &["--allow-deletes", "--no-push"],
    &["--allow-deletes", "--no-pull"],
    &["--allow-deletes", "--conflict", "keep-local"],
    &["--conflict", "keep-local"],
    &["--conflict", "use-remote"],
    &["--conflict", "skip"],
    &["--no-push", "--conflict", "keep-local"],
    &["--no-push", "--conflict", "use-remote"],
    &["--no-pull", "--conflict", "keep-local"],
    &["--no-pull", "--conflict", "use-remote"],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strategy {
    KeepLocal,
    UseRemote,
    Skip,
}

struct Flags {
    dry: bool,
    no_push: bool,
    no_pull: bool,
    allow_deletes: bool,
    conflict: Option<Strategy>,
}

impl Flags {
    fn parse(args: &[&str]) -> Flags {
        let conflict = args.iter().position(|a| *a == "--conflict").map(|i| match args[i + 1] {
            "keep-local" => Strategy::KeepLocal,
            "use-remote" => Strategy::UseRemote,
            "skip" => Strategy::Skip,
            other => panic!("unknown strategy {other}"),
        });
        Flags {
            dry: args.contains(&"--dry-run"),
            no_push: args.contains(&"--no-push"),
            no_pull: args.contains(&"--no-pull"),
            allow_deletes: args.contains(&"--allow-deletes"),
            conflict,
        }
    }
}

/// What one run did, for the first label unless named otherwise.
#[derive(Debug, Default)]
struct Seen {
    ok: bool,
    remote: Option<String>,
    local: Option<String>,
    shadow: bool,
    marker: bool,
    /// Some label named like the first one exists on the env (a restore
    /// re-creates it under a new id).
    named_remote: bool,
    /// POST / PATCH / DELETE against the core API.
    writes: usize,
    lock_changed: bool,
    urgent_remote: bool,
    urgent_local: bool,
    /// `(push, pull)` from a dry run's `Dry run: N would push, M would pull`.
    plan: Option<(usize, usize)>,
}

/// What a run should do. `None` = not checked.
#[derive(Debug, Default)]
struct Want {
    ok: Option<bool>,
    remote: Option<Option<&'static str>>,
    local: Option<Option<&'static str>>,
    shadow: Option<bool>,
    marker: Option<bool>,
    named_remote: Option<bool>,
    writes: Option<usize>,
    lock_changed: Option<bool>,
    urgent_remote: Option<bool>,
    urgent_local: Option<bool>,
    plan: Option<Option<(usize, usize)>>,
}

/// The specification: what `rdc sync test <flags>`, run without a terminal,
/// must do to an object in `state`.
fn want(state: State, f: &Flags) -> Want {
    let (remote0, local0) = state.after_apply();
    let mut w = Want::default();

    // --dry-run writes nothing, locally or remotely, whatever else is passed.
    if f.dry {
        w.ok = Some(true);
        w.remote = Some(remote0);
        w.local = Some(local0);
        w.shadow = Some(false);
        w.marker = Some(false);
        w.writes = Some(0);
        w.lock_changed = Some(false);
        w.urgent_remote = Some(state == State::RemoteCreate);
        w.urgent_local = Some(state == State::LocalCreate);
        // The preview counts what a real run with the same flags would do.
        let push = matches!(state, State::LocalEdit | State::LocalCreate | State::LocalDelete);
        let pull = matches!(state, State::RemoteEdit | State::RemoteCreate | State::RemoteDelete);
        w.plan = Some(Some((
            usize::from(push && !f.no_push),
            usize::from(pull && !f.no_pull),
        )));
        return w;
    }
    // --no-push never writes to the env.
    if f.no_push {
        w.writes = Some(0);
    }
    let pushes = !f.no_push;
    let pulls = !f.no_pull;
    w.ok = Some(true);

    match state {
        State::LocalEdit => {
            w.local = Some(Some(LOCAL));
            w.remote = Some(Some(if pushes { LOCAL } else { RED }));
        }
        State::RemoteEdit => {
            w.remote = Some(Some(REMOTE));
            w.local = Some(Some(if pulls { REMOTE } else { RED }));
            w.writes = Some(0);
        }
        State::Conflict => match f.conflict {
            // No terminal and no strategy: park the env's copy as a shadow.
            None | Some(Strategy::Skip) => {
                w.local = Some(Some(LOCAL));
                w.remote = Some(Some(REMOTE));
                w.shadow = Some(true);
                w.writes = Some(0);
            }
            // Under --no-push the resolution is recorded and the push waits
            // for the next sync (`no_push_keep_local_pushes_on_the_next_sync`).
            Some(Strategy::KeepLocal) => {
                w.local = Some(Some(LOCAL));
                w.remote = Some(Some(if pushes { LOCAL } else { REMOTE }));
                w.shadow = Some(false);
            }
            // An explicit strategy wins over --no-pull.
            Some(Strategy::UseRemote) => {
                w.remote = Some(Some(REMOTE));
                w.local = Some(Some(REMOTE));
                w.shadow = Some(false);
                w.writes = Some(0);
            }
        },
        State::LocalDelete => {
            w.local = Some(None);
            if !pushes {
                w.remote = Some(Some(RED));
            } else if f.allow_deletes {
                w.remote = Some(None);
            } else {
                // No terminal to ask, so refuse rather than delete.
                w.ok = Some(false);
                w.remote = Some(Some(RED));
                w.writes = Some(0);
            }
        }
        State::RemoteDelete => {
            w.remote = Some(None);
            w.writes = Some(0);
            // A clean remote delete is mirrored locally — unless --no-pull,
            // which must not apply remote changes to the snapshot.
            w.local = Some(if pulls { None } else { Some(RED) });
        }
        // `--conflict` resolves edit-vs-delete conflicts as its keys do.
        State::LocalEditRemoteDelete => {
            w.remote = Some(None);
            match f.conflict {
                None | Some(Strategy::Skip) => {
                    w.local = Some(Some(LOCAL));
                    w.marker = Some(true);
                    w.named_remote = Some(false);
                    w.writes = Some(0);
                }
                // Restore on the env: re-created, under a new id.
                Some(Strategy::KeepLocal) => {
                    w.local = Some(Some(LOCAL));
                    w.marker = Some(false);
                    w.named_remote = Some(pushes);
                }
                Some(Strategy::UseRemote) => {
                    w.local = Some(None);
                    w.marker = Some(false);
                    w.named_remote = Some(false);
                    w.writes = Some(0);
                }
            }
        }
        State::LocalDeleteRemoteEdit => match f.conflict {
            // Parked. The env's copy is restored for review — but not under
            // --no-pull, which leaves the snapshot alone.
            None | Some(Strategy::Skip) => {
                w.remote = Some(Some(REMOTE));
                w.local = Some(if pulls { Some(REMOTE) } else { None });
                w.marker = Some(true);
                w.writes = Some(0);
            }
            Some(Strategy::UseRemote) => {
                w.remote = Some(Some(REMOTE));
                w.local = Some(Some(REMOTE));
                w.marker = Some(false);
                w.writes = Some(0);
            }
            // Keep the local deletion: DELETE on the env, behind the same
            // gate as any delete, so no terminal means --allow-deletes.
            Some(Strategy::KeepLocal) => {
                w.local = Some(None);
                w.marker = Some(false);
                if !pushes {
                    w.remote = Some(Some(REMOTE));
                } else if f.allow_deletes {
                    w.remote = Some(None);
                } else {
                    w.ok = Some(false);
                    w.remote = Some(Some(REMOTE));
                    w.writes = Some(0);
                }
            }
        },
        State::RemoteCreate => {
            w.urgent_remote = Some(true);
            w.urgent_local = Some(pulls);
            w.writes = Some(0);
        }
        State::LocalCreate => {
            w.urgent_local = Some(true);
            w.urgent_remote = Some(pushes);
        }
    }
    w
}

/// Mismatches between `w` and `s`, one line each.
fn diff(w: &Want, s: &Seen) -> Vec<String> {
    let mut out = Vec::new();
    macro_rules! check {
        ($field:ident, $seen:expr) => {
            if let Some(expected) = &w.$field {
                if *expected != $seen {
                    out.push(format!("{}: want {:?}, got {:?}", stringify!($field), expected, $seen));
                }
            }
        };
    }
    check!(ok, s.ok);
    check!(remote, s.remote.as_deref());
    check!(local, s.local.as_deref());
    check!(shadow, s.shadow);
    check!(marker, s.marker);
    check!(named_remote, s.named_remote);
    check!(writes, s.writes);
    check!(lock_changed, s.lock_changed);
    check!(urgent_remote, s.urgent_remote);
    check!(urgent_local, s.urgent_local);
    check!(plan, s.plan);
    out
}

async fn run_cell(state: State, flags: &[&str]) -> (Seen, String) {
    let fx = LabelFx::new().await;
    fx.apply(state).await;
    let lock_before = fx.lockfile();
    let mut args = vec!["sync", "test"];
    args.extend_from_slice(flags);
    let (out, trace) = fx.project.run_rdc_traced(&args);
    let writes = trace
        .lines
        .iter()
        // The MDH listing is a POST that only reads.
        .filter(|l| l.method != "GET" && !l.url.ends_with("/collections/list"))
        .count();
    let seen = Seen {
        ok: out.status.success(),
        remote: fx.remote().await,
        local: fx.local(),
        shadow: fx.project.exists(&fx.shadow),
        marker: fx.project.exists(&fx.marker),
        named_remote: fx.remote_names().await.iter().any(|n| n == "Priority"),
        writes,
        lock_changed: fx.lockfile() != lock_before,
        urgent_remote: fx.remote_names().await.iter().any(|n| n == URGENT),
        urgent_local: fx.project.exists(URGENT_REL),
        plan: dry_run_counts(&combined(&out)),
    };
    (seen, combined(&out))
}

/// `(push, pull)` from the `Dry run: N would push, M would pull, …` summary.
fn dry_run_counts(output: &str) -> Option<(usize, usize)> {
    let line = output.lines().find(|l| l.contains("Dry run: "))?;
    let rest = line.split("Dry run: ").nth(1)?;
    let mut nums = rest.split(", ").map(|part| part.split(' ').next().and_then(|n| n.parse().ok()));
    Some((nums.next()??, nums.next()??))
}

/// Run every flag set against `state` and report every wrong cell at once.
async fn run_state(state: State) {
    let mut failures = Vec::new();
    for flags in FLAGS {
        let (seen, output) = run_cell(state, flags).await;
        let problems = diff(&want(state, &Flags::parse(flags)), &seen);
        if !problems.is_empty() {
            let output: Vec<&str> = output.lines().filter(|l| !l.contains(" list ")).collect();
            failures.push(format!(
                "rdc sync test {}\n    {}\n    --- output ---\n{}",
                flags.join(" "),
                problems.join("\n    "),
                output.join("\n")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{state:?}: {} cell(s) wrong:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

macro_rules! matrix_test {
    ($name:ident, $state:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            run_state($state).await;
        }
    };
}

matrix_test!(matrix_local_edit, State::LocalEdit);
matrix_test!(matrix_remote_edit, State::RemoteEdit);
matrix_test!(matrix_conflict, State::Conflict);
matrix_test!(matrix_local_delete, State::LocalDelete);
matrix_test!(matrix_remote_delete, State::RemoteDelete);
matrix_test!(matrix_local_edit_remote_delete, State::LocalEditRemoteDelete);
matrix_test!(matrix_local_delete_remote_edit, State::LocalDeleteRemoteEdit);
matrix_test!(matrix_remote_create, State::RemoteCreate);
matrix_test!(matrix_local_create, State::LocalCreate);

/// `--no-push --conflict keep-local` records the decision without pushing;
/// the next plain sync pushes it rather than raising the conflict again or,
/// worse, pulling the env's copy over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_push_keep_local_pushes_on_the_next_sync() {
    let fx = LabelFx::new().await;
    fx.apply(State::Conflict).await;
    let first = fx.project.run_rdc(&["sync", "test", "--no-push", "--conflict", "keep-local"]);
    assert!(first.status.success(), "{}", combined(&first));
    assert_eq!(fx.remote().await.as_deref(), Some(REMOTE), "--no-push must not push");

    let second = fx.project.run_rdc(&["sync", "test"]);
    assert!(second.status.success(), "{}", combined(&second));
    assert_eq!(fx.local().as_deref(), Some(LOCAL), "the kept local value survives");
    assert_eq!(fx.remote().await.as_deref(), Some(LOCAL), "and reaches the env on the next sync");
    assert!(!fx.project.exists(&fx.shadow));
}

/// The same deferral for both edit-vs-delete conflicts: `keep-local` under
/// --no-push is carried out by the next sync that pushes, without asking
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_push_keep_local_on_edit_vs_delete_waits_for_the_next_sync() {
    // Local edit, env deleted: the next sync re-creates it.
    let fx = LabelFx::new().await;
    fx.apply(State::LocalEditRemoteDelete).await;
    let first = fx.project.run_rdc(&["sync", "test", "--no-push", "--conflict", "keep-local"]);
    assert!(first.status.success(), "{}", combined(&first));
    assert!(!fx.remote_names().await.contains(&"Priority".to_string()));
    let second = fx.project.run_rdc(&["sync", "test"]);
    assert!(second.status.success(), "{}", combined(&second));
    assert!(fx.remote_names().await.contains(&"Priority".to_string()), "{}", combined(&second));
    assert_eq!(fx.local().as_deref(), Some(LOCAL));

    // Local delete, env edited: the next sync deletes it, as a plain delete.
    let fx = LabelFx::new().await;
    fx.apply(State::LocalDeleteRemoteEdit).await;
    let first = fx.project.run_rdc(&["sync", "test", "--no-push", "--conflict", "keep-local"]);
    assert!(first.status.success(), "{}", combined(&first));
    assert_eq!(fx.remote().await.as_deref(), Some(REMOTE));
    let second = fx.project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(second.status.success(), "{}", combined(&second));
    assert_eq!(fx.remote().await, None, "{}", combined(&second));
    assert_eq!(fx.local(), None);
    assert!(!fx.project.exists(&fx.marker));
}

/// A conflict parked non-interactively stays parked across syncs, and a later
/// `--conflict` resolves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_conflict_waits_for_a_strategy() {
    let fx = LabelFx::new().await;
    fx.apply(State::Conflict).await;
    for _ in 0..2 {
        let out = fx.project.run_rdc(&["sync", "test"]);
        assert!(out.status.success(), "{}", combined(&out));
        assert!(fx.project.exists(&fx.shadow), "the conflict stays parked");
        assert_eq!(fx.local().as_deref(), Some(LOCAL));
        assert_eq!(fx.remote().await.as_deref(), Some(REMOTE));
    }
    let out = fx.project.run_rdc(&["sync", "test", "--conflict", "use-remote"]);
    assert!(out.status.success(), "{}", combined(&out));
    assert_eq!(fx.local().as_deref(), Some(REMOTE));
    assert!(!fx.project.exists(&fx.shadow), "resolving sweeps the shadow");
}

// ---------------------------------------------------------------------------
// Argument combinations
// ---------------------------------------------------------------------------

/// How a command line must end.
#[derive(Debug, Clone, Copy)]
enum Ends {
    /// clap rejects it: exit 2, and stderr contains the needle.
    Usage(&'static str),
    /// rdc rejects it after parsing: exit 1, and stderr contains the needle.
    Fails(&'static str),
    /// It runs and succeeds.
    Succeeds,
}

/// Where a case runs: the shared two-env project, or a fresh empty directory
/// (for `init`, which would otherwise change what the other cases share).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum In {
    Project,
    Empty,
}

use Ends::{Fails, Succeeds, Usage};
use In::{Empty, Project};

const NO_TTY_ENV: &str = "env argument required without a terminal";
const UNDEFINED: &str = "is not defined in rdc.toml";
const CONFLICTS: &str = "cannot be used with";
const NEEDS_WATCH: &str = "--watch";
const ONCE: &str = "cannot be used multiple times";
const BAD_ENV_NAME: &str = "may only contain letters, digits, - and _";

/// None of these may start a watch for real: a `--watch` that parses runs
/// until it is killed.
const ARGS: &[(In, &[&str], Ends)] = &[
    // ---- sync: the env argument
    (Project, &["sync"], Fails(NO_TTY_ENV)),
    (Project, &["sync", "nope"], Fails(UNDEFINED)),
    (Project, &["sync", "TEST", "--dry-run"], Fails(UNDEFINED)),
    (Project, &["sync", "", "--dry-run"], Fails(UNDEFINED)),
    (Project, &["sync", "test", "extra"], Usage("unexpected argument 'extra'")),
    (Project, &["sync", "--no-push", "test"], Succeeds),
    // ---- sync: conflicts and dependencies between flags
    (Project, &["sync", "test", "--no-push", "--no-pull"], Usage(CONFLICTS)),
    (Project, &["sync", "test", "--watch", "--dry-run"], Usage(CONFLICTS)),
    (Project, &["sync", "test", "--watch", "--conflict", "skip"], Usage(CONFLICTS)),
    (Project, &["sync", "test", "--watch", "--no-poll", "--poll-interval", "5s"], Usage(CONFLICTS)),
    (Project, &["sync", "test", "--poll-interval", "5s"], Usage(NEEDS_WATCH)),
    (Project, &["sync", "test", "--no-poll"], Usage(NEEDS_WATCH)),
    (Project, &["sync", "test", "-v"], Usage(NEEDS_WATCH)),
    (Project, &["sync", "test", "--verbose"], Usage(NEEDS_WATCH)),
    (Project, &["sync", "test", "--no-bell"], Usage(NEEDS_WATCH)),
    (Project, &["sync", "test", "--dry-run", "--dry-run"], Usage(ONCE)),
    (Project, &["sync", "test", "--conflict", "skip", "--conflict", "use-remote"], Usage(ONCE)),
    // ---- sync: values
    (Project, &["sync", "test", "--conflict"], Usage("a value is required")),
    (Project, &["sync", "test", "--conflict", "bogus"], Usage("invalid value 'bogus'")),
    (Project, &["sync", "test", "--watch", "--poll-interval", "abc"], Fails("invalid duration")),
    (Project, &["sync", "test", "--watch", "--poll-interval", "5x"], Fails("invalid duration unit")),
    (Project, &["sync", "test", "--watch", "--poll-interval", "-5"], Usage("unexpected argument '-5'")),
    (Project, &["sync", "test", "--watch", "--poll-interval", "0"], Fails("at least 1s")),
    (Project, &["sync", "test", "--watch", "--poll-interval", "0s"], Fails("at least 1s")),
    (Project, &["sync", "test", "--watch", "--poll-interval", "0m"], Fails("at least 1s")),
    // ---- sync: flags that belong to another command
    (Project, &["sync", "test", "--only", "labels/x"], Usage("unexpected argument '--only'")),
    (Project, &["sync", "test", "--mirror"], Usage("unexpected argument '--mirror'")),
    // ---- --yes is global, before or after the subcommand
    (Project, &["--yes", "sync", "test", "--dry-run"], Succeeds),
    (Project, &["sync", "test", "--dry-run", "--yes"], Succeeds),
    // ---- migrate
    (Project, &["migrate"], Fails(NO_TTY_ENV)),
    (Project, &["migrate", "test"], Fails(NO_TTY_ENV)),
    (Project, &["migrate", "test", "test"], Fails("are the same")),
    (Project, &["migrate", "test", "nope"], Fails(UNDEFINED)),
    (Project, &["migrate", "nope", "prod"], Fails(UNDEFINED)),
    (Project, &["migrate", "nope", "prod", "--dry-run"], Fails(UNDEFINED)),
    (Project, &["migrate", "test", "prod", "--allow-recreate"], Usage("--mirror")),
    (Project, &["migrate", "test", "prod", "--mirror", "--allow-recreate", "--dry-run"], Succeeds),
    (Project, &["migrate", "test", "prod", "--only", "labels/*", "--dry-run"], Succeeds),
    (Project, &["migrate", "test", "prod", "--only", "*/*", "--dry-run"], Succeeds),
    (Project, &["migrate", "test", "prod", "--only", "labels/nope", "--dry-run"], Fails("matched 0 objects")),
    (Project, &["migrate", "test", "prod", "--only", "", "--dry-run"], Fails("invalid --only ''")),
    (Project, &["migrate", "test", "prod", "--only", "labels", "--dry-run"], Fails("invalid --only 'labels'")),
    (Project, &["migrate", "test", "prod", "--only", "bogus/*", "--dry-run"], Fails("unknown kind 'bogus'")),
    (Project, &["migrate", "test", "prod", "--carry", "all,automation", "--dry-run"], Succeeds),
    (Project, &["migrate", "test", "prod", "--carry", "bogus", "--dry-run"], Usage("invalid value 'bogus'")),
    (Project, &["migrate", "test", "prod", "--carry", "", "--dry-run"], Usage("a value is required")),
    (Project, &["migrate", "test", "prod", "--carry", "automation,", "--dry-run"], Usage("a value is required")),
    (Project, &["migrate", "test", "prod", "--no-push"], Usage("unexpected argument '--no-push'")),
    // ---- auth
    (Project, &["auth"], Fails(NO_TTY_ENV)),
    (Project, &["auth", "nope", "--token", "x"], Fails(UNDEFINED)),
    (Project, &["auth", "test", "--token", "x", "--username", "y"], Usage(CONFLICTS)),
    (Project, &["auth", "test", "--token"], Usage("a value is required")),
    // ---- doctor
    (Project, &["doctor"], Fails(NO_TTY_ENV)),
    (Project, &["doctor", "nope"], Fails(UNDEFINED)),
    (Project, &["doctor", "test", "--dry-run"], Succeeds),
    (Project, &["doctor", "test", "--allow-deletes"], Usage("unexpected argument '--allow-deletes'")),
    // ---- edit env rename
    (Project, &["edit", "env", "rename", "test"], Usage("required arguments were not provided")),
    (Project, &["edit", "env", "rename", "test", "prod", "--dry-run"], Fails("")),
    (Project, &["edit", "env", "rename", "nope", "other", "--dry-run"], Fails("")),
    (Project, &["edit", "env", "rename", "test", "bad/name", "--dry-run"], Fails(BAD_ENV_NAME)),
    (Project, &["edit", "env", "rename", "test", "staging", "--dry-run"], Succeeds),
    // ---- init: the env spec
    (Empty, &["init", "--env", "bad"], Fails("invalid --env spec")),
    (Empty, &["init", "--env", "x=http://h:notanum"], Fails("parsing org_id")),
    (Empty, &["init", "--env", "x=http://h"], Fails("org_id")),
    (Empty, &["init", "--env", "=http://h:1"], Fails(BAD_ENV_NAME)),
    (Empty, &["init", "--env", "a/b=http://h:1"], Fails(BAD_ENV_NAME)),
    (Empty, &["init", "--env", "../x=http://h:1"], Fails(BAD_ENV_NAME)),
    (Empty, &["init", "--env", "dev test=http://h:1"], Fails(BAD_ENV_NAME)),
    (Project, &["init", "--env", "test=http://h/api/v1:1"], Fails("already exists")),
    (Empty, &["init", "--force"], Fails("nothing to regenerate")),
    // ---- top level
    (Project, &["bogus"], Usage("unrecognized subcommand 'bogus'")),
    (Project, &[], Succeeds),
    (Project, &["upgrade", "--version", "not-a-version", "--check"], Fails("parsing")),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn argument_combinations() {
    let fx = LabelFx::new().await;
    // A second env on the same fake org, so migrate has a target.
    let spec = format!("prod={}:{}", fx.fake.api_base(), fx.fake.config().org_id);
    let out = fx.project.run_rdc(&["init", "--env", &spec]);
    assert!(out.status.success(), "{}", combined(&out));
    std::fs::copy(
        fx.project.path().join("secrets/test.secrets.json"),
        fx.project.path().join("secrets/prod.secrets.json"),
    )
    .unwrap();

    let mut failures = Vec::new();
    for (place, args, ends) in ARGS {
        let empty = tempfile::TempDir::new().unwrap();
        let dir = if *place == Empty { empty.path() } else { fx.project.path() };
        let out = assert_cmd::Command::cargo_bin("rdc")
            .unwrap()
            .current_dir(dir)
            .args(*args)
            .output()
            .expect("spawning rdc");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let code = out.status.code();
        let right = match ends {
            Usage(needle) => code == Some(2) && stderr.contains(needle),
            Fails(needle) => code == Some(1) && stderr.contains(needle),
            Succeeds => code == Some(0),
        };
        // A rejected `init` must leave the directory as it found it.
        let litter = *place == Empty
            && !matches!(ends, Succeeds)
            && std::fs::read_dir(dir).unwrap().next().is_some();
        if !right || litter {
            failures.push(format!(
                "rdc {:?}: want {ends:?}, got exit {code:?}{}\n{}",
                args,
                if litter { ", and it left files behind" } else { "" },
                stderr.lines().take(4).collect::<Vec<_>>().join("\n")
            ));
        }
    }
    assert!(failures.is_empty(), "{} case(s) wrong:\n\n{}", failures.len(), failures.join("\n\n"));
}
