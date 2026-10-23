//! [`KindCodec`] implementation for the `engines` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec, plain_json_artifact};
use crate::snapshot::create::{redact_for_disk, strip_for_cross_env_patch};

pub struct Engines;

impl KindCodec for Engines {
    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        redact_for_disk(&mut v, "engines");
        plain_json_artifact(v)
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
