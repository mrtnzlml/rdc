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

/// base_hash(v) must equal base_hash(parse(disk_bytes(v).json)).
/// This ensures the lockfile hash and the on-disk bytes are consistent:
/// re-reading what we wrote produces the same hash.
#[test]
fn hash_consistent_with_disk_bytes() {
    let c = codec("engines").expect("engines codec must be registered");
    let v = sample_engine();

    let hash1 = c.base_hash(&v).expect("base_hash on original");

    let art = c.disk_bytes(&v).expect("disk_bytes on original");
    let reparsed: serde_json::Value =
        serde_json::from_slice(&art.json).expect("disk json must be valid JSON");
    let hash2 = c.base_hash(&reparsed).expect("base_hash on reparsed");

    assert_eq!(hash1, hash2, "base_hash must be the same whether computed from the raw API body or the reparsed on-disk bytes");
}

/// disk_bytes must be idempotent: serializing the reparsed output again
/// must yield the same bytes.
#[test]
fn disk_bytes_idempotent() {
    let c = codec("engines").expect("engines codec must be registered");
    let v = sample_engine();

    let art1 = c.disk_bytes(&v).expect("disk_bytes pass 1");
    let reparsed: serde_json::Value =
        serde_json::from_slice(&art1.json).expect("disk json must parse");
    let art2 = c.disk_bytes(&reparsed).expect("disk_bytes pass 2");

    assert_eq!(
        art1.json, art2.json,
        "disk_bytes must produce identical bytes when applied twice"
    );
}

/// Redaction invariants for engines:
/// - `agenda_id` must NOT appear verbatim (it's redacted to the sentinel).
/// - the sentinel string MUST be present.
/// - `modified_at` must NOT appear (it's stripped by strip_hidden_fields_recursive).
#[test]
fn redaction_correct() {
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
