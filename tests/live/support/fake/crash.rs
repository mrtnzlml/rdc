//! Interrupting a sync partway, the way a killed process or a closed tab does.
//!
//! The run is `rdc`'s own sync, in this process (`embed::sync_logged`), so
//! the test can drop its future at a chosen moment: nothing after that point
//! runs, exactly as for a process that died there. Two kinds of moment:
//!
//! - on the server side, [`FakeOrg::crash_on`] stops one request before or
//!   after the fake applies it and never answers it;
//! - on the client side, [`CrashStore`] is the run's state store and never
//!   returns from the checkpoint whose lockfile matches its predicate.
//!
//! What each leaves on disk (the lockfile is saved before the store is
//! checkpointed, `state::store::journal_for`):
//!
//! | point                                   | how                                   | lockfile on disk                     | server        |
//! |-----------------------------------------|---------------------------------------|--------------------------------------|---------------|
//! | before begin-persist                    | `CrashStore`, `undo`                  | as of the last checkpoint: no op     | nothing sent  |
//! | after begin-persist, before apply       | `crash_on`, `BeforeApply`             | the op, no id                        | not applied   |
//! | after apply, before response            | `crash_on`, `AfterApplyBeforeResponse`| the op, no id                        | applied       |
//! | after finish-persist, before the driver | `CrashStore`, no `undo`               | the op with its id; no entry or file | applied       |

use super::*;
pub use crate::support::crash::*;
use rdc::state::store::StateStore;

/// A `POST` to exactly `path` whose body is named `name`.
pub fn post_named(path: &str, name: &str) -> RequestMatch {
    let (path, name) = (path.to_string(), name.to_string());
    Box::new(move |req| {
        req.method.as_str() == "POST" && req.url.path() == path && body_of(req)["name"] == name.as_str()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::assert_local::load_lockfile;
    use crate::support::project::ProjectFixture;
    use serde_json::json;

    /// A project whose only local object is one new label, `priority`.
    fn one_label_project(fake: &FakeOrg) -> ProjectFixture {
        let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
        let org_url = fake.state().org_url();
        project.write_json(
            "envs/test/labels/priority.json",
            &json!({ "id": 0, "url": "", "name": "priority", "organization": org_url, "color": "#ff8800" }),
        );
        project
    }

    fn fs() -> Arc<dyn StateStore> {
        Arc::new(rdc::state::store::FsStateStore)
    }

    /// A request that reaches the fake while it is dead gets no answer.
    async fn answered(fake: &FakeOrg) -> bool {
        let url = format!("{}/labels", fake.api_base());
        let req = reqwest::Client::new().get(url).header("authorization", "token fake-token").send();
        tokio::time::timeout(std::time::Duration::from_millis(300), req).await.is_ok()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn before_apply_applies_nothing_and_answers_nothing_after() {
        let fake = FakeOrg::start().await;
        let project = one_label_project(&fake);
        let fired = fake.crash_on(post_named("/api/v1/labels", "priority"), 1, CrashPoint::BeforeApply);

        until_crash(sync_in_process(project.path(), "fake-token", yes(false), fs()), &fired).await;

        assert!(fake.state().ids("labels").is_empty(), "the create was not applied");
        let ops = load_lockfile(project.path(), "test").unwrap().pending.expect("the op is on disk").ops;
        assert_eq!((ops[0].kind.as_str(), ops[0].id), ("labels", None));
        assert!(!answered(&fake).await, "a dead client's fake answers nothing");
        fake.revive();
        assert!(answered(&fake).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn after_apply_keeps_the_object_but_not_its_answer() {
        let fake = FakeOrg::start().await;
        let project = one_label_project(&fake);
        let fired = fake.crash_on(post_named("/api/v1/labels", "priority"), 1, CrashPoint::AfterApplyBeforeResponse);

        until_crash(sync_in_process(project.path(), "fake-token", yes(false), fs()), &fired).await;

        assert_eq!(fake.state().ids("labels").len(), 1, "the create was applied");
        let lf = load_lockfile(project.path(), "test").unwrap();
        assert_eq!(lf.pending.expect("the op is on disk").ops[0].id, None, "its answer never arrived");
        assert!(!lf.objects.get("labels").is_some_and(|m| m.contains_key("priority")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_undone_begin_never_reached_the_disk() {
        let fake = FakeOrg::start().await;
        let project = one_label_project(&fake);
        let store = CrashStore::new(begun("labels", "priority"), true);

        until_crash(sync_in_process(project.path(), "fake-token", yes(false), store.clone()), &store.fired).await;

        assert!(fake.state().ids("labels").is_empty(), "nothing was sent");
        let lf = load_lockfile(project.path(), "test").unwrap_or_default();
        assert_eq!(lf.pending, None, "the begin is not on disk");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finished_create_is_on_disk_before_the_driver_records_it() {
        let fake = FakeOrg::start().await;
        let project = one_label_project(&fake);
        let store = CrashStore::new(finished("labels", "priority"), false);

        until_crash(sync_in_process(project.path(), "fake-token", yes(false), store.clone()), &store.fired).await;

        let ids = fake.state().ids("labels");
        let lf = load_lockfile(project.path(), "test").unwrap();
        assert_eq!(lf.pending.expect("the op is on disk").ops[0].id, Some(ids[0]));
        assert!(!lf.objects.get("labels").is_some_and(|m| m.contains_key("priority")), "the driver has not recorded it");
    }

    /// `crash_after(n)` counts every API write: the first one here is the
    /// label's create.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crash_after_counts_api_writes() {
        let fake = FakeOrg::start().await;
        let project = one_label_project(&fake);
        let fired = fake.crash_after(1, CrashPoint::BeforeApply);

        until_crash(sync_in_process(project.path(), "fake-token", yes(false), fs()), &fired).await;

        assert!(fake.state().ids("labels").is_empty());
    }
}
