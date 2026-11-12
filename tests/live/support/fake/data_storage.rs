//! An in-memory Data Storage (`<host>/svc/data-storage/api/v1/...`) for the
//! fake org. Opt-in: [`super::FakeOrg::enable_data_storage`]. Without it every
//! Data Storage path stays a 404, as on a cluster with no MDH.
//!
//! It models what the sync's MDH path and the Data Storage state store send,
//! and nothing more: collections (`list`, `create`), regular indexes (`list`,
//! `create`), search-index listing, and the synchronous row verbs `find`,
//! `insert_many`, `replace_one`, `delete_many` and `aggregate` with a single
//! `$count` stage. A request outside that answers 400, so a test that strays
//! fails loudly instead of meeting a guess.
//!
//! Filters (`find`, `replace_one`, `delete_many`) are top-level field
//! conditions: plain equality, `$eq`, `$in`, and `$regex` of the one form the
//! state store sends, `^` followed by an escaped literal (a prefix match).

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use wiremock::{Request, ResponseTemplate};

use super::{body_of, json_response, TOKEN};

/// The largest document `replace_one` stores (MongoDB's BSON cap).
const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Where Data Storage lives on the fake's host.
pub const PREFIX: &str = "/svc/data-storage/api/v1/";

/// One collection: its documents in insert order, and its regular indexes
/// (the implicit `_id_` is added on listing).
#[derive(Default, Debug, Clone)]
pub struct Collection {
    pub docs: Vec<Value>,
    pub indexes: Vec<Value>,
}

#[derive(Default, Debug)]
pub struct DataStorage {
    pub collections: BTreeMap<String, Collection>,
    /// Feeds generated ObjectIds.
    next_oid: u64,
    /// `data/find` requests answered, for tests that count downloads.
    pub finds: usize,
}

impl DataStorage {
    /// The documents of `collection`, empty when it does not exist.
    pub fn docs(&self, collection: &str) -> Vec<Value> {
        self.collections.get(collection).map(|c| c.docs.clone()).unwrap_or_default()
    }

    /// The document of `collection` whose `_id` is `id`.
    pub fn doc(&self, collection: &str, id: &Value) -> Option<Value> {
        self.collections.get(collection)?.docs.iter().find(|d| d.get("_id") == Some(id)).cloned()
    }

    fn oid(&mut self) -> Value {
        self.next_oid += 1;
        json!({ "$oid": format!("{:024x}", self.next_oid) })
    }
}

/// The answer to `req` when it is a Data Storage request and Data Storage is
/// enabled; `None` otherwise, so the org router answers (a 404).
pub fn serve(ds: Option<&mut DataStorage>, req: &Request) -> Option<ResponseTemplate> {
    let path = req.url.path();
    let verb = path.strip_prefix(PREFIX)?;
    let ds = ds?;
    if req.method.as_str() != "POST" {
        return Some(error(405, "method not allowed"));
    }
    let bearer = req.headers.get("authorization").and_then(|v| v.to_str().ok());
    if bearer != Some(&format!("Bearer {TOKEN}")) {
        return Some(error(401, "not authenticated"));
    }
    let body = body_of(req);
    Some(match route(ds, verb, &body) {
        Ok(Some(result)) => json_response(200, &json!({ "code": "ok", "message": "", "result": result })),
        Ok(None) => json_response(200, &json!({ "code": "ok", "message": "" })),
        Err((status, message)) => error(status, &message),
    })
}

fn error(status: u16, message: &str) -> ResponseTemplate {
    json_response(status, &json!({ "code": "error", "message": message }))
}

type Answer = Result<Option<Value>, (u16, String)>;

fn bad(message: impl Into<String>) -> (u16, String) {
    (400, message.into())
}

fn route(ds: &mut DataStorage, verb: &str, body: &Value) -> Answer {
    let name = || {
        body.get("collectionName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| (422, "collectionName is required".to_string()))
    };
    match verb {
        "collections/list" => {
            let listed: Vec<Value> = ds
                .collections
                .keys()
                .map(|n| {
                    json!({
                        "name": n,
                        "type": "collection",
                        "options": {},
                        "info": { "readOnly": false },
                        "idIndex": { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
                    })
                })
                .collect();
            Ok(Some(Value::Array(listed)))
        }
        "collections/create" => {
            let name = name()?;
            if ds.collections.contains_key(&name) {
                return Err(bad(format!("Collection {name} already exists.")));
            }
            ds.collections.insert(name, Collection::default());
            Ok(None)
        }
        "indexes/list" => {
            let Some(c) = ds.collections.get(&name()?) else {
                return Ok(Some(json!([])));
            };
            let mut listed = vec![json!({ "v": 2, "key": { "_id": 1 }, "name": "_id_" })];
            listed.extend(c.indexes.iter().cloned());
            Ok(Some(Value::Array(listed)))
        }
        "indexes/create" => {
            let name = name()?;
            let index = body.get("indexName").and_then(Value::as_str).ok_or_else(|| bad("indexName is required"))?;
            let keys = body.get("keys").cloned().ok_or_else(|| bad("keys is required"))?;
            let c = ds.collections.entry(name).or_default();
            if c.indexes.iter().any(|ix| ix["name"] == index) {
                return Err(bad(format!("Index {index} already exists.")));
            }
            let mut def = Map::new();
            def.insert("v".into(), json!(2));
            def.insert("key".into(), keys);
            def.insert("name".into(), json!(index));
            if let Some(Value::Object(options)) = body.get("options") {
                for (k, v) in options {
                    def.insert(k.clone(), v.clone());
                }
            }
            c.indexes.push(Value::Object(def));
            Ok(None)
        }
        "search_indexes/list" => match ds.collections.get(&name()?) {
            Some(_) => Ok(Some(json!([]))),
            None => Err((404, "Dataset not found".to_string())),
        },
        "data/find" => {
            ds.finds += 1;
            let query = body.get("query").ok_or_else(|| bad("query is required"))?;
            check(query)?;
            let projection = body.get("projection").filter(|p| !p.is_null());
            let mut out = Vec::new();
            for doc in ds.collections.get(&name()?).map(|c| c.docs.as_slice()).unwrap_or_default() {
                if matches(doc, query)? {
                    out.push(match projection {
                        Some(p) => project(doc, p)?,
                        None => doc.clone(),
                    });
                }
            }
            Ok(Some(Value::Array(out)))
        }
        "data/insert_many" => {
            let name = name()?;
            let docs = body.get("documents").and_then(Value::as_array).ok_or_else(|| bad("documents is required"))?;
            let mut prepared = Vec::new();
            for doc in docs {
                let mut doc = doc.as_object().cloned().ok_or_else(|| bad("a document is not an object"))?;
                if !doc.contains_key("_id") {
                    doc.insert("_id".into(), ds.oid());
                }
                prepared.push(Value::Object(doc));
            }
            // `ordered: false`: every non-conflicting document goes in, and
            // one duplicate `_id` still fails the call as a whole.
            let c = ds.collections.entry(name).or_default();
            let mut ids = Vec::new();
            let mut failed = false;
            for doc in prepared {
                if c.docs.iter().any(|d| d.get("_id") == doc.get("_id")) {
                    failed = true;
                    continue;
                }
                ids.push(doc["_id"].clone());
                c.docs.push(doc);
            }
            if failed {
                return Err(bad("batch op errors occurred"));
            }
            Ok(Some(json!({ "inserted_ids": ids })))
        }
        "data/replace_one" => {
            let name = name()?;
            let filter = body.get("filter").ok_or_else(|| bad("filter is required"))?;
            check(filter)?;
            let replacement = body.get("replacement").and_then(Value::as_object).ok_or_else(|| bad("replacement is required"))?;
            let upsert = body.pointer("/options/upsert").and_then(Value::as_bool).unwrap_or(false);
            // Quirk `data_storage_document_is_capped_at_16_mib`: the real
            // cap is on the BSON document; its JSON length is close enough
            // for the sizes a probe sends.
            if serde_json::to_vec(replacement).map_or(0, |b| b.len()) > MAX_DOCUMENT_BYTES {
                return Err(bad(format!("Resulting document after update is larger than {MAX_DOCUMENT_BYTES}")));
            }
            let docs = ds.collections.get(&name).map(|c| c.docs.as_slice()).unwrap_or_default();
            let mut hit = None;
            for (i, d) in docs.iter().enumerate() {
                if matches(d, filter)? {
                    hit = Some(i);
                    break;
                }
            }
            if let Some(i) = hit {
                let c = ds.collections.get_mut(&name).expect("the collection holds the match");
                let id = c.docs[i]["_id"].clone();
                if replacement.get("_id").is_some_and(|r| *r != id) {
                    return Err(bad("the (immutable) field '_id' was found to have been altered"));
                }
                let mut doc = Map::new();
                doc.insert("_id".into(), id);
                for (k, v) in replacement {
                    if k != "_id" {
                        doc.insert(k.clone(), v.clone());
                    }
                }
                let doc = Value::Object(doc);
                let modified = usize::from(c.docs[i] != doc);
                c.docs[i] = doc;
                return Ok(Some(update_result(1, modified, Value::Null)));
            }
            if !upsert {
                return Ok(Some(update_result(0, 0, Value::Null)));
            }
            // A replacement upsert takes its `_id` from the replacement, else
            // from an equality on `_id` in the filter, else a new ObjectId.
            let id = match (replacement.get("_id"), filter.get("_id")) {
                (Some(id), _) => id.clone(),
                (None, Some(id)) if !id.as_object().is_some_and(|o| o.keys().any(|k| k.starts_with('$'))) => id.clone(),
                _ => ds.oid(),
            };
            let mut doc = Map::new();
            doc.insert("_id".into(), id.clone());
            for (k, v) in replacement {
                if k != "_id" {
                    doc.insert(k.clone(), v.clone());
                }
            }
            ds.collections.entry(name).or_default().docs.push(Value::Object(doc));
            Ok(Some(update_result(0, 0, id)))
        }
        "data/delete_many" => {
            let name = name()?;
            let filter = body.get("filter").ok_or_else(|| bad("filter is required"))?;
            check(filter)?;
            let Some(c) = ds.collections.get_mut(&name) else {
                return Ok(Some(json!({ "deleted_count": 0 })));
            };
            let mut kept = Vec::new();
            let mut deleted = 0;
            for d in std::mem::take(&mut c.docs) {
                if matches(&d, filter)? {
                    deleted += 1;
                } else {
                    kept.push(d);
                }
            }
            c.docs = kept;
            Ok(Some(json!({ "deleted_count": deleted })))
        }
        "data/aggregate" => {
            let pipeline = body.get("pipeline").and_then(Value::as_array).ok_or_else(|| bad("pipeline is required"))?;
            let [stage] = pipeline.as_slice() else {
                return Err(bad("the fake runs only a single `$count` stage"));
            };
            let Some(field) = stage.get("$count").and_then(Value::as_str) else {
                return Err(bad("the fake runs only a single `$count` stage"));
            };
            let n = ds.collections.get(&name()?).map_or(0, |c| c.docs.len());
            Ok(Some(if n == 0 { json!([]) } else { json!([{ field: n }]) }))
        }
        other => Err((404, format!("the fake has no Data Storage verb {other}"))),
    }
}

fn update_result(matched: usize, modified: usize, upserted: Value) -> Value {
    json!({ "raw_result": {}, "matched_count": matched, "modified_count": modified, "upserted_id": upserted })
}

/// Whether `doc` meets every top-level condition of `filter`.
fn matches(doc: &Value, filter: &Value) -> Result<bool, (u16, String)> {
    eval(doc, filter)
}

/// Refuse `filter` if the fake cannot run it, even with no document to test.
fn check(filter: &Value) -> Result<(), (u16, String)> {
    let probe = json!({ "_id": "" });
    eval(&probe, filter).map(|_| ())
}

fn eval(doc: &Value, filter: &Value) -> Result<bool, (u16, String)> {
    let filter = filter.as_object().ok_or_else(|| bad("a filter is an object"))?;
    let mut all = true;
    for (field, cond) in filter {
        if field.starts_with('$') {
            return Err(bad(format!("the fake does not run {field}")));
        }
        let value = doc.get(field);
        let ops = cond.as_object().filter(|o| o.keys().next().is_some_and(|k| k.starts_with('$')));
        let Some(ops) = ops else {
            all &= value == Some(cond);
            continue;
        };
        for (op, arg) in ops {
            let ok = match op.as_str() {
                "$eq" => value == Some(arg),
                "$in" => {
                    let set = arg.as_array().ok_or_else(|| bad("$in takes an array"))?;
                    value.is_some_and(|v| set.contains(v))
                }
                "$regex" => {
                    let prefix = regex_prefix(arg.as_str().ok_or_else(|| bad("$regex takes a string"))?)?;
                    value.and_then(Value::as_str).is_some_and(|v| v.starts_with(&prefix))
                }
                other => return Err(bad(format!("the fake does not run {other}"))),
            };
            all &= ok;
        }
    }
    Ok(all)
}

/// The literal of a `^<escaped literal>` pattern; any other pattern is refused.
fn regex_prefix(pattern: &str) -> Result<String, (u16, String)> {
    let refuse = || bad(format!("the fake runs only `^<escaped literal>` patterns, not {pattern:?}"));
    let rest = pattern.strip_prefix('^').ok_or_else(refuse)?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().ok_or_else(refuse)?),
            '.' | '^' | '$' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}' => return Err(refuse()),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// `doc` through an inclusion (`1`) or exclusion (`0`) projection.
fn project(doc: &Value, projection: &Value) -> Result<Value, (u16, String)> {
    let spec = projection.as_object().ok_or_else(|| bad("a projection is an object"))?;
    let on = |v: &Value| v.as_i64() == Some(1) || v.as_bool() == Some(true);
    let src = doc.as_object().cloned().unwrap_or_default();
    let including = spec.iter().any(|(k, v)| k != "_id" && on(v)) || spec.get("_id").is_some_and(on);
    let mut out = Map::new();
    for (k, v) in src {
        let keep = match spec.get(&k) {
            Some(s) => on(s),
            None => !including || (k == "_id"),
        };
        if keep {
            out.insert(k, v);
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::super::FakeOrg;
    use rdc::api::DataStorageClient;
    use serde_json::json;

    fn client(fake: &FakeOrg) -> DataStorageClient {
        let base = rdc::config::EnvConfig { api_base: fake.api_base(), org_id: 1 }.data_storage_base();
        DataStorageClient::new(base, "fake-token".into()).unwrap()
    }

    #[tokio::test]
    async fn disabled_by_default() {
        let fake = FakeOrg::start().await;
        let err = client(&fake).list_collections(None).await.unwrap_err();
        assert!(rdc::api::anyhow_has_status(&err, 404), "{err:#}");
    }

    #[tokio::test]
    async fn find_insert_replace_delete() {
        let fake = FakeOrg::start().await;
        fake.enable_data_storage();
        let c = client(&fake);
        c.insert_many("s", &[json!({ "_id": "a/1", "t": "x" }), json!({ "_id": "b/1", "t": "y" })], None).await.unwrap();
        let err = c.insert_many("s", &[json!({ "_id": "a/1" }), json!({ "_id": "c/1" })], None).await.unwrap_err();
        assert!(rdc::api::anyhow_has_status(&err, 400) && format!("{err:#}").contains("batch op errors occurred"));
        assert_eq!(fake.state().data_storage.as_ref().unwrap().docs("s").len(), 3, "the non-conflicting one went in");

        let prefix = c.find("s", &json!({ "_id": { "$regex": "^a/" } }), Some(&json!({ "_id": 1 })), None).await.unwrap();
        assert_eq!(prefix, vec![json!({ "_id": "a/1" })]);
        let some = c.find("s", &json!({ "_id": { "$in": ["b/1", "c/1"] } }), None, None).await.unwrap();
        assert_eq!(some.len(), 2);

        let cas = c.replace_where("s", &json!({ "_id": "a/1", "t": "nope" }), &json!({ "t": "z" }), false, None).await.unwrap();
        assert_eq!(cas.matched_count, 0);
        let cas = c.replace_where("s", &json!({ "_id": "a/1", "t": "x" }), &json!({ "t": "z" }), false, None).await.unwrap();
        assert_eq!((cas.matched_count, cas.modified_count), (1, 1));
        let up = c.replace_where("s", &json!({ "_id": "d/1" }), &json!({ "t": "w" }), true, None).await.unwrap();
        assert_eq!((up.matched_count, up.upserted_id), (0, Some(json!("d/1"))));
        assert_eq!(fake.state().data_storage.as_ref().unwrap().doc("s", &json!("d/1")), Some(json!({ "_id": "d/1", "t": "w" })));

        assert_eq!(c.delete_many_by_ids("s", &[json!("a/1"), json!("zz")], None).await.unwrap(), 1);
        assert_eq!(c.count_documents("s", None).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn an_unsupported_pattern_is_refused() {
        let fake = FakeOrg::start().await;
        fake.enable_data_storage();
        let err = client(&fake).find("s", &json!({ "_id": { "$regex": "a.*" } }), None, None).await.unwrap_err();
        assert!(rdc::api::anyhow_has_status(&err, 400), "{err:#}");
    }

    #[tokio::test]
    async fn collections_and_indexes() {
        let fake = FakeOrg::start().await;
        fake.enable_data_storage();
        let c = client(&fake);
        assert!(c.list_indexes("v", None).await.unwrap().is_empty(), "a missing collection lists no index");
        assert!(rdc::api::anyhow_has_status(&c.list_search_indexes("v", None).await.unwrap_err(), 404));
        c.create_index("v", "v_idx", &json!({ "a": 1 }), &json!({}), None).await.unwrap();
        let names: Vec<String> = c.list_collections(None).await.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["v"], "an index create brings its collection");
        assert_eq!(c.list_indexes("v", None).await.unwrap()[1], json!({ "v": 2, "key": { "a": 1 }, "name": "v_idx" }));
        c.create_collection("w", None).await.unwrap();
        assert!(c.create_collection("w", None).await.is_err());
    }
}
