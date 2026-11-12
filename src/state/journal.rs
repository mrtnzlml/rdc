//! The in-flight create journal stored inside the lockfile.
//!
//! A sync that creates remote objects records its intent here before it
//! issues each create, so a crashed run can be recognised and reconciled on
//! the next one. The lockfile carries it as [`crate::state::Lockfile::pending`].

use crate::state::Lockfile;
use anyhow::Result;
use futures::lock::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::Arc;

/// Writes a lockfile durably: the committed objects plus the journal's current
/// `pending`. Supplied by the caller (the sync wires `lockfile.save` and a
/// state-store checkpoint), so the journal itself does no I/O. Its future is
/// `Send` natively and local on wasm32, as [`crate::rt::BoxFuture`] is.
pub type PersistFn = Arc<dyn Fn(&Lockfile) -> crate::rt::BoxFuture<'static, Result<()>> + Send + Sync>;

/// A run's journal of creates that may or may not have reached the server.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Pending {
    /// Identifier of the run that wrote this journal.
    pub run: String,
    /// The creates the run intended, in issue order.
    pub ops: Vec<CreateIntent>,
}

/// One create the run intended to issue.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct CreateIntent {
    /// Position of this op within the run.
    pub seq: u64,
    /// Lockfile kind of the object being created (e.g. `hooks`).
    pub kind: String,
    /// Slug the new object will be recorded under.
    pub slug: String,
    /// Identifying fields used to find the object again if the create's
    /// response was lost.
    pub fields: BTreeMap<String, String>,
    /// Unix time the create was started.
    pub started_at_unix: u64,
    /// The server-assigned id, once known.
    pub id: Option<u64>,
}

/// The fields that identify a created object when its create's response was
/// lost: `name` plus the kind's parent reference, as the create body carries
/// it after `strip_for_create`. A candidate must match every one of these the
/// body has; [`match_fields`] hashes them. Every create body has a `name`.
///
/// Sources, per row:
/// - `labels`, `workspaces`: the body's only other reference is
///   `organization`, the same for every object in the org.
/// - `schemas`: `queues` is stripped from the body (the server fills it from
///   each queue's `schema`), so nothing is left to point at a parent.
/// - `engines`: the body has no reference at all.
/// - `queues`: `workspace` and `schema`, both mandatory links of the body.
/// - `inboxes`, `hooks`, `rules`: `queues`. The store-extension install
///   body (`{name, hook_template, events, queues, token_owner}`) has it too.
/// - `email_templates`: `queue`.
/// - `saved_views`: `queues_filter`, its only queue reference.
/// - `engine_fields`: `engine`.
pub const IDENTITY_FIELDS: &[(&str, &[&str])] = &[
    ("labels", &["name"]),
    ("workspaces", &["name"]),
    ("schemas", &["name"]),
    ("engines", &["name"]),
    ("queues", &["name", "workspace", "schema"]),
    ("inboxes", &["name", "queues"]),
    ("hooks", &["name", "queues"]),
    ("rules", &["name", "queues"]),
    ("email_templates", &["name", "queue"]),
    ("saved_views", &["name", "queues_filter"]),
    ("engine_fields", &["name", "engine"]),
];

/// The [`IDENTITY_FIELDS`] row for `kind`, if it is a kind rdc creates.
pub fn identity_fields(kind: &str) -> Option<&'static [&'static str]> {
    IDENTITY_FIELDS.iter().find(|(k, _)| *k == kind).map(|(_, f)| *f)
}

tokio::task_local! {
    /// The `(kind, slug)` the current create is for. Set by the driver around
    /// each create with [`item`]; read by [`Journal::begin`].
    static ITEM: (String, String);
}

/// Run `f` with `kind`/`slug` as the object any create inside it is for.
/// Task-local for the same reason as `vfs::scope` and
/// `cli::stdin_coord::with_route`: the continuation may resume on another
/// worker thread, and the item must travel with the task, not the thread.
pub fn item<F: Future>(kind: &str, slug: &str, f: F) -> impl Future<Output = F::Output> {
    ITEM.scope((kind.to_owned(), slug.to_owned()), f)
}

/// The identifying fields of a create body: one SHA-256 hex per top-level key,
/// over the canonical JSON of that key's value. Recovery compares these with
/// the server's objects to find a create whose response was lost.
///
/// The WHOLE body is canonicalized once, with the canonicalizer
/// [`crate::state::content_hash`] uses, and only then split by key. Several of
/// its rules depend on the key a value sits under (the top-level noise-field
/// strip, the `subject`/`message` trailing-whitespace trim), so canonicalizing
/// each value alone would skip them, and a body would stop matching the
/// server's normalized copy of the very object it created.
///
/// `secrets` is skipped: the journal is written to disk, and even a hash of a
/// secret is not something to persist. A non-object body has no fields.
pub fn match_fields(body: &Value) -> BTreeMap<String, String> {
    let bytes = serde_json::to_vec(body).expect("a serde_json::Value always serializes");
    let canonical = crate::snapshot::noise::canonicalize_for_hash(&bytes, &Lockfile::default());
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&canonical) else {
        return BTreeMap::new();
    };
    // The values are canonical already (keys sorted, noise gone), so each is
    // hashed verbatim.
    map.iter()
        .filter(|(k, _)| k.as_str() != "secrets")
        .map(|(k, v)| {
            let bytes = serde_json::to_vec(v).expect("a serde_json::Value always serializes");
            (k.clone(), crate::state::raw_content_hash(&bytes))
        })
        .collect()
}

/// Unix seconds from the system clock: the production `now` for
/// [`Journal::new`]. `web_time` so the same code reads `Date.now()` on wasm.
pub fn system_now() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The run's create journal: a write-ahead log over the last committed
/// lockfile. Every write persists "committed objects + current `pending`";
/// the driver's own `&mut Lockfile` is never touched.
///
/// Cloning shares the state. The mutex is held across the persist call, so
/// concurrent writers persist one at a time and each persisted state extends
/// the one before it.
#[derive(Clone)]
pub struct Journal {
    state: Arc<Mutex<State>>,
    persist: PersistFn,
    now: fn() -> u64,
    run: Arc<str>,
}

struct State {
    /// The last lockfile handed to `new` or `commit`, with `pending: None`.
    committed: Lockfile,
    ops: Vec<CreateIntent>,
    next_seq: u64,
    /// Ids per kind of the driver's in-memory lockfile, as of the last
    /// [`Journal::observe`]. Never persisted.
    observed: BTreeMap<String, HashSet<u64>>,
    /// Ids per kind that existed before this run's first create, from
    /// [`Journal::note_existing`]. Never persisted.
    existing: BTreeMap<String, HashSet<u64>>,
}

impl Journal {
    /// A journal over `committed` with no ops. Persists nothing until the
    /// first [`Journal::begin`], so a run that creates nothing leaves no
    /// journal. `now` is injectable for tests; production passes
    /// [`system_now`].
    pub fn new(mut committed: Lockfile, persist: PersistFn, now: fn() -> u64) -> Self {
        committed.pending = None;
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("reading OS randomness for the run id");
        let run: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self {
            state: Arc::new(Mutex::new(State { committed, ops: Vec::new(), next_seq: 1, observed: BTreeMap::new(), existing: BTreeMap::new() })),
            persist,
            now,
            run: run.into(),
        }
    }

    /// This run's id: 32 lowercase hex chars, written as `pending.run`.
    pub fn run_id(&self) -> String {
        self.run.to_string()
    }

    /// Record the intent to create `body` for the [`item`] in scope, and
    /// persist it before the caller issues the create. Returns the op's
    /// `seq`, or `None` (persisting nothing) when no item is in scope.
    ///
    /// If the persist fails the op is dropped again: the caller will not
    /// issue the create, so nothing on the server can match it.
    pub async fn begin(&self, body: &Value) -> Result<Option<u64>> {
        let Ok((kind, slug)) = ITEM.try_with(Clone::clone) else {
            return Ok(None);
        };
        let mut st = self.state.lock().await;
        let seq = st.next_seq;
        st.next_seq += 1;
        st.ops.push(CreateIntent {
            seq,
            kind,
            slug,
            fields: match_fields(body),
            started_at_unix: (self.now)(),
            id: None,
        });
        if let Err(e) = self.persist_locked(&st).await {
            st.ops.retain(|op| op.seq != seq);
            return Err(e);
        }
        Ok(Some(seq))
    }

    /// Record the server-assigned `id` of op `seq` and persist it. An unknown
    /// `seq` changes nothing but still persists.
    pub async fn finish(&self, seq: u64, id: u64) -> Result<()> {
        let mut st = self.state.lock().await;
        if let Some(op) = st.ops.iter_mut().find(|op| op.seq == seq) {
            op.id = Some(id);
        }
        self.persist_locked(&st).await
    }

    /// Note the ids `lockfile` (the driver's in-memory one) records, so
    /// [`Journal::claimed`] covers objects this run pulled or created before
    /// its next commit. Persists nothing. The drivers call it right before
    /// each create, through `RossumClient::observe_lockfile`.
    pub async fn observe(&self, lockfile: &Lockfile) {
        let ids = lockfile
            .objects
            .iter()
            .map(|(kind, m)| (kind.clone(), m.values().map(|e| e.id).collect()))
            .collect();
        self.state.lock().await.observed = ids;
    }

    /// Note ids the env held before this run created anything (the sync's
    /// pre-push listing). A lost create cannot be one of them, tracked or
    /// not. Persists nothing.
    pub async fn note_existing(&self, ids: BTreeMap<String, HashSet<u64>>) {
        self.state.lock().await.existing = ids;
    }

    /// Ids under `kind` that already belong to a slug, so a lost create must
    /// not be matched to them: every id the committed lockfile records, the
    /// ids of the last [`Journal::observe`] and [`Journal::note_existing`],
    /// and every op of this run that already has its id.
    pub async fn claimed(&self, kind: &str) -> HashSet<u64> {
        let st = self.state.lock().await;
        let committed = st.committed.objects.get(kind).into_iter().flat_map(|m| m.values().map(|e| e.id));
        let observed = st.observed.get(kind).into_iter().flatten().copied();
        let existing = st.existing.get(kind).into_iter().flatten().copied();
        let finished = st.ops.iter().filter(|op| op.kind == kind).filter_map(|op| op.id);
        committed.chain(observed).chain(existing).chain(finished).collect()
    }

    /// Drop op `seq` (its create definitely did not happen) and persist.
    pub async fn abandon(&self, seq: u64) -> Result<()> {
        let mut st = self.state.lock().await;
        st.ops.retain(|op| op.seq != seq);
        self.persist_locked(&st).await
    }

    /// Make `lockfile` the new committed state and persist it. Every op whose
    /// `(kind, slug)` entry in `lockfile` now carries the op's id is settled
    /// and dropped; the rest stay pending. Always persists, so this is the
    /// run's checkpoint save.
    pub async fn commit(&self, lockfile: &Lockfile) -> Result<()> {
        let mut st = self.state.lock().await;
        st.committed = lockfile.clone();
        st.committed.pending = None;
        st.ops.retain(|op| {
            let recorded = lockfile.objects.get(&op.kind).and_then(|m| m.get(&op.slug)).map(|e| e.id);
            op.id.is_none() || recorded != op.id
        });
        self.persist_locked(&st).await
    }

    /// Persist `st`: the committed lockfile with `pending` set to the current
    /// ops, or `None` when no op is left. The caller
    /// holds the lock.
    async fn persist_locked(&self, st: &State) -> Result<()> {
        let mut lf = st.committed.clone();
        lf.pending = (!st.ops.is_empty()).then(|| Pending {
            run: self.run.to_string(),
            ops: st.ops.clone(),
        });
        (self.persist)(&lf).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Lockfile, ObjectEntry};
    use serde_json::json;
    use std::sync::{Arc, Mutex as StdMutex};

    const NOW: u64 = 1_000;

    fn fixed_now() -> u64 {
        NOW
    }

    fn entry(id: u64) -> ObjectEntry {
        ObjectEntry { id, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None }
    }

    /// A journal over a lockfile tracking `queues/main`, whose persist
    /// records every lockfile it is handed. The persist future yields once,
    /// so concurrent callers really contend for the lock.
    fn recording() -> (Journal, Arc<StdMutex<Vec<Lockfile>>>) {
        let mut committed = Lockfile::default();
        committed.upsert("queues", "main", entry(7));
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let persist: PersistFn = Arc::new(move |lf: &Lockfile| {
            sink.lock().unwrap().push(lf.clone());
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok(())
            })
        });
        (Journal::new(committed, persist, fixed_now), seen)
    }

    fn last(seen: &Arc<StdMutex<Vec<Lockfile>>>) -> Lockfile {
        seen.lock().unwrap().last().cloned().expect("something was persisted")
    }

    #[tokio::test]
    async fn begin_without_item_is_not_journaled() {
        let (j, seen) = recording();
        assert_eq!(j.begin(&json!({"name": "a"})).await.unwrap(), None);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn begin_persists_committed_objects_plus_op() {
        let (j, seen) = recording();
        let body = json!({"name": "a", "url": "x"});
        let seq = item("hooks", "a", j.begin(&body)).await.unwrap();
        assert_eq!(seq, Some(1));
        let lf = last(&seen);
        assert_eq!(lf.objects["queues"]["main"].id, 7);
        let p = lf.pending.expect("pending written");
        assert_eq!(p.run, j.run_id());
        assert_eq!(p.run.len(), 32);
        assert!(p.run.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_eq!(p.ops.len(), 1);
        let op = &p.ops[0];
        assert_eq!((op.seq, op.kind.as_str(), op.slug.as_str()), (1, "hooks", "a"));
        assert_eq!(op.id, None);
        assert_eq!(op.started_at_unix, NOW);
        assert_eq!(op.fields, match_fields(&body));
    }

    #[tokio::test]
    async fn finish_sets_id_and_persists() {
        let (j, seen) = recording();
        let seq = item("hooks", "a", j.begin(&json!({}))).await.unwrap().unwrap();
        let before = seen.lock().unwrap().len();
        j.finish(seq, 42).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), before + 1);
        let p = last(&seen).pending.unwrap();
        assert_eq!(p.ops[0].id, Some(42));
    }

    #[tokio::test]
    async fn commit_drops_ops_whose_entry_has_the_id_and_keeps_others() {
        let (j, seen) = recording();
        let a = item("hooks", "a", j.begin(&json!({}))).await.unwrap().unwrap();
        let b = item("hooks", "b", j.begin(&json!({}))).await.unwrap().unwrap();
        j.finish(a, 10).await.unwrap();
        j.finish(b, 11).await.unwrap();

        let mut lf = Lockfile::default();
        lf.upsert("hooks", "a", entry(10));
        j.commit(&lf).await.unwrap();
        let persisted = last(&seen);
        assert!(persisted.objects["hooks"].contains_key("a"));
        assert!(!persisted.objects.contains_key("queues"), "commit replaces the committed objects");
        let p = persisted.pending.expect("b is still pending");
        assert_eq!(p.ops.len(), 1);
        assert_eq!(p.ops[0].slug, "b");
        assert_eq!(p.run, j.run_id());

        lf.upsert("hooks", "b", entry(11));
        j.commit(&lf).await.unwrap();
        assert_eq!(last(&seen).pending, None);
    }

    #[tokio::test]
    async fn abandon_removes_the_op() {
        let (j, seen) = recording();
        let a = item("hooks", "a", j.begin(&json!({}))).await.unwrap().unwrap();
        let _b = item("hooks", "b", j.begin(&json!({}))).await.unwrap().unwrap();
        j.abandon(a).await.unwrap();
        let p = last(&seen).pending.unwrap();
        assert_eq!(p.ops.iter().map(|o| o.slug.as_str()).collect::<Vec<_>>(), ["b"]);
    }

    #[tokio::test]
    async fn claimed_is_committed_observed_existing_and_finished_ids() {
        let (j, seen) = recording();
        let a = item("queues", "a", j.begin(&json!({}))).await.unwrap().unwrap();
        let _b = item("queues", "b", j.begin(&json!({}))).await.unwrap().unwrap();
        let h = item("hooks", "h", j.begin(&json!({}))).await.unwrap().unwrap();
        j.finish(a, 8).await.unwrap();
        j.finish(h, 9).await.unwrap();
        assert_eq!(j.claimed("queues").await, HashSet::from([7, 8]));
        assert_eq!(j.claimed("hooks").await, HashSet::from([9]));
        assert!(j.claimed("labels").await.is_empty());

        let mut live = Lockfile::default();
        live.upsert("labels", "pulled", entry(20));
        let before = seen.lock().unwrap().len();
        j.observe(&live).await;
        assert_eq!(j.claimed("labels").await, HashSet::from([20]));
        assert_eq!(seen.lock().unwrap().len(), before, "observe persists nothing");

        j.note_existing(BTreeMap::from([("labels".to_string(), HashSet::from([30]))])).await;
        assert_eq!(j.claimed("labels").await, HashSet::from([20, 30]));
        assert_eq!(seen.lock().unwrap().len(), before, "note_existing persists nothing");
    }

    #[test]
    fn every_create_kind_has_identity_fields_led_by_name() {
        for kind in [
            "labels", "workspaces", "schemas", "engines", "queues", "inboxes", "hooks", "rules",
            "email_templates", "saved_views", "engine_fields",
        ] {
            assert_eq!(identity_fields(kind).expect(kind)[0], "name", "{kind}");
        }
        assert_eq!(identity_fields("workflows"), None);
    }

    #[test]
    fn secrets_never_hashed() {
        let fields = match_fields(&json!({"name": "a", "secrets": {"k": "v"}}));
        assert_eq!(fields.keys().collect::<Vec<_>>(), ["name"]);
        assert_eq!(fields["name"].len(), 64);
        assert!(match_fields(&json!(["not", "an", "object"])).is_empty());
    }

    #[test]
    fn match_fields_applies_top_level_rules() {
        let trailing = match_fields(&json!({"subject": "hi\n", "modified_at": "t"}));
        let trimmed = match_fields(&json!({"subject": "hi"}));
        assert_eq!(trailing, trimmed);
    }

    #[tokio::test]
    async fn concurrent_begins_serialize() {
        let (j, seen) = recording();
        let slugs = ["a", "b", "c", "d", "e"];
        let body = json!({});
        let futs = slugs.iter().map(|s| item("hooks", s, j.begin(&body)));
        let mut seqs: Vec<u64> =
            futures::future::join_all(futs).await.into_iter().map(|r| r.unwrap().unwrap()).collect();
        seqs.sort();
        assert_eq!(seqs, [1, 2, 3, 4, 5]);

        let states = seen.lock().unwrap().clone();
        assert_eq!(states.len(), 5);
        let mut prev: Vec<u64> = Vec::new();
        for lf in states {
            let now: Vec<u64> = lf.pending.unwrap().ops.iter().map(|o| o.seq).collect();
            assert_eq!(now.len(), prev.len() + 1);
            assert_eq!(&now[..prev.len()], &prev[..], "each state extends the previous one");
            prev = now;
        }
    }
}
