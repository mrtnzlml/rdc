//! [`KindCodec`] implementation for the `engines` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{redact_for_disk, strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct Engines;

impl KindCodec for Engines {
    fn kind(&self) -> &'static str {
        "engines"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        redact_for_disk(&mut v, "engines");
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "engines");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "engines");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        overlay.engine(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.engine_dir(slug).join("engine.json")
    }
}
