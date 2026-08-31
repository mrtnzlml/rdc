//! Copy `testdata/live/snapshot/` into a project's env tree, substituting the
//! run id and the org url.
//!
//! Unlike every other live fixture, this one is not seeded through the API and
//! pulled — it is written straight to disk with no lockfile entries, so `rdc
//! sync` classifies every object as a LocalCreate and POSTs the whole graph.
//! That is the only way to exercise the create path in a SINGLE org: the
//! migrate-driven scenarios need `RDC_LIVE_TGT_*` and skip without it.
//!
//! It is also the only test anywhere that proves a hand-written snapshot — what
//! a user commits to git and deploys into a fresh env — works against the real
//! API.

use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use std::path::{Path, PathBuf};

/// `testdata/live/snapshot`, resolved from the crate root.
pub fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/live/snapshot")
}

/// Replace the two fixture placeholders. `{{RUN}}` is the bare run id (so a
/// fixture path reads `rdc-it-{{RUN}}-engine` and the marker stays legible);
/// `{{ORG_URL}}` is the env's organization url, required on a workspace or
/// label create and unavoidably env-specific.
pub fn substitute(raw: &str, run: &str, org_url: &str) -> String {
    raw.replace("{{RUN}}", run).replace("{{ORG_URL}}", org_url)
}

/// Copy the whole fixture tree into `envs/<env>/`, substituting placeholders in
/// both file CONTENTS and PATH components.
#[allow(dead_code)]
pub fn write_snapshot(project: &ProjectFixture, env: &str, run_id: &RunId, org_url: &str) {
    let src = snapshot_dir();
    let dst = project.path().join(format!("envs/{env}"));
    copy_dir(&src, &dst, run_id.as_str(), org_url);
}

fn copy_dir(src: &Path, dst: &Path, run: &str, org_url: &str) {
    for entry in std::fs::read_dir(src)
        .unwrap_or_else(|e| panic!("reading fixture dir {}: {e}", src.display()))
    {
        let entry = entry.expect("fixture dir entry");
        let name = entry.file_name().to_string_lossy().to_string();
        let target = dst.join(substitute(&name, run, org_url));
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target)
                .unwrap_or_else(|e| panic!("creating {}: {e}", target.display()));
            copy_dir(&entry.path(), &target, run, org_url);
        } else {
            let raw = std::fs::read_to_string(entry.path())
                .unwrap_or_else(|e| panic!("reading {}: {e}", entry.path().display()));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
            }
            std::fs::write(&target, substitute(&raw, run, org_url))
                .unwrap_or_else(|e| panic!("writing {}: {e}", target.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_both_placeholders() {
        let out = substitute(
            r#"{"name":"rdc-it-{{RUN}}-ws","organization":"{{ORG_URL}}"}"#,
            "abc123",
            "https://api.example/v1/organizations/9",
        );
        assert_eq!(
            out,
            r#"{"name":"rdc-it-abc123-ws","organization":"https://api.example/v1/organizations/9"}"#
        );
    }

    /// The fixture must stay parseable and fully substituted — a stray
    /// placeholder would reach the API verbatim and fail with something far
    /// less legible than this assertion.
    #[test]
    fn every_fixture_file_parses_after_substitution() {
        let mut seen = 0;
        walk(&snapshot_dir(), &mut |path: &Path| {
            let raw = std::fs::read_to_string(path).unwrap();
            let out = substitute(&raw, "abc123", "https://api.example/v1/organizations/9");
            assert!(
                !out.contains("{{"),
                "unsubstituted placeholder left in {}",
                path.display()
            );
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                serde_json::from_str::<serde_json::Value>(&out)
                    .unwrap_or_else(|e| panic!("{} is not valid json after substitution: {e}", path.display()));
            }
            seen += 1;
        });
        assert_eq!(seen, 19, "fixture file count changed — update this test deliberately");
    }

    /// The queue MUST bind the engine. Without that binding the server never
    /// validates the schema's extracted fields against the engine's, and the
    /// ordering scenario silently stops testing the edge it exists for.
    #[test]
    fn the_queue_binds_the_engine() {
        let raw = std::fs::read_to_string(
            snapshot_dir()
                .join("workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/queue.json"),
        )
        .expect("queue fixture");
        assert!(
            raw.contains(r#""engine": "rdc://engines/rdc-it-{{RUN}}-engine""#),
            "the fixture queue must bind the fixture engine: {raw}"
        );
    }

    /// The engine field's `name` must equal the schema datapoint's `id`, or
    /// `POST /queues` refuses the create with "extracted field '<x>' is not
    /// present among names of engine fields".
    #[test]
    fn the_engine_field_name_matches_the_schema_datapoint_id() {
        let field: serde_json::Value = serde_json::from_str(&substitute(
            &std::fs::read_to_string(snapshot_dir().join(
                "engines/rdc-it-{{RUN}}-engine/fields/rdc-it-{{RUN}}-probe-field.json",
            ))
            .expect("field fixture"),
            "abc123",
            "https://api.example/v1/organizations/9",
        ))
        .unwrap();
        let schema: serde_json::Value = serde_json::from_str(&substitute(
            &std::fs::read_to_string(snapshot_dir().join(
                "workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/schema.json",
            ))
            .expect("schema fixture"),
            "abc123",
            "https://api.example/v1/organizations/9",
        ))
        .unwrap();
        let dp_id = &schema["content"][0]["children"][0]["id"];
        assert_eq!(&field["name"], dp_id, "engine field name must equal the datapoint id");
    }

    fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            if e.path().is_dir() {
                walk(&e.path(), f);
            } else {
                f(&e.path());
            }
        }
    }
}
