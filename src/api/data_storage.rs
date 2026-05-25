//! Rossum Data Storage (MDH) API client.
//!
//! The MDH API is RPC-style — every call is a `POST` to
//! `<base>/v1/<resource>/<verb>` with a JSON body, and every response is
//! wrapped in `{code, message, result}`. Our pull driver only uses the
//! `list` verbs (read-only); push/edit verbs (`create`, `drop`, `rename`,
//! `data/find`, `data/insert_*`) are intentionally not implemented here
//! because the snapshot scope is collection metadata + indexes, not rows.
//!
//! Base URL convention: `<host>/svc/data-storage/api`. For example,
//! `https://elis.rossum.ai/svc/data-storage/api`. We append `/v1/...` per
//! call.
//!
//! Note on host: the API and Data Storage services share the same parent
//! domain. The API lives under the `api.` subdomain
//! (`api.elis.rossum.ai/v1/...`) while Data Storage lives at the bare
//! parent domain plus a service path (`elis.rossum.ai/svc/data-storage/api`).

use crate::api::ApiError;
use crate::api::retry::ProgressHandle;
use crate::model::Collection;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct DataStorageClient {
    base_url: String,
    token: String,
    http: Client,
}

/// Generic envelope wrapping every Data Storage response.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    code: String,
    #[serde(default)]
    message: String,
    result: T,
}

impl DataStorageClient {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = Client::builder()
            // See RossumClient::new — same Nagle-off rationale.
            .tcp_nodelay(true)
            .build()
            .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))?;
        Ok(Self { base_url, token, http })
    }

    /// `POST /v1/collections/list` with `{nameOnly: false}` returns full
    /// collection metadata (name, type, options, info, idIndex).
    pub async fn list_collections(&self, progress: ProgressHandle) -> Result<Vec<Collection>> {
        self.post_envelope("/v1/collections/list", json!({"nameOnly": false}), progress).await
    }

    /// `POST /v1/indexes/list` with `{collectionName, nameOnly: false}` —
    /// regular MongoDB-style indexes (incl. the implicit `_id_` index).
    pub async fn list_indexes(&self, collection: &str, progress: ProgressHandle) -> Result<Vec<Value>> {
        self.post_envelope("/v1/indexes/list", json!({
            "collectionName": collection,
            "nameOnly": false,
        }), progress).await
    }

    /// `POST /v1/search_indexes/list` — Atlas Search indexes.
    pub async fn list_search_indexes(&self, collection: &str, progress: ProgressHandle) -> Result<Vec<Value>> {
        self.post_envelope("/v1/search_indexes/list", json!({
            "collectionName": collection,
            "nameOnly": false,
        }), progress).await
    }

    async fn post_envelope<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
        progress: ProgressHandle,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        // Data Storage is a separate service from the core API and is not
        // subject to the `default.core_api` 10 req/s policy that
        // [`RossumClient`] paces itself against — no client-side limiter
        // here.
        let resp = crate::api::retry::send_with_retry(
            || self.http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.token))
                .json(&body),
            &format!("POST {url}"),
            progress,
            None,
        ).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body, env: None }.into());
        }
        // Decode the envelope as untyped first so we can surface a useful
        // error when code != "ok" without `result` being a parseable T.
        let env: Envelope<Value> = resp
            .json()
            .await
            .with_context(|| format!("decoding response from {url}"))?;
        if env.code != "ok" {
            return Err(ApiError::Status {
                status: status.as_u16(),
                body: format!("Data Storage API returned code='{}', message='{}'", env.code, env.message),
                env: None,
            }.into());
        }
        let typed: T = serde_json::from_value(env.result)
            .with_context(|| format!("decoding `result` field from {url}"))?;
        Ok(typed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_deserializes_ok_response() {
        let raw = r#"{"code":"ok","message":"","result":["a","b"]}"#;
        let e: Envelope<Vec<String>> = serde_json::from_str(raw).unwrap();
        assert_eq!(e.code, "ok");
        assert_eq!(e.result, vec!["a", "b"]);
    }

    #[test]
    fn envelope_deserializes_collection_with_uuid() {
        // Mongo-style binary-encoded UUID ends up in extra.
        let raw = r#"{
            "code":"ok",
            "message":"",
            "result":[
              {"name":"vendors","type":"collection","options":{},
               "info":{"readOnly":false,"uuid":{"$binary":{"base64":"AA==","subType":"04"}}},
               "idIndex":{"v":2,"key":{"_id":1},"name":"_id_"}}
            ]
        }"#;
        let e: Envelope<Vec<Collection>> = serde_json::from_str(raw).unwrap();
        assert_eq!(e.result.len(), 1);
        assert_eq!(e.result[0].name, "vendors");
        // Everything besides `name` lands in `extra`.
        assert!(e.result[0].extra.contains_key("info"));
        assert!(e.result[0].extra.contains_key("idIndex"));
    }
}
