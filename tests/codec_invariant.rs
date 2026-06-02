//! Invariant tests for the KindCodec trait.
//!
//! These tests verify that the codec produces consistent hashes and stable
//! on-disk representations, and that redaction works as specified.

use rdc::snapshot::codec::codec;
use serde_json::json;

fn sample_engine() -> serde_json::Value {
    json!({
        "id": 401,
        "url": "https://x/api/v1/engines/401",
        "name": "E",
        "type": "extractor",
        "agenda_id": "tnt_live_123",
        "modified_at": "2026-04-20T08:00:00Z"
    })
}

fn sample_hook() -> serde_json::Value {
    json!({
        "id": 200,
        "url": "https://x/api/v1/hooks/200",
        "name": "My Function Hook",
        "type": "function",
        "queues": [],
        "events": ["annotation_content"],
        "status": "ready",
        "config": {
            "runtime": "python3.12",
            "code": "def hook(payload, settings):\n    return {}\n"
        },
        "modified_at": "2026-05-10T09:00:00Z"
    })
}

fn sample_queue() -> serde_json::Value {
    json!({
        "id": 300,
        "url": "https://x/api/v1/queues/300",
        "name": "Invoices",
        "workspace": "https://x/api/v1/workspaces/5",
        "schema": "https://x/api/v1/schemas/9",
        "counts": {
            "to_review": 7,
            "importing": 2,
            "exported": 50
        },
        "automation_level": "never",
        "modified_at": "2026-05-15T10:30:00Z"
    })
}

fn sample_label() -> serde_json::Value {
    json!({
        "id": 10,
        "url": "https://x/api/v1/labels/10",
        "name": "Approved",
        "modified_at": "2026-05-01T08:00:00Z"
    })
}

fn sample_rule() -> serde_json::Value {
    json!({
        "id": 20,
        "url": "https://x/api/v1/rules/20",
        "name": "Validate Totals",
        "queues": [],
        "trigger_condition": "annotation_content.total > 0\n",
        "modified_at": "2026-05-02T08:00:00Z"
    })
}

fn sample_schema() -> serde_json::Value {
    json!({
        "id": 30,
        "url": "https://x/api/v1/schemas/30",
        "name": "Test Schema",
        "queues": [],
        "content": [
            {
                "category": "section",
                "id": "header",
                "label": "Header",
                "children": [
                    { "category": "datapoint", "id": "invoice_id", "type": "string" },
                    {
                        "category": "datapoint",
                        "id": "amount_total",
                        "type": "number",
                        "formula": "amount_due + amount_tax"
                    }
                ]
            }
        ],
        "modified_at": "2026-05-03T08:00:00Z"
    })
}

fn sample_inbox() -> serde_json::Value {
    json!({
        "id": 40,
        "url": "https://x/api/v1/inboxes/40",
        "name": "AP Invoices Inbox",
        "queues": ["https://x/api/v1/queues/300"],
        "email": "ap-invoices@example.rossum.app",
        "email_prefix": "ap-invoices",
        "filters": [],
        "modified_at": "2026-05-04T08:00:00Z"
    })
}

fn sample_workspace() -> serde_json::Value {
    // Includes a nested `modified_at` to exercise recursive stripping.
    json!({
        "id": 700852,
        "url": "https://x/api/v1/workspaces/700852",
        "name": "Invoices AP",
        "organization": "https://x/api/v1/organizations/285704",
        "queues": ["https://x/api/v1/queues/2137275"],
        "modified_at": "2026-03-15T11:00:00Z",
        "metadata": {
            "tag": "ap",
            "modified_at": "2026-03-15T11:00:00Z"
        }
    })
}

fn sample_engine_field() -> serde_json::Value {
    json!({
        "id": 501,
        "url": "https://x/api/v1/engine_fields/501",
        "name": "Invoice ID",
        "engine": "https://x/api/v1/engines/401",
        "field_type": "string",
        "modified_at": "2026-04-15T08:00:00Z"
    })
}

fn sample_workflow() -> serde_json::Value {
    json!({
        "id": 700,
        "url": "https://x/api/v1/workflows/700",
        "name": "AP Invoice Flow",
        "organization": "https://x/api/v1/organizations/285704",
        "modified_at": "2026-04-01T10:00:00Z"
    })
}

fn sample_workflow_step() -> serde_json::Value {
    json!({
        "id": 42,
        "url": "https://x/api/v1/workflow_steps/42",
        "name": "Manager Approval",
        "workflow": "https://x/api/v1/workflows/700",
        "step_type": "approval",
        "modified_at": "2026-04-20T08:00:00Z"
    })
}

/// Run the consistency + idempotency invariants against every registered codec
/// that can be exercised with the sample values defined above.
fn codec_cases() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("engine_fields", sample_engine_field()),
        ("engines", sample_engine()),
        ("hooks", sample_hook()),
        ("inboxes", sample_inbox()),
        ("labels", sample_label()),
        ("queues", sample_queue()),
        ("rules", sample_rule()),
        ("schemas", sample_schema()),
        ("workflow_steps", sample_workflow_step()),
        ("workflows", sample_workflow()),
        ("workspaces", sample_workspace()),
    ]
}

/// Splice sidecar content back into a reparsed JSON value so that
/// `base_hash(spliced)` matches `base_hash(original)`.
///
/// - `code` sidecar (hooks): spliced into `config.code`.
/// - `trigger_condition` sidecar (rules): spliced as a top-level field.
/// - `formulas/<id>.py` sidecars (schemas): spliced into matching
///   datapoint `formula` fields by recursively walking `content`.
fn splice_sidecars_for_rehash(value: &mut serde_json::Value, sidecars: &[(String, Vec<u8>)]) {
    for (label, bytes) in sidecars {
        let Ok(text) = std::str::from_utf8(bytes) else {
            continue;
        };
        if label == "code" {
            // Hook: splice into config.code.
            if let Some(config) = value.get_mut("config").and_then(|c| c.as_object_mut()) {
                config.insert(
                    "code".to_string(),
                    serde_json::Value::String(text.to_string()),
                );
            }
        } else if label == "trigger_condition" {
            // Rule: splice as top-level field.
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "trigger_condition".to_string(),
                    serde_json::Value::String(text.to_string()),
                );
            }
        } else if let Some(field_id) = label
            .strip_prefix("formulas/")
            .and_then(|s| s.strip_suffix(".py"))
        {
            // Schema formula: recursively splice into the matching datapoint.
            splice_formula_into_content(value, field_id, text);
        }
    }
}

/// Recursively walk a schema `content`/`children` tree and insert `formula`
/// into the datapoint whose `id` matches `field_id`. Mirrors
/// `extract_formulas` in `src/snapshot/schema.rs` but in reverse.
fn splice_formula_into_content(node: &mut serde_json::Value, field_id: &str, formula: &str) {
    // Walk a top-level JSON object that has a `content` array.
    if let Some(content) = node.get_mut("content").and_then(|c| c.as_array_mut()) {
        for child in content.iter_mut() {
            splice_formula_into_node(child, field_id, formula);
        }
    }
}

fn splice_formula_into_node(node: &mut serde_json::Value, field_id: &str, formula: &str) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    let category = obj
        .get("category")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let id = obj
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();

    if category == "datapoint" && id == field_id {
        obj.insert(
            "formula".to_string(),
            serde_json::Value::String(formula.to_string()),
        );
        return;
    }

    // Recurse into children (array for sections/tuples, object for multivalues).
    match obj.get_mut("children") {
        Some(serde_json::Value::Array(children)) => {
            for child in children.iter_mut() {
                splice_formula_into_node(child, field_id, formula);
            }
        }
        Some(child @ serde_json::Value::Object(_)) => {
            splice_formula_into_node(child, field_id, formula);
        }
        _ => {}
    }
}

/// base_hash(v) must equal base_hash(parse(disk_bytes(v).json)).
/// This ensures the lockfile hash and the on-disk bytes are consistent:
/// re-reading what we wrote produces the same hash.
///
/// For kinds with sidecars (e.g. hooks, rules, schemas), the on-disk artifact
/// is NOT self-contained in the JSON alone — the sidecar content is also part
/// of the canonical representation. We simulate a full disk read by splicing
/// sidecar content back into the value before the second `base_hash` call.
#[test]
fn hash_consistent_with_disk_bytes() {
    for (kind, v) in codec_cases() {
        let c = codec(kind).unwrap_or_else(|| panic!("{kind} codec must be registered"));

        let hash1 = c
            .base_hash(&v)
            .unwrap_or_else(|e| panic!("{kind}: base_hash on original: {e}"));

        let art = c
            .disk_bytes(&v)
            .unwrap_or_else(|e| panic!("{kind}: disk_bytes on original: {e}"));
        let mut reparsed: serde_json::Value = serde_json::from_slice(&art.json)
            .unwrap_or_else(|e| panic!("{kind}: disk json must be valid JSON: {e}"));

        // Splice sidecar content back into the reparsed value so that
        // base_hash(reparsed) matches base_hash(original). Each sidecar kind
        // has its own splice convention:
        //   - hooks:   config.code  (label "code")
        //   - rules:   trigger_condition  (label "trigger_condition")
        //   - schemas: content[*].formula  (labels "formulas/<id>.py")
        splice_sidecars_for_rehash(&mut reparsed, &art.sidecars);

        let hash2 = c
            .base_hash(&reparsed)
            .unwrap_or_else(|e| panic!("{kind}: base_hash on reparsed: {e}"));

        assert_eq!(
            hash1, hash2,
            "{kind}: base_hash must be the same whether computed from the raw API body or the reparsed on-disk bytes (with sidecars spliced back)"
        );
    }
}

/// disk_bytes must be idempotent: serializing the reparsed output again
/// must yield the same bytes.
#[test]
fn disk_bytes_idempotent() {
    for (kind, v) in codec_cases() {
        let c = codec(kind).unwrap_or_else(|| panic!("{kind} codec must be registered"));

        let art1 = c
            .disk_bytes(&v)
            .unwrap_or_else(|e| panic!("{kind}: disk_bytes pass 1: {e}"));
        let reparsed: serde_json::Value = serde_json::from_slice(&art1.json)
            .unwrap_or_else(|e| panic!("{kind}: disk json must parse: {e}"));
        let art2 = c
            .disk_bytes(&reparsed)
            .unwrap_or_else(|e| panic!("{kind}: disk_bytes pass 2: {e}"));

        assert_eq!(
            art1.json, art2.json,
            "{kind}: disk_bytes must produce identical bytes when applied twice"
        );
    }
}

/// Redaction invariants for engines:
/// - `agenda_id` must NOT appear verbatim (it's redacted to the sentinel).
/// - the sentinel string MUST be present.
/// - `modified_at` must NOT appear (it's stripped by strip_hidden_fields_recursive).
#[test]
fn engines_redaction_correct() {
    let c = codec("engines").expect("engines codec must be registered");
    let v = sample_engine();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("tnt_live_123"),
        "agenda_id value must not appear verbatim on disk; got:\n{disk_str}"
    );
    assert!(
        disk_str.contains("refreshed live in Rossum"),
        "redaction sentinel must be present on disk; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from on-disk bytes; got:\n{disk_str}"
    );
}

/// Redaction invariants for hooks:
/// - `status` must be replaced by the sentinel.
/// - `modified_at` must be stripped.
/// - code must be extracted into a sidecar, not left in the JSON.
#[test]
fn hooks_redaction_correct() {
    let c = codec("hooks").expect("hooks codec must be registered");
    let v = sample_hook();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("\"ready\""),
        "hook status 'ready' must not appear verbatim; got:\n{disk_str}"
    );
    assert!(
        disk_str.contains("refreshed live in Rossum"),
        "redaction sentinel must be present for hook status; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from hook disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("def hook"),
        "code must be extracted into sidecar, not left in JSON; got:\n{disk_str}"
    );
    assert_eq!(art.sidecars.len(), 1, "exactly one 'code' sidecar expected");
    assert_eq!(art.sidecars[0].0, "code", "sidecar label must be 'code'");
}

/// Redaction invariants for queues:
/// - `counts` must be replaced by the sentinel.
/// - `modified_at` must be stripped.
/// - no sidecars.
#[test]
fn queues_redaction_correct() {
    let c = codec("queues").expect("queues codec must be registered");
    let v = sample_queue();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("to_review"),
        "raw counts fields must not appear on disk; got:\n{disk_str}"
    );
    assert!(
        disk_str.contains("refreshed live in Rossum"),
        "redaction sentinel must be present for queue counts; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from queue disk bytes; got:\n{disk_str}"
    );
    assert!(art.sidecars.is_empty(), "queues must produce no sidecars");
}

/// Labels: no redaction, no sidecars, modified_at stripped.
#[test]
fn labels_invariants() {
    let c = codec("labels").expect("labels codec must be registered");
    let v = sample_label();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from label disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("refreshed live in Rossum"),
        "labels must not have any redaction sentinel; got:\n{disk_str}"
    );
    assert!(art.sidecars.is_empty(), "labels must produce no sidecars");
}

/// Rules: trigger_condition extracted to a single "trigger_condition" sidecar.
#[test]
fn rules_sidecar_invariants() {
    let c = codec("rules").expect("rules codec must be registered");
    let v = sample_rule();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("trigger_condition"),
        "trigger_condition must be extracted to sidecar; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from rule disk bytes; got:\n{disk_str}"
    );
    assert_eq!(
        art.sidecars.len(),
        1,
        "expected exactly one sidecar for a rule with trigger_condition"
    );
    assert_eq!(
        art.sidecars[0].0, "trigger_condition",
        "sidecar label must be 'trigger_condition'"
    );
}

/// Schemas: formula fields extracted into `formulas/<field_id>.py` sidecars.
/// modified_at stripped, no redaction.
#[test]
fn schemas_sidecar_invariants() {
    let c = codec("schemas").expect("schemas codec must be registered");
    let v = sample_schema();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("amount_due + amount_tax"),
        "formula must be extracted to sidecar; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from schema disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("refreshed live in Rossum"),
        "schemas must not have any redaction sentinel; got:\n{disk_str}"
    );
    assert_eq!(
        art.sidecars.len(),
        1,
        "expected exactly one formula sidecar"
    );
    assert_eq!(
        art.sidecars[0].0, "formulas/amount_total.py",
        "sidecar label must be 'formulas/<field_id>.py'"
    );
}

/// Workspaces: no redaction, no sidecars, modified_at stripped recursively
/// (including nested modified_at inside sub-objects).
#[test]
fn workspaces_invariants() {
    let c = codec("workspaces").expect("workspaces codec must be registered");
    let v = sample_workspace();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("modified_at"),
        "modified_at (including nested) must be stripped from workspace disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("refreshed live in Rossum"),
        "workspaces must not have any redaction sentinel; got:\n{disk_str}"
    );
    assert!(
        art.sidecars.is_empty(),
        "workspaces must produce no sidecars"
    );
}

/// Engine fields: no redaction, no sidecars, modified_at stripped.
/// `name` kept by create_body but stripped by cross_env_body.
#[test]
fn engine_fields_invariants() {
    let c = codec("engine_fields").expect("engine_fields codec must be registered");
    let v = sample_engine_field();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from engine_fields disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("refreshed live in Rossum"),
        "engine_fields must not have any redaction sentinel; got:\n{disk_str}"
    );
    assert!(
        art.sidecars.is_empty(),
        "engine_fields must produce no sidecars"
    );

    // create_body keeps name
    let mut body = v.clone();
    c.create_body(&mut body);
    assert!(
        body.get("name").is_some(),
        "create_body must keep name for engine_fields"
    );

    // cross_env_body strips name
    let mut body = v.clone();
    c.cross_env_body(&mut body);
    assert!(
        body.get("name").is_none(),
        "cross_env_body must strip the immutable name for engine_fields"
    );
}

/// Workflows: no redaction, no sidecars, modified_at stripped.
#[test]
fn workflows_invariants() {
    let c = codec("workflows").expect("workflows codec must be registered");
    let v = sample_workflow();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from workflow disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("refreshed live in Rossum"),
        "workflows must not have any redaction sentinel; got:\n{disk_str}"
    );
    assert!(
        art.sidecars.is_empty(),
        "workflows must produce no sidecars"
    );
}

/// Workflow steps: no redaction, no sidecars, modified_at stripped.
#[test]
fn workflow_steps_invariants() {
    let c = codec("workflow_steps").expect("workflow_steps codec must be registered");
    let v = sample_workflow_step();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from workflow_steps disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("refreshed live in Rossum"),
        "workflow_steps must not have any redaction sentinel; got:\n{disk_str}"
    );
    assert!(
        art.sidecars.is_empty(),
        "workflow_steps must produce no sidecars"
    );
}

/// Inboxes: no redaction, no sidecars, modified_at stripped,
/// email stripped by create_body.
#[test]
fn inboxes_invariants() {
    let c = codec("inboxes").expect("inboxes codec must be registered");
    let v = sample_inbox();

    let art = c.disk_bytes(&v).expect("disk_bytes");
    let disk_str = std::str::from_utf8(&art.json).expect("disk bytes must be UTF-8");

    assert!(
        !disk_str.contains("modified_at"),
        "modified_at must be stripped from inbox disk bytes; got:\n{disk_str}"
    );
    assert!(
        !disk_str.contains("refreshed live in Rossum"),
        "inboxes must not have any redaction sentinel; got:\n{disk_str}"
    );
    assert!(art.sidecars.is_empty(), "inboxes must produce no sidecars");

    // Verify create_body strips the email field.
    let mut body = v.clone();
    c.create_body(&mut body);
    assert!(
        body.get("email").is_none(),
        "create_body must strip the server-assigned email field"
    );
}
