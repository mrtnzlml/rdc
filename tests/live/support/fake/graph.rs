//! Back-reference maintenance and cascade delete for the object graph
//! `state.rs` stores.
//!
//! `relink`/`unlink` keep every parent -> child back-reference in sync with
//! whichever field on the child actually names the relationship (a queue's
//! `workspace`/`schema`, an inbox's `queues`, a hook's or rule's `queues`);
//! `cascade_queue_delete` removes what the real API removes alongside a
//! queue once its grace period elapses. All seven functions here are
//! `OrgState` methods, defined as a second `impl OrgState` block — see the
//! doc comment on `OrgState` in `state.rs`.

use serde_json::{json, Value};

use super::state::{Deletion, OrgState};

impl OrgState {
    /// Remove a queue and everything the server removes with it: its
    /// auto-created email templates and its inbox. The SCHEMA survives —
    /// teardown deletes it explicitly, and needs a retry precisely because it
    /// outlives the queue's purge (`tests/live/support/teardown.rs:38`).
    ///
    /// No `unlink("queues", queue_id)` call here: `delete()` already severed
    /// `workspace.queues` / `schema.queues` at request time, before this
    /// queue was nulled out and put on the grace clock — see the comment
    /// there.
    pub(super) fn cascade_queue_delete(&mut self, queue_id: u64) {
        let queue_url = self.url("queues", queue_id);
        let doomed_templates: Vec<u64> = self
            .objects
            .get("email_templates")
            .map(|m| {
                m.iter()
                    .filter(|(_, t)| {
                        t.get("queue").and_then(Value::as_str) == Some(queue_url.as_str())
                    })
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default();
        let doomed_inboxes: Vec<u64> = self
            .objects
            .get("inboxes")
            .map(|m| {
                m.iter()
                    .filter(|(_, i)| {
                        i.get("queues")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().any(|q| q.as_str() == Some(queue_url.as_str())))
                            .unwrap_or(false)
                    })
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default();
        for id in doomed_templates {
            self.objects.get_mut("email_templates").and_then(|m| m.remove(&id));
        }
        for id in doomed_inboxes {
            self.objects.get_mut("inboxes").and_then(|m| m.remove(&id));
        }
        self.objects.get_mut("queues").and_then(|m| m.remove(&queue_id));
    }

    /// Push `child_url` into `parent.<field>` if it is not already there.
    fn add_ref(
        &mut self,
        parent_kind: &'static str,
        parent_url: &str,
        field: &str,
        child_url: &str,
    ) {
        let Some(id) = parent_url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        let Some(parent) = self.objects.get_mut(parent_kind).and_then(|m| m.get_mut(&id)) else {
            return;
        };
        let Some(obj) = parent.as_object_mut() else { return };
        let arr = obj.entry(field.to_string()).or_insert_with(|| json!([]));
        if let Some(list) = arr.as_array_mut() {
            let v = json!(child_url);
            if !list.contains(&v) {
                list.push(v);
            }
        }
    }

    /// The inverse of `add_ref`.
    fn remove_ref(
        &mut self,
        parent_kind: &'static str,
        parent_url: &str,
        field: &str,
        child_url: &str,
    ) {
        let Some(id) = parent_url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        let Some(parent) = self.objects.get_mut(parent_kind).and_then(|m| m.get_mut(&id)) else {
            return;
        };
        if let Some(list) = parent.get_mut(field).and_then(|v| v.as_array_mut()) {
            list.retain(|v| v != &json!(child_url));
        }
    }

    /// Set `<kind>/<id from url>.<field>` to `value`.
    fn set_field(&mut self, kind: &'static str, url: &str, field: &str, value: Value) {
        let Some(id) = url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        if let Some(obj) = self
            .objects
            .get_mut(kind)
            .and_then(|m| m.get_mut(&id))
            .and_then(|v| v.as_object_mut())
        {
            obj.insert(field.to_string(), value);
        }
    }

    /// The inverse of `set_field` — vacates the key entirely rather than
    /// setting it to `null`. Some Rossum fields (e.g. `queue.inbox`, see
    /// `src/model/queue.rs`) are `Option<T>` with
    /// `skip_serializing_if = "Option::is_none"` because the real API
    /// rejects `null` on PATCH ("This field may not be null."); the fake
    /// must vacate the key the same way the real server does, or a re-fetch
    /// would hand back a shape the real API never emits.
    fn remove_field(&mut self, kind: &'static str, url: &str, field: &str) {
        let Some(id) = url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        if let Some(obj) = self
            .objects
            .get_mut(kind)
            .and_then(|m| m.get_mut(&id))
            .and_then(|v| v.as_object_mut())
        {
            obj.remove(field);
        }
    }

    /// Grow every back-reference this object's own refs imply. The real API
    /// maintains these server-side; `pull::queues::refresh_backrefs` exists
    /// because they change under rdc's feet.
    pub(super) fn relink(&mut self, kind: &'static str, id: u64) {
        let Some(me) = self.get(kind, id) else { return };
        let my_url = self.url(kind, id);
        match kind {
            "queues" => {
                if let Some(ws) = me.get("workspace").and_then(|v| v.as_str()) {
                    self.add_ref("workspaces", ws, "queues", &my_url);
                }
                if let Some(sc) = me.get("schema").and_then(|v| v.as_str()) {
                    self.add_ref("schemas", sc, "queues", &my_url);
                }
            }
            "inboxes" => {
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    if let Some(q) = q.as_str() {
                        self.set_field("queues", q, "inbox", json!(my_url));
                    }
                }
            }
            "hooks" | "rules" => {
                let field = if kind == "hooks" { "hooks" } else { "rules" };
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    if let Some(q) = q.as_str() {
                        self.add_ref("queues", q, field, &my_url);
                    }
                }
            }
            _ => {}
        }
    }

    /// The inverse, so a delete leaves no dangling back-reference.
    pub(super) fn unlink(&mut self, kind: &'static str, id: u64) {
        let Some(me) = self.get(kind, id) else { return };
        let my_url = self.url(kind, id);
        match kind {
            "queues" => {
                if let Some(ws) = me.get("workspace").and_then(|v| v.as_str()) {
                    self.remove_ref("workspaces", ws, "queues", &my_url);
                }
                if let Some(sc) = me.get("schema").and_then(|v| v.as_str()) {
                    self.remove_ref("schemas", sc, "queues", &my_url);
                }
            }
            "inboxes" => {
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    if let Some(q) = q.as_str() {
                        self.remove_field("queues", q, "inbox");
                    }
                }
            }
            "hooks" | "rules" => {
                let field = if kind == "hooks" { "hooks" } else { "rules" };
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    if let Some(q) = q.as_str() {
                        self.remove_ref("queues", q, field, &my_url);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn st() -> OrgState {
        OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1)
    }

    fn seeded_graph(s: &mut OrgState) -> (u64, u64, u64) {
        let ws = s.create("workspaces", json!({ "name": "Main" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "Invoices" })).unwrap();
        let q = s
            .create(
                "queues",
                json!({ "name": "Invoices", "workspace": ws["url"], "schema": sc["url"] }),
            )
            .unwrap();
        (
            ws["id"].as_u64().unwrap(),
            sc["id"].as_u64().unwrap(),
            q["id"].as_u64().unwrap(),
        )
    }

    #[test]
    fn creating_a_queue_grows_its_workspace_and_schema() {
        let mut s = st();
        let (ws, sc, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        assert_eq!(s.get("workspaces", ws).unwrap()["queues"], json!([q_url]));
        assert_eq!(
            s.get("schemas", sc).unwrap()["queues"],
            json!([q_url]),
            "schema.queues gains the queue on create (pull/queues.rs refresh_backrefs)"
        );
    }

    #[test]
    fn creating_an_inbox_sets_its_queues_inbox() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let inbox = s
            .create(
                "inboxes",
                json!({ "name": "In", "email_prefix": "p", "queues": [s.url("queues", q)] }),
            )
            .unwrap();
        assert_eq!(s.get("queues", q).unwrap()["inbox"], inbox["url"]);
    }

    #[test]
    fn creating_a_hook_or_rule_grows_its_queues_back_ref() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        let hook = s
            .create("hooks", json!({ "name": "H", "queues": [q_url.clone()] }))
            .unwrap();
        let rule = s
            .create("rules", json!({ "name": "R", "queues": [q_url.clone()] }))
            .unwrap();
        assert_eq!(s.get("queues", q).unwrap()["hooks"], json!([hook["url"]]));
        assert_eq!(s.get("queues", q).unwrap()["rules"], json!([rule["url"]]));
    }

    #[test]
    fn deleting_a_child_shrinks_the_back_ref() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        let hook = s
            .create("hooks", json!({ "name": "H", "queues": [q_url] }))
            .unwrap();
        s.delete("hooks", hook["id"].as_u64().unwrap()).unwrap();
        assert_eq!(s.get("queues", q).unwrap()["hooks"], json!([]));
    }

    #[test]
    fn patching_a_hooks_queues_moves_the_back_ref() {
        let mut s = st();
        let (_, _, q1) = seeded_graph(&mut s);
        let q1_obj = s.get("queues", q1).unwrap();
        let q2 = s
            .create(
                "queues",
                json!({ "name": "Q2", "workspace": q1_obj["workspace"], "schema": q1_obj["schema"] }),
            )
            .unwrap();
        let q2_id = q2["id"].as_u64().unwrap();
        let q1_url = s.url("queues", q1);
        let q2_url = s.url("queues", q2_id);
        let hook = s
            .create("hooks", json!({ "name": "H", "queues": [q1_url] }))
            .unwrap();
        let hook_id = hook["id"].as_u64().unwrap();
        s.patch("hooks", hook_id, &json!({ "queues": [q2_url] })).unwrap();
        assert_eq!(
            s.get("queues", q1).unwrap()["hooks"],
            json!([]),
            "the old parent must lose the back-ref, not just the new one gain it"
        );
        assert_eq!(s.get("queues", q2_id).unwrap()["hooks"], json!([s.url("hooks", hook_id)]));
    }

    #[test]
    fn patching_a_queues_workspace_moves_the_back_ref() {
        let mut s = st();
        let a = s.create("workspaces", json!({ "name": "A" })).unwrap();
        let b = s.create("workspaces", json!({ "name": "B" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "S" })).unwrap();
        let a_id = a["id"].as_u64().unwrap();
        let b_id = b["id"].as_u64().unwrap();
        let q = s
            .create("queues", json!({ "name": "Q", "workspace": a["url"], "schema": sc["url"] }))
            .unwrap();
        let q_id = q["id"].as_u64().unwrap();
        s.patch("queues", q_id, &json!({ "workspace": b["url"] })).unwrap();
        assert_eq!(
            s.get("workspaces", a_id).unwrap()["queues"],
            json!([]),
            "the queue must not still be listed under its old workspace"
        );
        assert_eq!(
            s.get("workspaces", b_id).unwrap()["queues"],
            json!([s.url("queues", q_id)])
        );
    }

    #[test]
    fn deleting_an_inbox_removes_its_queues_inbox_key() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let inbox = s
            .create(
                "inboxes",
                json!({ "name": "In", "email_prefix": "p", "queues": [s.url("queues", q)] }),
            )
            .unwrap();
        s.delete("inboxes", inbox["id"].as_u64().unwrap()).unwrap();
        let queue = s.get("queues", q).unwrap();
        assert!(
            !queue.as_object().unwrap().contains_key("inbox"),
            "the key must be vacated (real API rejects inbox: null), not set to null: {queue:?}"
        );
    }

    #[test]
    fn deleting_a_queue_shrinks_its_workspace_and_schema() {
        let mut s = st();
        let (ws, sc, q) = seeded_graph(&mut s);
        assert_eq!(s.delete("queues", q).unwrap(), Deletion::Requested);
        s.tick_deletions();
        assert_eq!(s.get("workspaces", ws).unwrap()["queues"], json!([]));
        assert_eq!(s.get("schemas", sc).unwrap()["queues"], json!([]));
    }

    /// `patch`'s unlink-then-relink round trip preserves MEMBERSHIP, not
    /// order — see the comment on the `self.unlink(kind, id)` call in
    /// `patch`. This pins the property that actually matters: patching one
    /// child does not evict its siblings from the shared parent back-ref.
    /// Compared as a sorted set on purpose, so this does not accidentally
    /// re-assert an order guarantee the fake deliberately does not make.
    #[test]
    fn patching_one_child_keeps_every_sibling_back_ref() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        let h1 = s
            .create("hooks", json!({ "name": "H1", "queues": [q_url.clone()] }))
            .unwrap();
        let h2 = s.create("hooks", json!({ "name": "H2", "queues": [q_url] })).unwrap();
        let h1_id = h1["id"].as_u64().unwrap();
        let h1_url = s.url("hooks", h1_id);
        let h2_url = s.url("hooks", h2["id"].as_u64().unwrap());
        // A drift-repair PATCH that never touches `queues` at all (the shape
        // `src/cli/push/hooks.rs:544` sends whenever ANY field of a hook has
        // drifted, not only its `queues`).
        s.patch("hooks", h1_id, &json!({ "name": "H1 renamed" })).unwrap();
        let mut got: Vec<String> = s.get("queues", q).unwrap()["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        got.sort();
        let mut want = vec![h1_url, h2_url];
        want.sort();
        assert_eq!(
            got, want,
            "both siblings must survive a patch to just one of them, order aside"
        );
    }

    #[test]
    fn a_queue_delete_cascades_to_its_templates_and_inbox() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        s.create("inboxes", json!({ "name": "In", "email_prefix": "p", "queues": [q_url] }))
            .unwrap();
        assert_eq!(s.ids("email_templates").len(), 5);
        assert_eq!(s.ids("inboxes").len(), 1);
        s.delete("queues", q).unwrap();
        s.tick_deletions();
        assert!(s.ids("email_templates").is_empty(), "templates go with the queue");
        assert!(s.ids("inboxes").is_empty(), "so does the inbox");
    }

    #[test]
    fn a_cascaded_queue_delete_leaves_its_schema_for_the_caller() {
        // Teardown deletes schemas explicitly, with a retry, because the
        // schema outlives the queue's purge (`tests/live/support/teardown.rs:38`).
        let mut s = st();
        let (_, sc, q) = seeded_graph(&mut s);
        s.delete("queues", q).unwrap();
        s.tick_deletions();
        assert!(s.get("schemas", sc).is_some());
        assert_eq!(s.delete("schemas", sc).unwrap(), Deletion::Gone);
    }
}
