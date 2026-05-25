//! Rossum Data Storage (MDH) API client.
//!
//! The MDH API is RPC-style — every call is a `POST` to
//! `<base>/v1/<resource>/<verb>` with a JSON body, and every response is
//! wrapped in `{code, message, result}`. Collection CRUD
//! (`collections/create`, `collections/drop`, `collections/rename`) and
//! row-data verbs (`data/find`, `data/insert_*`) are intentionally not
//! implemented here — the snapshot scope is collection metadata +
//! indexes, not the row data itself, and dataset creation/removal is a
//! UI-side concern. Index CRUD (`indexes/create`, `indexes/drop`, and
//! the corresponding `search_indexes/*`) IS implemented to support
//! user edits to `envs/<env>/mdh/<slug>/indexes.json` being pushed.
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

/// Generic envelope wrapping every Data Storage response. Write
/// endpoints (`*/create`, `*/drop`) return `{code, message}` without a
/// `result` field, so we model `result` as optional and use
/// [`post_envelope_void`] for those, leaving [`post_envelope`] for the
/// read paths that need to decode the body.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default = "Option::default")]
    result: Option<T>,
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

    /// `POST /v1/indexes/create` — create a regular MongoDB index on
    /// the given collection. `keys` is the standard mongo key spec
    /// (`{field: 1 | -1 | "text"}`); `options` carries `unique`,
    /// `sparse`, `expireAfterSeconds`, etc. when relevant. The
    /// response carries no body besides the envelope status.
    pub async fn create_index(
        &self,
        collection: &str,
        index_name: &str,
        keys: &Value,
        options: &Value,
        progress: ProgressHandle,
    ) -> Result<()> {
        self.post_envelope_void(
            "/v1/indexes/create",
            json!({
                "collectionName": collection,
                "indexName": index_name,
                "keys": keys,
                "options": options,
            }),
            progress,
        )
        .await
    }

    /// `POST /v1/indexes/drop` — drop a regular MongoDB index by name.
    /// Server-managed indexes (`_id_`) reject the call; callers must
    /// filter those out before invoking.
    pub async fn drop_index(
        &self,
        collection: &str,
        index_name: &str,
        progress: ProgressHandle,
    ) -> Result<()> {
        self.post_envelope_void(
            "/v1/indexes/drop",
            json!({
                "collectionName": collection,
                "indexName": index_name,
            }),
            progress,
        )
        .await
    }

    /// `POST /v1/search_indexes/create` — create an Atlas Search index
    /// on the given collection. `mappings` is the field-mapping spec;
    /// `analyzers` carries any custom analyzer definitions (typically
    /// omitted / an empty array for the default analyzer).
    pub async fn create_search_index(
        &self,
        collection: &str,
        index_name: &str,
        mappings: &Value,
        analyzers: &Value,
        progress: ProgressHandle,
    ) -> Result<()> {
        self.post_envelope_void(
            "/v1/search_indexes/create",
            json!({
                "collectionName": collection,
                "indexName": index_name,
                "mappings": mappings,
                "analyzers": analyzers,
            }),
            progress,
        )
        .await
    }

    /// `POST /v1/search_indexes/drop` — drop an Atlas Search index
    /// by name. Atlas tears down the underlying index asynchronously
    /// in the background after the API returns.
    pub async fn drop_search_index(
        &self,
        collection: &str,
        index_name: &str,
        progress: ProgressHandle,
    ) -> Result<()> {
        self.post_envelope_void(
            "/v1/search_indexes/drop",
            json!({
                "collectionName": collection,
                "indexName": index_name,
            }),
            progress,
        )
        .await
    }

    async fn post_envelope<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
        progress: ProgressHandle,
    ) -> Result<T> {
        let (status, env) = self.send_envelope(path, body, progress).await?;
        let result = env.result.ok_or_else(|| ApiError::Status {
            status: status.as_u16(),
            body: format!(
                "Data Storage API returned code='ok' but no `result` field for {path}",
            ),
            env: None,
        })?;
        let typed: T = serde_json::from_value(result)
            .with_context(|| format!("decoding `result` field from {path}"))?;
        Ok(typed)
    }

    /// Write-endpoint companion to [`post_envelope`]: still validates
    /// the envelope's `code == "ok"` invariant, but accepts the
    /// resultless `{code, message}` body that the create/drop verbs
    /// return.
    async fn post_envelope_void(
        &self,
        path: &str,
        body: Value,
        progress: ProgressHandle,
    ) -> Result<()> {
        let _ = self.send_envelope(path, body, progress).await?;
        Ok(())
    }

    /// Shared HTTP + envelope decode + `code == "ok"` validation. The
    /// caller decides whether to require `result` or not.
    async fn send_envelope(
        &self,
        path: &str,
        body: Value,
        progress: ProgressHandle,
    ) -> Result<(reqwest::StatusCode, Envelope<Value>)> {
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
        let env: Envelope<Value> = resp
            .json()
            .await
            .with_context(|| format!("decoding response from {url}"))?;
        // `"ok"` = synchronous success (read endpoints). `"accept"` =
        // HTTP 202, async operation queued (most write endpoints —
        // index drops complete in the background after the API
        // returns). Anything else is an error.
        if env.code != "ok" && env.code != "accept" {
            return Err(ApiError::Status {
                status: status.as_u16(),
                body: format!("Data Storage API returned code='{}', message='{}'", env.code, env.message),
                env: None,
            }.into());
        }
        Ok((status, env))
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
        assert_eq!(e.result, Some(vec!["a".to_string(), "b".to_string()]));
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
        let result = e.result.expect("envelope should carry result");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "vendors");
        // Everything besides `name` lands in `extra`.
        assert!(result[0].extra.contains_key("info"));
        assert!(result[0].extra.contains_key("idIndex"));
    }

    #[test]
    fn envelope_decodes_write_response_without_result_field() {
        // create_index / drop_index / search_indexes/* return only
        // `{code, message}` — no `result`. Envelope must accept it.
        let raw = r#"{"code":"ok","message":"Index created."}"#;
        let e: Envelope<Value> = serde_json::from_str(raw).unwrap();
        assert_eq!(e.code, "ok");
        assert!(e.result.is_none(), "write responses have no result");
    }
}
