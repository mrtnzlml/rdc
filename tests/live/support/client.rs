use crate::support::config::LiveConfig;
use anyhow::{anyhow, Result};
use rdc::api::RossumClient;

#[allow(dead_code)]
pub struct LiveClient {
    inner: RossumClient,
    pub org_url: String,
}

#[allow(dead_code)]
impl LiveClient {
    pub fn connect(cfg: &LiveConfig) -> Result<LiveClient> {
        let inner = RossumClient::new(cfg.api_base.clone(), cfg.token.clone())?;
        let org_url = format!("{}/organizations/{}", cfg.api_base.trim_end_matches('/'), cfg.org_id);
        Ok(LiveClient { inner, org_url })
    }

    /// Create an object of `kind` from a fully-resolved body. Returns the
    /// server-assigned (id, url). `None` progress = silent.
    pub async fn create(&self, kind: &str, body: &serde_json::Value) -> Result<(u64, String)> {
        let (id, url) = match kind {
            "workspace" => {
                let w = self.inner.create_workspace(body, None).await?;
                (w.id, w.url)
            }
            "queue" => {
                let q = self.inner.create_queue(body, None).await?;
                (q.id, q.url)
            }
            "schema" => {
                let s = self.inner.create_schema(body, None).await?;
                (s.id, s.url)
            }
            "hook" => {
                let h = self.inner.create_hook(body, None).await?;
                (h.id, h.url)
            }
            "inbox" => {
                let i = self.inner.create_inbox(body, None).await?;
                (i.id, i.url)
            }
            "label" => {
                let l = self.inner.create_label(body, None).await?;
                (l.id, l.url)
            }
            "rule" => {
                let r = self.inner.create_rule(body, None).await?;
                (r.id, r.url)
            }
            "email_template" => {
                let t = self.inner.create_email_template(body, None).await?;
                (t.id, t.url)
            }
            other => return Err(anyhow!("create: unsupported kind '{other}'")),
        };
        Ok((id, url))
    }

    pub async fn delete(&self, kind: &str, id: u64) -> Result<()> {
        match kind {
            "workspace" => self.inner.delete_workspace(id, None).await,
            "queue" => self.inner.delete_queue(id, None).await,
            "schema" => self.inner.delete_schema(id, None).await,
            "hook" => self.inner.delete_hook(id, None).await,
            "inbox" => self.inner.delete_inbox(id, None).await,
            "label" => self.inner.delete_label(id, None).await,
            "rule" => self.inner.delete_rule(id, None).await,
            "email_template" => self.inner.delete_email_template(id, None).await,
            other => Err(anyhow!("delete: unsupported kind '{other}'")),
        }
    }

    /// List a kind and return (id, name) for objects whose `name` starts with
    /// `prefix`. Used by teardown and the janitor.
    pub async fn list_ids_by_name_prefix(
        &self,
        kind: &str,
        prefix: &str,
    ) -> Result<Vec<(u64, String)>> {
        let values: Vec<serde_json::Value> = match kind {
            "workspace" => to_values(self.inner.list_workspaces(None).await?)?,
            "queue" => to_values(self.inner.list_queues(None).await?)?,
            "hook" => to_values(self.inner.list_hooks(None).await?)?,
            "label" => to_values(self.inner.list_labels(None).await?)?,
            "rule" => to_values(self.inner.list_rules(None).await?)?,
            "inbox" => to_values(self.inner.list_inboxes(None).await?)?,
            "email_template" => to_values(self.inner.list_email_templates(None).await?)?,
            other => return Err(anyhow!("list: unsupported kind '{other}'")),
        };
        let mut out = Vec::new();
        for v in values {
            // Treat soft-deleted queues as deleted: Rossum's async queue DELETE
            // returns 202 `deletion_requested` and nulls the workspace; such
            // queues linger ~24h but are gone for our purposes.
            if kind == "queue" {
                let soft_deleted = v.get("status").and_then(|s| s.as_str()) == Some("deletion_requested")
                    || v.get("workspace").map(|w| w.is_null()).unwrap_or(true);
                if soft_deleted {
                    continue;
                }
            }
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.starts_with(prefix)
                && let Some(id) = v.get("id").and_then(|i| i.as_u64())
            {
                out.push((id, name.to_string()));
            }
        }
        Ok(out)
    }

    /// Schemas have no list endpoint. Collect the schema ids referenced by queues
    /// whose name starts with `prefix`, parsed from each queue's `schema` URL.
    /// Call this BEFORE deleting the queues.
    pub async fn schema_ids_for_queue_prefix(&self, prefix: &str) -> anyhow::Result<Vec<u64>> {
        let queues = self.inner.list_queues(None).await?;
        let mut out = Vec::new();
        for q in queues {
            let soft_deleted = q.workspace.is_none()
                || q.extra.get("status").and_then(|s| s.as_str()) == Some("deletion_requested");
            if soft_deleted {
                continue;
            }
            if q.name.starts_with(prefix)
                && let Some(url) = q.schema.as_deref()
                && let Some(id) =
                    url.trim_end_matches('/').rsplit('/').next().and_then(|s| s.parse::<u64>().ok())
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Fetch one object as raw JSON (typed getter -> Value).
    /// Kinds with a direct GET endpoint use it; kinds that only have a list
    /// endpoint ("queue", "label") fall through to `find_listed_value`.
    pub async fn get_value(&self, kind: &str, id: u64) -> Result<serde_json::Value> {
        match kind {
            "workspace" => Ok(serde_json::to_value(self.inner.get_workspace(id, None).await?)?),
            "hook" => Ok(serde_json::to_value(self.inner.get_hook(id, None).await?)?),
            "schema" => Ok(serde_json::to_value(self.inner.get_schema(id, None).await?)?),
            "inbox" => Ok(serde_json::to_value(self.inner.get_inbox(id, None).await?)?),
            // "queue" has no GET-by-id endpoint; use the list-based fallback.
            "queue" => self
                .find_listed_value("queue", id)
                .await?
                .ok_or_else(|| anyhow!("get_value: queue {id} not found")),
            other => Err(anyhow!("get_value: unsupported kind '{other}'")),
        }
    }

    /// PATCH only the `name` field of an object via the generic value endpoint.
    pub async fn patch_name(&self, kind: &str, id: u64, name: &str) -> anyhow::Result<()> {
        self.patch_fields(kind, id, serde_json::json!({ "name": name })).await
    }

    /// PATCH arbitrary fields of an object via the generic value endpoint.
    /// `fields` is a partial body, e.g. `json!({ "color": "#00ff00" })`.
    pub async fn patch_fields(
        &self,
        kind: &str,
        id: u64,
        fields: serde_json::Value,
    ) -> anyhow::Result<()> {
        let endpoint = match kind {
            "queue" => "queues",
            "workspace" => "workspaces",
            "hook" => "hooks",
            "label" => "labels",
            "rule" => "rules",
            "schema" => "schemas",
            "inbox" => "inboxes",
            "email_template" => "email_templates",
            other => anyhow::bail!("patch_fields: unsupported kind '{other}'"),
        };
        let path = format!("/{endpoint}/{id}");
        self.inner.patch_value(&path, &fields, None).await?;
        Ok(())
    }

    /// Fetch one object as raw JSON by listing its kind and finding it by id.
    /// Use for kinds that have a list endpoint but no get-by-id (e.g. labels).
    pub async fn find_listed_value(
        &self,
        kind: &str,
        id: u64,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let values: Vec<serde_json::Value> = match kind {
            "label" => self
                .inner
                .list_labels(None)
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?,
            "queue" => self
                .inner
                .list_queues(None)
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?,
            "hook" => self
                .inner
                .list_hooks(None)
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?,
            "rule" => self
                .inner
                .list_rules(None)
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?,
            "inbox" => self
                .inner
                .list_inboxes(None)
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?,
            "workspace" => self
                .inner
                .list_workspaces(None)
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?,
            other => anyhow::bail!("find_listed_value: unsupported kind '{other}'"),
        };
        Ok(values
            .into_iter()
            .find(|v| v.get("id").and_then(|i| i.as_u64()) == Some(id)))
    }
}

#[allow(dead_code)]
fn to_values<T: serde::Serialize>(items: Vec<T>) -> Result<Vec<serde_json::Value>> {
    items.into_iter().map(|i| Ok(serde_json::to_value(i)?)).collect()
}
