//! Building `.rdc/mapping.toml` for the deploy-flow scenarios.
//!
//! Written in the CURRENT N-way format — one `[[<kind>]]` row per object,
//! naming that object's slug in each env it exists in — rather than the legacy
//! per-pair `.rdc/map/<a>-to-<b>.toml`. The legacy → generic conversion has
//! hermetic coverage in `tests/cli_migrate.rs`; what the live suite must
//! exercise is the format projects actually commit today.

use crate::support::assert_local::lockfile_keys;
use rdc::state::lockfile::Lockfile;

/// Kinds that get a rename row. `email_templates` is deliberately absent: its
/// key is the compound `<ws>/<q>/<template>`, whose workspace and queue
/// segments already change with their parents' renames.
const RENAMED_KINDS: [&str; 7] =
    ["workspaces", "queues", "schemas", "inboxes", "hooks", "rules", "labels"];

/// A mapping that renames every object this run owns from `<slug>` in `src` to
/// `<slug><suffix>` in `tgt`.
///
/// Only slugs carrying `prefix` are mapped, so objects belonging to other runs
/// (or pre-existing org content) are never named in the file.
pub fn rename_mapping(lf_src: &Lockfile, prefix: &str, src: &str, tgt: &str, suffix: &str) -> String {
    let mut out = String::from("version = 1\n\n");
    for kind in RENAMED_KINDS {
        for slug in lockfile_keys(lf_src, kind)
            .into_iter()
            .filter(|s| s.starts_with(prefix))
        {
            out.push_str(&format!(
                "[[{kind}]]\n{src} = \"{slug}\"\n{tgt} = \"{slug}{suffix}\"\n\n"
            ));
        }
    }
    out
}

/// Write `mapping` to `<project>/.rdc/mapping.toml`, creating `.rdc/`.
pub fn write_mapping(project_root: &std::path::Path, mapping: &str) {
    let dir = project_root.join(".rdc");
    std::fs::create_dir_all(&dir).expect("creating .rdc/");
    std::fs::write(dir.join("mapping.toml"), mapping).expect("writing .rdc/mapping.toml");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdc::state::lockfile::ObjectEntry;
    use std::collections::BTreeMap;

    fn lf_with(kind: &str, slugs: &[&str]) -> Lockfile {
        let mut objects: BTreeMap<String, BTreeMap<String, ObjectEntry>> = BTreeMap::new();
        let mut inner = BTreeMap::new();
        for (i, s) in slugs.iter().enumerate() {
            inner.insert(
                (*s).to_string(),
                ObjectEntry {
                    id: 100 + i as u64,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
        }
        objects.insert(kind.to_string(), inner);
        Lockfile { version: 3, objects, ..Lockfile::default() }
    }

    #[test]
    fn emits_one_row_per_object_in_the_generic_format() {
        let lf = lf_with("queues", &["rdc-it-x-invoices", "rdc-it-x-orders"]);
        let m = rename_mapping(&lf, "rdc-it-x-", "test", "prod", "-prod");
        assert!(m.starts_with("version = 1\n"), "{m}");
        assert!(
            m.contains("[[queues]]\ntest = \"rdc-it-x-invoices\"\nprod = \"rdc-it-x-invoices-prod\"\n"),
            "{m}"
        );
        assert!(
            m.contains("[[queues]]\ntest = \"rdc-it-x-orders\"\nprod = \"rdc-it-x-orders-prod\"\n"),
            "{m}"
        );
    }

    #[test]
    fn skips_slugs_outside_this_runs_prefix() {
        let lf = lf_with("labels", &["rdc-it-x-mine", "someone-elses-label"]);
        let m = rename_mapping(&lf, "rdc-it-x-", "test", "prod", "-prod");
        assert!(m.contains("rdc-it-x-mine"), "{m}");
        assert!(
            !m.contains("someone-elses-label"),
            "a mapping must never name objects this run does not own: {m}"
        );
    }

    #[test]
    fn the_result_parses_as_a_generic_mapping() {
        let lf = lf_with("hooks", &["rdc-it-x-validator"]);
        let m = rename_mapping(&lf, "rdc-it-x-", "test", "prod", "-prod");
        let parsed: rdc::mapping::GenericMapping =
            toml::from_str(&m).expect("rename_mapping must emit a parseable mapping");
        let rows = parsed.kind_rows("hooks").expect("hooks rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("test").map(String::as_str), Some("rdc-it-x-validator"));
        assert_eq!(rows[0].get("prod").map(String::as_str), Some("rdc-it-x-validator-prod"));
    }
}
