use crate::model::Schema;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Write a schema to `<queue_dir>/schema.json`, extracting any formula field
/// `formula` strings into `<queue_dir>/formulas/<field_id>.py` files.
/// Returns the JSON bytes written (for content_hash).
///
/// **Hash coverage gap:** The returned bytes are the post-extraction
/// `schema.json` content. Changes to extracted `formulas/*.py` files are NOT
/// reflected in the returned hash.
///
/// **Combined-hash algorithm for M7's three-way merge:** When implementing
/// drift detection, compute the canonical schema content hash as
/// `SHA-256(schema_json_bytes || 0x00 || formula_1_path || 0x00 ||
/// formula_1_bytes || 0x00 || formula_2_path || 0x00 || formula_2_bytes ||
/// ...)` where formulas are sorted by `field_id`. The 0x00 separator makes
/// boundaries unambiguous; sorting makes the hash deterministic across
/// platforms with non-stable filesystem listing order. Until M7, the
/// lockfile stores the simpler `schema.json`-only hash.
pub fn write_schema(queue_dir: &Path, schema: &Schema) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(schema)
        .context("serializing schema to value")?;

    // Walk the content tree, extracting formula strings.
    let mut formulas: Vec<(String, String)> = Vec::new();
    if let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
        for node in content.iter_mut() {
            extract_formulas(node, &mut formulas);
        }
    }

    let json_path = queue_dir.join("schema.json");
    let bytes = serde_json::to_vec_pretty(&value)
        .context("serializing schema json")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&json_path, &bytes)?;

    if !formulas.is_empty() {
        let formulas_dir = queue_dir.join("formulas");
        std::fs::create_dir_all(&formulas_dir)
            .with_context(|| format!("creating {}", formulas_dir.display()))?;
        for (field_id, formula) in formulas {
            let py_path = formulas_dir.join(format!("{field_id}.py"));
            // Byte-exact: write the formula text as-is, no trailing-newline padding.
            write_atomic(&py_path, formula.as_bytes())?;
        }
    }

    Ok(bytes)
}

/// Read a schema from disk. If a `formulas/` subdirectory exists, splice each
/// `<id>.py` back into the corresponding datapoint's `formula` property.
pub fn read_schema(queue_dir: &Path) -> Result<Schema> {
    let json_path = queue_dir.join("schema.json");
    let raw = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    let formulas_dir = queue_dir.join("formulas");
    if formulas_dir.is_dir() {
        if let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
            for node in content.iter_mut() {
                merge_formulas(node, &formulas_dir)?;
            }
        }
    }

    let schema: Schema = serde_json::from_value(value)
        .with_context(|| format!("deserializing schema from {}", json_path.display()))?;
    Ok(schema)
}

/// Serialize a schema to its on-disk byte form WITHOUT writing. Returns the
/// JSON bytes (post-extraction) and the list of `(field_id, formula_bytes)`
/// pairs sorted by field_id. Used by the queues driver to compute
/// `schema_combined_hash` for 3-way merge before deciding whether to write.
pub fn serialize_schema(schema: &Schema) -> Result<(Vec<u8>, Vec<(String, Vec<u8>)>)> {
    let mut value = serde_json::to_value(schema)
        .context("serializing schema to value")?;

    let mut formulas: Vec<(String, String)> = Vec::new();
    if let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
        for node in content.iter_mut() {
            extract_formulas(node, &mut formulas);
        }
    }
    formulas.sort_by(|a, b| a.0.cmp(&b.0));

    let bytes = serde_json::to_vec_pretty(&value)
        .context("serializing schema json")?;
    let mut bytes = bytes;
    bytes.push(b'\n');

    let formulas_bytes: Vec<(String, Vec<u8>)> = formulas
        .into_iter()
        .map(|(id, code)| (id, code.into_bytes()))
        .collect();

    Ok((bytes, formulas_bytes))
}

/// Walk the on-disk `<queue_dir>/formulas/` directory and return
/// `(field_id, bytes)` pairs sorted by `field_id`. Returns an empty vec if the
/// directory does not exist. Used to compute the LOCAL combined hash.
pub fn read_local_formulas(queue_dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let formulas_dir = queue_dir.join("formulas");
    if !formulas_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in std::fs::read_dir(&formulas_dir)
        .with_context(|| format!("reading {}", formulas_dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("listing {}", formulas_dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(field_id) = name.strip_suffix(".py") {
            let bytes = std::fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            out.push((field_id.to_string(), bytes));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Recursively walk a schema content node. For any datapoint with a string
/// `formula`, remove it and append (id, formula) to `out`. Recurses into
/// `children` arrays.
fn extract_formulas(node: &mut Value, out: &mut Vec<(String, String)>) {
    let Some(obj) = node.as_object_mut() else { return };

    let is_datapoint = obj.get("category").and_then(|c| c.as_str()) == Some("datapoint");
    if is_datapoint {
        let id = obj.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
        if let Some(id) = id {
            if let Some(Value::String(formula)) = obj.remove("formula") {
                out.push((id, formula));
            }
        }
    }

    if let Some(children) = obj.get_mut("children").and_then(|c| c.as_array_mut()) {
        for child in children.iter_mut() {
            extract_formulas(child, out);
        }
    }
}

/// Reverse of `extract_formulas`: for any datapoint without a `formula`
/// property, look up `<formulas_dir>/<id>.py` and insert its contents.
fn merge_formulas(node: &mut Value, formulas_dir: &Path) -> Result<()> {
    let Some(obj) = node.as_object_mut() else { return Ok(()) };

    let is_datapoint = obj.get("category").and_then(|c| c.as_str()) == Some("datapoint");
    if is_datapoint && !obj.contains_key("formula") {
        let id = obj.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
        if let Some(id) = id {
            let py_path = formulas_dir.join(format!("{id}.py"));
            if py_path.exists() {
                let formula = std::fs::read_to_string(&py_path)
                    .with_context(|| format!("reading {}", py_path.display()))?;
                obj.insert("formula".to_string(), Value::String(formula));
            }
        }
    }

    if let Some(children) = obj.get_mut("children").and_then(|c| c.as_array_mut()) {
        for child in children.iter_mut() {
            merge_formulas(child, formulas_dir)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample_with_formula() -> Schema {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/schemas/1",
            "name": "S",
            "queues": [],
            "content": [
                {
                    "category": "section",
                    "id": "header",
                    "label": "Header",
                    "children": [
                        {
                            "category": "datapoint",
                            "id": "invoice_id",
                            "type": "string"
                        },
                        {
                            "category": "datapoint",
                            "id": "amount_total",
                            "type": "number",
                            "formula": "amount_due + amount_tax"
                        }
                    ]
                }
            ]
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn writes_schema_json_and_formulas_py() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        write_schema(&dir.path().join("q1"), &sample_with_formula()).unwrap();
        assert!(dir.path().join("q1/schema.json").exists());
        assert!(dir.path().join("q1/formulas/amount_total.py").exists());
        let f = std::fs::read_to_string(dir.path().join("q1/formulas/amount_total.py")).unwrap();
        assert_eq!(f, "amount_due + amount_tax");
    }

    #[test]
    fn json_does_not_contain_formula_string() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        write_schema(&dir.path().join("q1"), &sample_with_formula()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("q1/schema.json")).unwrap();
        assert!(!raw.contains("amount_due + amount_tax"), "formula should be in .py, not .json: {raw}");
        assert!(raw.contains("amount_total"), "field id preserved");
    }

    #[test]
    fn round_trip_preserves_formula() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        let original = sample_with_formula();
        write_schema(&dir.path().join("q1"), &original).unwrap();
        let read = read_schema(&dir.path().join("q1")).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn schema_with_no_formulas_creates_no_formulas_dir() {
        let v = json!({
            "id": 2,
            "url": "https://x/api/v1/schemas/2",
            "name": "Empty",
            "queues": [],
            "content": [
                {
                    "category": "datapoint",
                    "id": "plain",
                    "type": "string"
                }
            ]
        });
        let schema: Schema = serde_json::from_value(v).unwrap();
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q2")).unwrap();
        write_schema(&dir.path().join("q2"), &schema).unwrap();
        assert!(dir.path().join("q2/schema.json").exists());
        assert!(!dir.path().join("q2/formulas").exists(), "formulas dir should not be created when no formulas exist");
    }

    #[test]
    fn serialize_schema_returns_json_and_formulas() {
        let s = sample_with_formula();
        let (json_bytes, formulas) = serialize_schema(&s).unwrap();
        let json_str = std::str::from_utf8(&json_bytes).unwrap();
        assert!(!json_str.contains("amount_due + amount_tax"));
        assert_eq!(formulas.len(), 1);
        assert_eq!(formulas[0].0, "amount_total");
        assert_eq!(formulas[0].1, b"amount_due + amount_tax".to_vec());
    }

    #[test]
    fn serialize_schema_returns_empty_formulas_when_none() {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/schemas/1",
            "name": "S",
            "queues": [],
            "content": [{ "category": "datapoint", "id": "f", "type": "string" }]
        });
        let s: Schema = serde_json::from_value(v).unwrap();
        let (_, formulas) = serialize_schema(&s).unwrap();
        assert!(formulas.is_empty());
    }

    #[test]
    fn read_local_formulas_returns_empty_when_dir_missing() {
        let dir = TempDir::new().unwrap();
        let res = read_local_formulas(dir.path()).unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn read_local_formulas_returns_sorted_by_field_id() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("formulas")).unwrap();
        std::fs::write(dir.path().join("formulas/zeta.py"), b"z").unwrap();
        std::fs::write(dir.path().join("formulas/alpha.py"), b"a").unwrap();
        std::fs::write(dir.path().join("formulas/mid.py"), b"m").unwrap();
        let f = read_local_formulas(dir.path()).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].0, "alpha");
        assert_eq!(f[1].0, "mid");
        assert_eq!(f[2].0, "zeta");
    }

    #[test]
    fn deeply_nested_formula_extracted() {
        let v = json!({
            "id": 3,
            "url": "https://x/api/v1/schemas/3",
            "name": "Nested",
            "queues": [],
            "content": [
                {
                    "category": "section",
                    "id": "outer",
                    "children": [
                        {
                            "category": "section",
                            "id": "inner",
                            "children": [
                                {
                                    "category": "datapoint",
                                    "id": "deep_field",
                                    "formula": "1 + 2"
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        let schema: Schema = serde_json::from_value(v).unwrap();
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q3")).unwrap();
        write_schema(&dir.path().join("q3"), &schema).unwrap();
        assert!(dir.path().join("q3/formulas/deep_field.py").exists());
    }
}
