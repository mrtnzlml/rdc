//! The sync state's snapshot: one env's files as one opaque byte string, the
//! form a host stores (`state::snapshot_store`). It is the gzip of the JSON
//! `{files: {path: index}, contents: [text]}`; `files` maps each path to its
//! content's index, so identical contents (a base copy and its env file) are
//! stored once.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Snapshot {
    files: BTreeMap<String, usize>,
    contents: Vec<String>,
}

/// `files` (path → text) as a snapshot.
pub fn encode(files: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut snap = Snapshot::default();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (rel, text) in files {
        let i = *seen.entry(text.as_str()).or_insert_with(|| {
            snap.contents.push(text.clone());
            snap.contents.len() - 1
        });
        snap.files.insert(rel.clone(), i);
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&serde_json::to_vec(&snap)?)?;
    Ok(gz.finish()?)
}

/// A snapshot's files (path → text).
pub fn decode(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    use std::io::Read;
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(bytes).read_to_end(&mut json).context("decompressing it")?;
    let snap: Snapshot = serde_json::from_slice(&json).context("parsing it")?;
    snap.files
        .into_iter()
        .map(|(rel, i)| match snap.contents.get(i) {
            Some(text) => Ok((rel, text.clone())),
            None => Err(anyhow!("{rel} names content {i}, which it does not hold")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_stores_identical_contents_once() {
        let files: BTreeMap<String, String> = [
            ("envs/dev/a.json", "{\"a\": \"✓\"}\n"),
            (".rdc/state/dev.base/a.json", "{\"a\": \"✓\"}\n"),
            (".rdc/state/dev.lock.json", "{}\n"),
            ("envs/dev/empty.txt", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let bytes = encode(&files).unwrap();
        assert_eq!(decode(&bytes).unwrap(), files);
        use std::io::Read;
        let mut json = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut json).unwrap();
        let snap: Snapshot = serde_json::from_slice(&json).unwrap();
        assert_eq!(snap.contents.len(), 3, "the base copy shares its env file's content");
        assert!(decode(b"not gzip").is_err());
    }
}
