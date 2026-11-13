//! A test-only host store for the sync state: what a browser host does with
//! rdc's `SnapshotBackend` callbacks, over Data Storage — the fake's or a real
//! org's. rdc itself no longer talks to Data Storage for the state.
//!
//! One document per `(key, env)`: `{_id: "<key>/<env>", rev, snapshot}`, the
//! snapshot as base64. A first save is an `insert_many`; a later one a
//! `replace_one` filtered on `{_id, rev: expected}`. A lost answer is settled
//! by reading the document back, as the README asks a host to.

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use rdc::rt::BoxFuture;
use rdc::state::snapshot_store::SnapshotBackend;
use serde_json::{Value, json};

/// The collection these tests keep the state in; its `__` prefix keeps it
/// out of MDH sync, as any host's must be.
pub const COLLECTION: &str = "__rdc_state";

pub struct DsBackend {
    http: reqwest::Client,
    /// `<host>/svc/data-storage/api`.
    base: String,
    token: String,
    collection: String,
    key: String,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

impl DsBackend {
    /// Over the Data Storage of the org at `api_base`.
    pub fn new(api_base: &str, token: &str, collection: &str, key: &str) -> Self {
        let base = rdc::config::EnvConfig { api_base: api_base.to_string(), org_id: 0 }.data_storage_base();
        Self { http: reqwest::Client::new(), base, token: token.to_string(), collection: collection.to_string(), key: key.to_string() }
    }

    fn id(&self, env: &str) -> String {
        format!("{}/{env}", self.key)
    }

    /// POST `verb` with `body` (plus the collection); `Ok(Err(status, msg))`
    /// for an answered error.
    pub async fn post(&self, verb: &str, mut body: Value) -> Result<std::result::Result<Value, (u16, String)>> {
        body["collectionName"] = json!(self.collection);
        let resp = self
            .http
            .post(format!("{}/v1/{verb}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {verb}"))?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if (200..300).contains(&status) && v["code"] == "ok" {
            Ok(Ok(v["result"].clone()))
        } else {
            Ok(Err((status, v["message"].as_str().unwrap_or_default().to_string())))
        }
    }

    /// Every `_id` under this key, sorted: a run's documents, to clean up.
    pub async fn stored_ids(&self) -> Result<Vec<String>> {
        let mut prefix = String::from("^");
        for c in format!("{}/", self.key).chars() {
            if !c.is_ascii_alphanumeric() {
                prefix.push('\\');
            }
            prefix.push(c);
        }
        let found = self
            .post("data/find", json!({ "query": { "_id": { "$regex": prefix } }, "projection": { "_id": 1 } }))
            .await?
            .map_err(|(s, m)| anyhow!("find answered {s}: {m}"))?;
        let mut ids: Vec<String> =
            found.as_array().into_iter().flatten().filter_map(|d| d["_id"].as_str().map(str::to_string)).collect();
        ids.sort();
        Ok(ids)
    }

    /// Delete every document under this key; how many there were.
    pub async fn delete_all(&self) -> Result<usize> {
        let ids = self.stored_ids().await?;
        if !ids.is_empty() {
            self.post("data/delete_many", json!({ "filter": { "_id": { "$in": ids } }, "waitForFullWrite": true }))
                .await?
                .map_err(|(s, m)| anyhow!("delete_many answered {s}: {m}"))?;
        }
        Ok(ids.len())
    }

    async fn read(&self, env: &str) -> Result<rdc::state::snapshot_store::Stored> {
        let found = self
            .post("data/find", json!({ "query": { "_id": self.id(env) } }))
            .await?
            .map_err(|(s, m)| anyhow!("find answered {s}: {m}"))?;
        let Some(doc) = found.as_array().and_then(|a| a.first()) else {
            return Ok(None);
        };
        let rev = doc["rev"].as_u64().ok_or_else(|| anyhow!("no rev"))?;
        let bytes = b64().decode(doc["snapshot"].as_str().ok_or_else(|| anyhow!("no snapshot"))?)?;
        Ok(Some((rev, bytes)))
    }
}

impl SnapshotBackend for DsBackend {
    fn load<'a>(&'a self, env: &'a str) -> BoxFuture<'a, Result<rdc::state::snapshot_store::Stored>> {
        Box::pin(self.read(env))
    }

    fn save<'a>(
        &'a self,
        env: &'a str,
        snapshot: Vec<u8>,
        expected_rev: Option<u64>,
        next_rev: u64,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let doc = json!({ "rev": next_rev, "snapshot": b64().encode(&snapshot) });
            let answer = match expected_rev {
                Some(rev) => self
                    .post(
                        "data/replace_one",
                        json!({ "filter": { "_id": self.id(env), "rev": rev }, "replacement": doc,
                                "options": { "upsert": false }, "waitForFullWrite": true }),
                    )
                    .await
                    .map(|r| r.map(|v| v["matched_count"].as_u64() == Some(1))),
                None => {
                    let mut first = doc.clone();
                    first["_id"] = json!(self.id(env));
                    self.post("data/insert_many", json!({ "documents": [first], "ordered": false, "waitForFullWrite": true }))
                        .await
                        .map(|r| r.map(|_| true))
                }
            };
            match answer {
                Ok(Ok(true)) => Ok(true),
                // Not written, or the answer is unknown: what is stored says.
                other => match self.read(env).await? {
                    Some((rev, bytes)) if rev == next_rev && bytes == snapshot => Ok(true),
                    stored if stored.as_ref().map(|(r, _)| *r) != expected_rev => Ok(false),
                    _ => match other {
                        Err(e) => Err(e),
                        Ok(Err((s, m))) => Err(anyhow!("Data Storage answered {s}: {m}")),
                        Ok(Ok(_)) => Ok(false),
                    },
                },
            }
        })
    }
}
