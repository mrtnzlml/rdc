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

/// Run the consistency + idempotency invariants against every registered codec
/// that can be exercised with the sample values defined above.
fn codec_cases() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("engines", sample_engine()),
        ("hooks", sample_hook()),
        ("queues", sample_queue()),
    ]
}

/// base_hash(v) must equal base_hash(parse(disk_bytes(v).json)).
/// This ensures the lockfile hash and the on-disk bytes are consistent:
/// re-reading what we wrote produces the same hash.
///
/// For kinds with sidecars (e.g. hooks), the on-disk artifact is NOT
/// self-contained in the JSON alone — the sidecar content (code) is also
/// part of the canonical representation. We simulate a full disk read by
/// splicing sidecar content back into `config.code` before the second
/// `base_hash` call, which is exactly what `read_hook_value` does on a real
/// filesystem read.
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

        // For kinds that extract code into a sidecar (hooks), splice the sidecar
        // content back into `config.code` before re-hashing. This simulates a
        // real disk read, where `read_hook_value` reads both the JSON file and
        // the `.py`/`.js` sidecar and recombines them in memory.
        if let Some((label, bytes)) = art.sidecars.first()
            && label == "code"
            && let Ok(code_str) = std::str::from_utf8(bytes)
            && let Some(config) = reparsed.get_mut("config").and_then(|c| c.as_object_mut())
        {
            config.insert(
                "code".to_string(),
                serde_json::Value::String(code_str.to_string()),
            );
        }

        let hash2 = c
            .base_hash(&reparsed)
            .unwrap_or_else(|e| panic!("{kind}: base_hash on reparsed: {e}"));

        assert_eq!(
            hash1, hash2,
            "{kind}: base_hash must be the same whether computed from the raw API body or the reparsed on-disk bytes (with sidecar spliced back for hook kinds)"
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
