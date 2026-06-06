//! Identity-collision detection — a fail-loud safety net.
//!
//! rdc identifies the queue-nested kinds (`queues`, `schemas`, `inboxes`) by
//! a bare, name-derived slug (`slugify(name)`) that is deduplicated only
//! *per workspace* (`per_ws_used_q_slugs`), yet stored and matched in a
//! single global namespace (`lockfile.objects["queues"]: BTreeMap<slug, _>`,
//! and the `(kind, slug)` keys the classifier merges on). Two queues with the
//! same name in *different* workspaces therefore collapse to one identity
//! key: one queue's lockfile entry silently overwrites the other, and a
//! later sync cross-attributes one queue's `queue.json` / `schema` /
//! formulas / `inbox` onto the other's remote object.
//!
//! Detecting the queue-slug collision covers all three kinds, because schemas
//! and inboxes are keyed by the very same bare queue slug.
//!
//! This detector is a *defense-in-depth* guard: until identity is re-keyed on
//! the stable server `id` (see the architecture plan), it lets sync abort
//! loudly on such a collision instead of silently corrupting data. The
//! companion kinds with parent-qualified composite keys (`email_templates`,
//! `engine_fields`, `workflow_steps`) are not vulnerable and are not checked
//! here.

use std::collections::BTreeMap;

/// One remote object that landed on a shared identity key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionMember {
    /// Stable Rossum object id — the thing identity *should* be keyed on.
    pub id: u64,
    /// The object's remote `name`.
    pub name: String,
    /// Human context for the message (the workspace slug, for queue-nested
    /// kinds).
    pub parent: String,
}

/// An identity key `(kind, slug)` claimed by two or more distinct ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub kind: String,
    pub slug: String,
    pub members: Vec<CollisionMember>,
}

/// Accumulates the identity key each remote object is assigned and reports
/// any key claimed by more than one distinct id.
#[derive(Default)]
pub struct CollisionDetector {
    seen: BTreeMap<(String, String), Vec<CollisionMember>>,
}

impl CollisionDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that remote object `id` (named `name`, under `parent`) was
    /// assigned identity key `(kind, slug)`. Observing the same `id` twice
    /// for the same key is idempotent.
    pub fn observe(&mut self, kind: &str, slug: &str, id: u64, name: &str, parent: &str) {
        let members = self
            .seen
            .entry((kind.to_string(), slug.to_string()))
            .or_default();
        if members.iter().any(|m| m.id == id) {
            return;
        }
        members.push(CollisionMember {
            id,
            name: name.to_string(),
            parent: parent.to_string(),
        });
    }

    /// Identity keys claimed by two or more *distinct* ids. `observe`
    /// dedupes by id, so a member count of two or more means two or more
    /// distinct remote objects landed on the same `(kind, slug)`.
    pub fn collisions(&self) -> Vec<Collision> {
        self.seen
            .iter()
            .filter(|(_, members)| members.len() >= 2)
            .map(|((kind, slug), members)| Collision {
                kind: kind.clone(),
                slug: slug.clone(),
                members: members.clone(),
            })
            .collect()
    }
}

/// Format detected collisions into a single actionable, loud error message.
pub fn format_collisions(collisions: &[Collision]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = write!(
        s,
        "identity collision: {} object name(s) are shared by multiple objects \
         that rdc cannot tell apart.\n\n\
         rdc currently identifies queues, schemas and inboxes by their name \
         (per workspace), not by their stable id, so two same-named queues in \
         different workspaces collapse onto one identity — syncing would \
         silently cross-attribute one queue's schema/formulas/inbox onto the \
         other. Aborting before any write.\n",
        collisions.len(),
    );
    for c in collisions {
        let _ = write!(s, "\n  {} \"{}\" is claimed by:", c.kind, c.slug);
        for m in &c.members {
            let _ = write!(
                s,
                "\n    - id {} (name \"{}\", workspace \"{}\")",
                m.id, m.name, m.parent
            );
        }
    }
    let _ = write!(
        s,
        "\n\nResolve by giving each object a distinct name on the remote \
         (e.g. rename one queue), then re-run `rdc sync`."
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_two_distinct_ids_on_one_identity_key() {
        let mut d = CollisionDetector::new();
        d.observe(
            "queues",
            "shared-queue",
            100,
            "Shared Queue",
            "workspace-alpha",
        );
        d.observe(
            "queues",
            "shared-queue",
            200,
            "Shared Queue",
            "workspace-beta",
        );
        let collisions = d.collisions();
        assert_eq!(collisions.len(), 1, "one identity key is contested");
        let c = &collisions[0];
        assert_eq!(c.kind, "queues");
        assert_eq!(c.slug, "shared-queue");
        assert_eq!(c.members.len(), 2);
        let ids: Vec<u64> = c.members.iter().map(|m| m.id).collect();
        assert!(ids.contains(&100) && ids.contains(&200));
    }

    #[test]
    fn no_collision_for_distinct_slugs() {
        // Two same-name queues in ONE workspace get deduped slugs upstream
        // (`shared-queue` / `shared-queue-2`) — distinct identity keys, safe.
        let mut d = CollisionDetector::new();
        d.observe("queues", "shared-queue", 100, "Shared Queue", "ws");
        d.observe("queues", "shared-queue-2", 200, "Shared Queue", "ws");
        assert!(d.collisions().is_empty());
    }

    #[test]
    fn observing_same_id_twice_is_idempotent() {
        let mut d = CollisionDetector::new();
        d.observe("queues", "shared-queue", 100, "Shared Queue", "ws");
        d.observe("queues", "shared-queue", 100, "Shared Queue", "ws");
        assert!(
            d.collisions().is_empty(),
            "the same object re-observed is not a collision"
        );
    }

    #[test]
    fn format_message_names_kind_slug_ids_and_workspaces() {
        let c = Collision {
            kind: "queues".to_string(),
            slug: "shared-queue".to_string(),
            members: vec![
                CollisionMember {
                    id: 100,
                    name: "Shared Queue".to_string(),
                    parent: "workspace-alpha".to_string(),
                },
                CollisionMember {
                    id: 200,
                    name: "Shared Queue".to_string(),
                    parent: "workspace-beta".to_string(),
                },
            ],
        };
        let msg = format_collisions(&[c]);
        assert!(msg.contains("identity collision"), "msg: {msg}");
        assert!(msg.contains("shared-queue"), "msg: {msg}");
        assert!(msg.contains("100") && msg.contains("200"), "msg: {msg}");
        assert!(
            msg.contains("workspace-alpha") && msg.contains("workspace-beta"),
            "msg: {msg}"
        );
    }
}
