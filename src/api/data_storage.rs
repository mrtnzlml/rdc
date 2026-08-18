//! Rossum Data Storage (MDH) API client.
//!
//! The MDH API is RPC-style — every call is a `POST` to
//! `<base>/v1/<resource>/<verb>` with a JSON body, and every response is
//! wrapped in `{code, message, result}`. Collection CRUD
//! (`collections/create`, `collections/drop`, `collections/rename`) is
//! partially implemented: only `create` is needed. Row-data verbs ARE
//! implemented, but only the SYNCHRONOUS ones — `data/find`,
//! `data/insert_many`, `data/delete_many`, `data/replace_one`,
//! `data/aggregate` — and only for datasets the snapshot flags
//! `"data": "manual"`. `data/bulk_write` is deliberately absent: it answers
//! 202 with an EMPTY `message`, so it carries no operation id and its
//! completion cannot be observed. rdc never touches the rows of a dataset that
//! is not flagged manual.
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

/// `result` shape of `data/aggregate` with a `$count` stage. An EMPTY result
/// array is the documented answer for an empty collection, so the absence of a
/// row means zero — but a present row missing `n` is a contract break.
#[derive(Debug, Deserialize)]
struct CountRow {
    n: usize,
}

/// `result` shape of `data/delete_many`.
#[derive(Debug, Deserialize)]
struct DeleteResult {
    deleted_count: usize,
}

/// `result` shape of `data/replace_one`. A missing field here must be a decode
/// error, never a silent 0 — callers read `matched_count == 0` as "the row
/// vanished between our read and our write".
#[derive(Debug, Deserialize)]
struct ReplaceResult {
    matched_count: usize,
}

impl DataStorageClient {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = crate::api::build_http_client()?;
        Ok(Self { base_url, token, http })
    }

    /// `POST /v1/collections/list` with `{nameOnly: false}` returns full
    /// collection metadata (name, type, options, info, idIndex).
    pub async fn list_collections(&self, progress: ProgressHandle) -> Result<Vec<Collection>> {
        self.post_envelope("/v1/collections/list", json!({"nameOnly": false}), progress).await
    }

    /// `POST /v1/collections/create` with `{collectionName}` — create an EMPTY
    /// collection (no indexes beyond the implicit `_id_`, no documents). Used to
    /// materialize an index-less "data-only" collection on a target env: such a
    /// collection has no index schema for the normal create-via-indexes path
    /// ([`push_dataset`]) to act on, so without this it would never appear on the
    /// target. Response body is the resultless `{success: true}` envelope.
    pub async fn create_collection(
        &self,
        collection: &str,
        progress: ProgressHandle,
    ) -> Result<()> {
        self.post_envelope_void(
            "/v1/collections/create",
            json!({ "collectionName": collection }),
            progress,
        )
        .await
    }

    /// `POST /v1/data/aggregate` — run a read-only MongoDB aggregation
    /// pipeline against a collection and return the result documents.
    /// Used by the unique-index duplicate-key preflight: a `$group` over
    /// the index's key fields detects data that would make the async
    /// index build fail before any create is attempted.
    pub async fn aggregate(
        &self,
        collection: &str,
        pipeline: &Value,
        progress: ProgressHandle,
    ) -> Result<Vec<Value>> {
        self.post_envelope(
            "/v1/data/aggregate",
            json!({
                "collectionName": collection,
                "pipeline": pipeline,
            }),
            progress,
        )
        .await
    }

    /// `POST /v1/data/find` with an empty query — every document in the
    /// collection, in one call. Verified live: a single response carried 1206
    /// documents (1002 documents ≈ 442 KB in 0.35 s), so no paging is needed at
    /// the scale rdc versions. Row ORDER is not stable across calls; callers
    /// impose their own (see `snapshot::mdh_data::to_jsonl`).
    ///
    /// A missing collection returns `{"code":"ok","result":[]}` — NOT a 404 — so
    /// an empty result never implies the collection is gone. Existence comes
    /// from [`list_collections`].
    pub async fn find_all(
        &self,
        collection: &str,
        progress: ProgressHandle,
    ) -> Result<Vec<Value>> {
        self.post_envelope(
            "/v1/data/find",
            json!({ "collectionName": collection, "query": {} }),
            progress,
        )
        .await
    }

    /// Row count via `POST /v1/data/aggregate [{"$count": "n"}]` — one cheap
    /// call for the size guardrail (`collections/list` carries no count). An
    /// empty collection yields `result: []`, which reads as 0.
    pub async fn count_documents(
        &self,
        collection: &str,
        progress: ProgressHandle,
    ) -> Result<usize> {
        let rows: Vec<CountRow> = self
            .post_envelope(
                "/v1/data/aggregate",
                json!({ "collectionName": collection, "pipeline": [{ "$count": "n" }] }),
                progress,
            )
            .await?;
        Ok(rows.first().map_or(0, |r| r.n))
    }

    /// `POST /v1/data/insert_many`. `ordered: false` so one bad document does
    /// not mask the rest, `waitForFullWrite: true` so the same-cycle pull-back
    /// reads what we just wrote.
    ///
    /// A duplicate `_id` fails the call with HTTP 400 "batch op errors
    /// occurred" — and the NON-conflicting documents in the same batch are
    /// still inserted. Callers must therefore treat an error as PARTIALLY
    /// applied and re-read rather than assume a no-op.
    pub async fn insert_many(
        &self,
        collection: &str,
        documents: &[Value],
        progress: ProgressHandle,
    ) -> Result<()> {
        self.post_envelope_void(
            "/v1/data/insert_many",
            json!({
                "collectionName": collection,
                "documents": documents,
                "ordered": false,
                "waitForFullWrite": true,
            }),
            progress,
        )
        .await
    }

    /// `POST /v1/data/delete_many` filtered by raw `_id` values. The id list is
    /// intentionally mixed-type: one collection can hold both server-generated
    /// ObjectId wrappers and user-authored scalar ids, and `$in` accepts both in
    /// a single array. Returns `deleted_count`.
    pub async fn delete_many_by_ids(
        &self,
        collection: &str,
        ids: &[Value],
        progress: ProgressHandle,
    ) -> Result<usize> {
        let result: DeleteResult = self
            .post_envelope(
                "/v1/data/delete_many",
                json!({
                    "collectionName": collection,
                    "filter": { "_id": { "$in": ids } },
                    "waitForFullWrite": true,
                }),
                progress,
            )
            .await?;
        Ok(result.deleted_count)
    }

    /// `POST /v1/data/replace_one`, matching on `_id`. `replacement` MUST NOT
    /// contain `_id`: the API rejects a replacement that alters the immutable
    /// field ("the (immutable) field '_id' was found to have been altered"),
    /// and omitting it makes that error unreachable — the filter carries
    /// identity. No upsert: a no-match leaves the collection untouched and
    /// returns `matched_count: 0`, which the caller reads as "the row vanished
    /// between our read and our write".
    pub async fn replace_one(
        &self,
        collection: &str,
        id: &Value,
        replacement: &Value,
        progress: ProgressHandle,
    ) -> Result<usize> {
        let result: ReplaceResult = self
            .post_envelope(
                "/v1/data/replace_one",
                json!({
                    "collectionName": collection,
                    "filter": { "_id": id },
                    "replacement": replacement,
                    "waitForFullWrite": true,
                }),
                progress,
            )
            .await?;
        Ok(result.matched_count)
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

    /// `create_collection` must POST `/v1/collections/create` with exactly
    /// `{"collectionName": <name>}` and accept the `{success:true}` envelope.
    /// The `body_json` matcher makes the mock respond ONLY to that exact body,
    /// so a wrong endpoint/payload yields no match → the call errors → test fails.
    #[tokio::test]
    async fn create_collection_posts_expected_endpoint_and_payload() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/collections/create"))
            .and(body_json(json!({ "collectionName": "PO_LINE_DESC_SYNONYMS" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok",
                "message": "",
                "result": { "success": true }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        client
            .create_collection("PO_LINE_DESC_SYNONYMS", None)
            .await
            .expect("create_collection should succeed on an ok envelope");
        // `.expect(1)` on drop verifies exactly one matching request was made.
    }

    /// A non-ok envelope (e.g. permission error) must surface as an error, not
    /// be swallowed — the caller aborts the sync so the collection isn't
    /// silently missing.
    #[tokio::test]
    async fn create_collection_errors_on_non_ok_envelope() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/collections/create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "error",
                "message": "permission denied"
            })))
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        let err = client
            .create_collection("X", None)
            .await
            .expect_err("non-ok envelope must be an error");
        assert!(
            format!("{err:#}").contains("permission denied"),
            "error should carry the API message: {err:#}"
        );
    }

    /// `find_all` must POST `data/find` with an empty query and hand back every
    /// document — one call returns the whole collection (live-verified: 1206
    /// documents in a single response).
    #[tokio::test]
    async fn find_all_posts_empty_query_and_returns_every_document() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .and(body_json(json!({ "collectionName": "GL_CODES", "query": {} })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok",
                "message": "",
                "result": [
                    { "_id": { "$oid": "6a8403a6070b60eaa348d173" }, "code": "1000" },
                    { "_id": "gl-2000", "code": "2000" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        let rows = client.find_all("GL_CODES", None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["_id"], json!("gl-2000"));
    }

    /// `$count` returns `[{"n": N}]`, and `[]` for an empty collection — the
    /// empty case must read as 0, not as an error.
    #[tokio::test]
    async fn count_documents_handles_the_empty_collection_result() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "pipeline": [{ "$count": "n" }]
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "code": "ok", "message": "", "result": [] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        assert_eq!(client.count_documents("GL_CODES", None).await.unwrap(), 0);

        let server2 = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "pipeline": [{ "$count": "n" }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "code": "ok", "message": "", "result": [{ "n": 129 }] }),
            ))
            .expect(1)
            .mount(&server2)
            .await;
        let client2 = DataStorageClient::new(server2.uri(), "TOKEN".into()).unwrap();
        assert_eq!(client2.count_documents("GL_CODES", None).await.unwrap(), 129);
    }

    /// The insert body's exact shape is load-bearing: `ordered:false` +
    /// `waitForFullWrite:true` is the combination verified against the live API.
    #[tokio::test]
    async fn insert_many_posts_the_verified_body_shape() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/insert_many"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "documents": [{ "code": "1000" }],
                "ordered": false,
                "waitForFullWrite": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": { "inserted_ids": ["x"] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        client
            .insert_many("GL_CODES", &[json!({ "code": "1000" })], None)
            .await
            .unwrap();
    }

    /// Deletes target raw `_id` values via `$in`, and the id list is
    /// deliberately mixed-type (ObjectId wrappers and plain scalars coexist in
    /// one collection).
    #[tokio::test]
    async fn delete_many_by_ids_posts_mixed_type_in_filter_and_returns_count() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/delete_many"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "filter": { "_id": { "$in": [{ "$oid": "6a8403a6070b60eaa348d173" }, "gl-2000"] } },
                "waitForFullWrite": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": { "deleted_count": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        let n = client
            .delete_many_by_ids(
                "GL_CODES",
                &[json!({ "$oid": "6a8403a6070b60eaa348d173" }), json!("gl-2000")],
                None,
            )
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    /// The replacement must NOT carry `_id`: the live API rejects a replacement
    /// that alters the immutable field, and omitting it makes that unreachable.
    /// `matched_count` comes back so the caller can detect a vanished row.
    #[tokio::test]
    async fn replace_one_omits_id_from_the_replacement_and_returns_matched_count() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/replace_one"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "filter": { "_id": "gl-1000" },
                "replacement": { "code": "1000", "label": "new" },
                "waitForFullWrite": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "",
                "result": { "matched_count": 1, "modified_count": 1, "upserted_id": null }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        let matched = client
            .replace_one(
                "GL_CODES",
                &json!("gl-1000"),
                &json!({ "code": "1000", "label": "new" }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(matched, 1);
    }
}
