//! Renames an env inside an existing `.gitlab-ci.yml`, for `rdc edit env
//! rename`.
//!
//! Both generated regions name envs. `rdc:archive-envs` is re-rendered from
//! `rdc.toml` anyway, but `rdc:deploy-jobs` is the project's own text, and
//! `merge_deploy_jobs` decides which envs still need a draft by reading the
//! archive region on disk. So the old name is rewritten in both regions
//! first and the normal splice runs on the result: the renamed job counts as
//! already offered, and no duplicate draft is appended.

use crate::cli::gitlab_ci::{has_deploy_job, render_regions_for_existing, REGION_ARCHIVE_ENVS, REGION_DEPLOY_JOBS};
use crate::cli::regions::{self, YAML};
use crate::config::EnvConfig;
use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// One value rewritten inside `rdc:deploy-jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiChange {
    /// The job the line belongs to, e.g. `deploy:test`.
    pub job: String,
    /// The YAML key whose value changed; empty for a job-key rename.
    pub key: String,
    pub from: String,
    pub to: String,
}

impl std::fmt::Display for CiChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.key.is_empty() {
            write!(f, "{} -> {}", self.from, self.to)
        } else {
            write!(f, "{} {} \"{}\" -> \"{}\"", self.job, self.key, self.from, self.to)
        }
    }
}

/// The outcome of renaming an env in a pipeline.
#[derive(Debug)]
pub struct PipelineRename {
    /// The new file text; `None` when the file has no rdc markers and is
    /// left alone.
    pub text: Option<String>,
    pub changes: Vec<CiChange>,
    pub warnings: Vec<String>,
}

/// Rename `old` to `new` in the pipeline text `existing`, then refresh both
/// regions for `new_envs` (the env set after the rename).
pub fn rename_in_pipeline(
    existing: &str,
    old: &str,
    new: &str,
    new_envs: &BTreeMap<String, EnvConfig>,
) -> Result<PipelineRename> {
    // A job left behind for an env removed from rdc.toml (the deploy region
    // never drops one) would become a duplicate YAML key.
    if has_deploy_job(existing, new) {
        bail!(".gitlab-ci.yml already has a deploy:{new} job. Delete or rename it, then retry the rename.");
    }
    let (rewritten, changes, mut warnings) = rewrite_regions(existing, old, new);
    let regions = render_regions_for_existing(&rewritten, new_envs);
    let text = regions::splice(&rewritten, &regions, YAML)?;
    if text.is_none() {
        warnings.push(".gitlab-ci.yml has no rdc region markers, so rdc left it alone".to_string());
    }
    Ok(PipelineRename { text, changes, warnings })
}

fn rewrite_regions(existing: &str, old: &str, new: &str) -> (String, Vec<CiChange>, Vec<String>) {
    let old_job = format!("deploy:{old}");
    let new_job = format!("deploy:{new}");
    let mut out = String::with_capacity(existing.len());
    let mut changes = Vec::new();
    let mut warnings = Vec::new();
    let mut region: Option<String> = None;
    let mut job = String::new();
    for (idx, raw) in existing.split_inclusive('\n').enumerate() {
        let body = raw.trim_end_matches(['\n', '\r']);
        let eol = &raw[body.len()..];
        if let Some(name) = regions::marker_name(body, ">>>", YAML) {
            region = Some(name);
            out.push_str(raw);
            continue;
        }
        if regions::marker_name(body, "<<<", YAML).is_some() {
            region = None;
            out.push_str(raw);
            continue;
        }
        if let Some(key) = job_key(body) {
            job = key.to_string();
        }
        let in_deploy = region.as_deref() == Some(REGION_DEPLOY_JOBS);
        if !in_deploy && region.as_deref() != Some(REGION_ARCHIVE_ENVS) {
            if names_word(body, old) {
                warnings.push(format!(
                    ".gitlab-ci.yml line {}: still names \"{old}\" outside the rdc regions; rdc left it alone",
                    idx + 1
                ));
            }
            out.push_str(raw);
            continue;
        }
        if job_key(body) == Some(old_job.as_str()) {
            out.push_str(&body.replacen(&old_job, &new_job, 1));
            out.push_str(eol);
            if in_deploy {
                changes.push(CiChange {
                    job: old_job.clone(),
                    key: String::new(),
                    from: old_job.clone(),
                    to: new_job.clone(),
                });
            }
            job = new_job.clone();
            continue;
        }
        // `needs: ["deploy:<old>"]` and the like: GitLab rejects the whole
        // pipeline when a needed job does not exist.
        if in_deploy && let Some(line) = replace_token(body, &old_job, &new_job) {
            out.push_str(&line);
            out.push_str(eol);
            changes.push(CiChange { job: job.clone(), key: line_key(body), from: old_job.clone(), to: new_job.clone() });
            continue;
        }
        match rewrite_value(body, old, new) {
            Some((line, key)) => {
                out.push_str(&line);
                out.push_str(eol);
                // The archive region is re-rendered from rdc.toml right after,
                // so only the deploy jobs' changes are worth reporting.
                if in_deploy {
                    changes.push(CiChange { job: job.clone(), key, from: old.into(), to: new.into() });
                }
            }
            None => out.push_str(raw),
        }
    }
    (out, changes, warnings)
}

/// A top-level mapping key (`"deploy:dev":`, `stages:`), unquoted.
fn job_key(line: &str) -> Option<&str> {
    if line.starts_with(|c: char| c.is_whitespace() || c == '#' || c == '-') {
        return None;
    }
    let key = line.trim_end().strip_suffix(':')?;
    let key = ['"', '\'']
        .iter()
        .find_map(|q| key.strip_prefix(*q).and_then(|k| k.strip_suffix(*q)))
        .unwrap_or(key);
    (!key.is_empty()).then_some(key)
}

/// `line` with every whole-token `token` replaced, or `None` if it has none.
fn replace_token(line: &str, token: &str, with: &str) -> Option<String> {
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    for (i, _) in line.match_indices(token) {
        let before = line[..i].chars().next_back();
        let after = line[i + token.len()..].chars().next();
        if !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char) {
            out.push_str(&line[last..i]);
            out.push_str(with);
            last = i + token.len();
        }
    }
    (last > 0).then(|| out + &line[last..])
}

/// The YAML key a line sets (`needs` for `  needs: [...]`, `job` for
/// `  - job: x`), or `reference` when it has none.
fn line_key(line: &str) -> String {
    let t = line.trim_start();
    let t = t.strip_prefix("- ").unwrap_or(t);
    match t.split_once(':') {
        Some((k, _)) if !k.is_empty() && !k.contains(char::is_whitespace) => k.to_string(),
        _ => "reference".to_string(),
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.')
}

/// If `line` is `key: value` (optionally a `- ` list item) whose scalar value
/// equals `old`, the line with `new` in its place — same quoting, same
/// trailing comment — and the key.
fn rewrite_value(line: &str, old: &str, new: &str) -> Option<(String, String)> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let (dash, rest) = match rest.strip_prefix("- ") {
        Some(r) => ("- ", r),
        None => ("", rest),
    };
    let colon = rest.find(": ")?;
    let key = &rest[..colon];
    if key.is_empty() || key.starts_with('#') || key.contains(char::is_whitespace) {
        return None;
    }
    let after = &rest[colon + 2..];
    let gap = &after[..after.len() - after.trim_start().len()];
    let value = &after[gap.len()..];
    let (scalar, quote, tail) = match value.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &value[1..];
            let end = inner.find(q)?;
            (&inner[..end], Some(q), &inner[end + 1..])
        }
        _ => {
            let end = value.find(" #").unwrap_or(value.len());
            let scalar = value[..end].trim_end();
            (scalar, None, &value[scalar.len()..])
        }
    };
    if scalar != old {
        return None;
    }
    let replaced = match quote {
        Some(q) => format!("{q}{new}{q}"),
        None => new.to_string(),
    };
    Some((format!("{indent}{dash}{key}: {gap}{replaced}{tail}"), key.to_string()))
}

/// Whether `word` appears in `line` as a whole env name: not in a comment
/// line, and not inside `dev-us`, `RDC_TOKEN_DEV` or a path like
/// `/dev/null`.
fn names_word(line: &str, word: &str) -> bool {
    if line.trim_start().starts_with('#') {
        return false;
    }
    line.match_indices(word).any(|(i, _)| {
        let before = line[..i].chars().next_back();
        let after = line[i + word.len()..].chars().next();
        !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIPELINE: &str = "\
stages: [archive, deploy]

archive:
  parallel:
    matrix:
      # >>> rdc:archive-envs  (generated)
      - RDC_ENV: \"dev\"
        RDC_VAR_SUFFIX: \"DEV\"
      - RDC_ENV: \"dev-us\"
        RDC_VAR_SUFFIX: \"DEV_US\"
      - RDC_ENV: \"prod\"
        RDC_VAR_SUFFIX: \"PROD\"
      # <<< rdc:archive-envs

# >>> rdc:deploy-jobs  (kept)
\"deploy:dev\":
  extends: .rdc-deploy
  resource_group: \"dev\"
  environment:
    name: \"dev\"
  variables:
    RDC_ENV: \"dev\"
    RDC_SRC: \"\"   # TODO: env to promote from

\"deploy:prod\":
  extends: .rdc-deploy
  variables:
    RDC_ENV: \"prod\"
    RDC_SRC: dev   # promote from dev

\"deploy:dev-us\":
  extends: .rdc-deploy
  variables:
    RDC_ENV: \"dev-us\"
    RDC_SRC: \"dev-us\"
# <<< rdc:deploy-jobs

notify:
  script: echo dev done
";

    fn envs(names: &[&str]) -> BTreeMap<String, EnvConfig> {
        names
            .iter()
            .map(|n| (n.to_string(), EnvConfig { api_base: "https://example.test/api/v1".into(), org_id: 1 }))
            .collect()
    }

    fn renamed() -> PipelineRename {
        rename_in_pipeline(PIPELINE, "dev", "sandbox", &envs(&["dev-us", "prod", "sandbox"])).unwrap()
    }

    #[test]
    fn renames_exact_values_only() {
        let r = renamed();
        let text = r.text.unwrap();
        assert!(text.contains("\"deploy:sandbox\":\n"));
        assert!(!text.contains("\"deploy:dev\":"));
        assert!(text.contains("  resource_group: \"sandbox\"\n"));
        assert!(text.contains("    name: \"sandbox\"\n"));
        assert!(text.contains("    RDC_ENV: \"sandbox\"\n"));
        // unquoted, hand-edited value: rewritten, comment kept as written
        assert!(text.contains("    RDC_SRC: sandbox   # promote from dev\n"));
        // a longer env name that starts with `dev` is left alone
        assert!(text.contains("\"deploy:dev-us\":\n"));
        assert!(text.contains("    RDC_ENV: \"dev-us\"\n    RDC_SRC: \"dev-us\"\n"));
        let changes: Vec<String> = r.changes.iter().map(ToString::to_string).collect();
        assert_eq!(
            changes,
            vec![
                "deploy:dev -> deploy:sandbox",
                "deploy:sandbox resource_group \"dev\" -> \"sandbox\"",
                "deploy:sandbox name \"dev\" -> \"sandbox\"",
                "deploy:sandbox RDC_ENV \"dev\" -> \"sandbox\"",
                "deploy:prod RDC_SRC \"dev\" -> \"sandbox\"",
            ]
        );
    }

    #[test]
    fn appends_no_draft_and_rerenders_the_archive() {
        let text = renamed().text.unwrap();
        assert_eq!(text.matches("extends: .rdc-deploy").count(), 3);
        assert!(text.contains("      - RDC_ENV: \"sandbox\"\n        RDC_VAR_SUFFIX: \"SANDBOX\"\n"));
        assert!(!text.contains("RDC_ENV: \"dev\"\n"));
        // re-rendered in rdc.toml's (sorted) order
        let prod = text.find("- RDC_ENV: \"prod\"").unwrap();
        let sandbox = text.find("- RDC_ENV: \"sandbox\"").unwrap();
        assert!(prod < sandbox);
    }

    #[test]
    fn leaves_the_rest_of_the_file_alone_and_warns() {
        let r = renamed();
        let text = r.text.unwrap();
        assert!(text.starts_with("stages: [archive, deploy]\n\narchive:\n"));
        assert!(text.ends_with("notify:\n  script: echo dev done\n"));
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("still names \"dev\" outside the rdc regions"));
    }

    #[test]
    fn comments_and_paths_outside_the_regions_do_not_warn() {
        let text = PIPELINE.replace(
            "notify:\n  script: echo dev done\n",
            "# Keep hand-authored source envs (dev) out of the deploy jobs.\nnotify:\n  script: find . 2>/dev/null\n",
        );
        let r = rename_in_pipeline(&text, "dev", "sandbox", &envs(&["dev-us", "prod", "sandbox"])).unwrap();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn refuses_when_the_new_name_already_has_a_job() {
        let leftover = PIPELINE.replace(
            "# <<< rdc:deploy-jobs",
            "\"deploy:sandbox\":\n  extends: .rdc-deploy\n# <<< rdc:deploy-jobs",
        );
        let err = rename_in_pipeline(&leftover, "dev", "sandbox", &envs(&["dev-us", "prod", "sandbox"]))
            .unwrap_err();
        assert!(format!("{err:#}").contains("already has a deploy:sandbox job"), "{err:#}");
    }

    #[test]
    fn rewrites_references_to_the_old_job() {
        let chained = PIPELINE
            .replace(
                "    RDC_SRC: dev   # promote from dev\n",
                "    RDC_SRC: dev   # promote from dev\n  needs: [\"pytest\", \"deploy:dev\"]\n",
            )
            .replace(
                "    RDC_SRC: \"dev-us\"\n",
                "    RDC_SRC: \"dev-us\"\n  needs:\n    - job: deploy:dev-us\n",
            );
        let r = rename_in_pipeline(&chained, "dev", "sandbox", &envs(&["dev-us", "prod", "sandbox"])).unwrap();
        let text = r.text.unwrap();
        assert!(text.contains("  needs: [\"pytest\", \"deploy:sandbox\"]\n"), "{text}");
        assert!(text.contains("    - job: deploy:dev-us\n"), "{text}");
        assert!(
            r.changes.iter().any(|c| c.to_string() == "deploy:prod needs \"deploy:dev\" -> \"deploy:sandbox\""),
            "{:?}",
            r.changes
        );
    }

    #[test]
    fn single_quoted_job_keys_are_renamed() {
        let single = PIPELINE.replace("\"deploy:dev\":", "'deploy:dev':");
        let r = rename_in_pipeline(&single, "dev", "sandbox", &envs(&["dev-us", "prod", "sandbox"])).unwrap();
        let text = r.text.unwrap();
        assert!(text.contains("'deploy:sandbox':\n"), "{text}");
        assert!(!text.contains("'deploy:dev':"), "{text}");
    }

    #[test]
    fn a_file_without_markers_is_left_alone() {
        let r = rename_in_pipeline("deploy:\n  script: echo dev\n", "dev", "sandbox", &envs(&["sandbox"])).unwrap();
        assert!(r.text.is_none());
        assert!(r.warnings.iter().any(|w| w.contains("no rdc region markers")));
    }

    #[test]
    fn broken_markers_are_an_error() {
        let broken = "# >>> rdc:deploy-jobs\n\"deploy:dev\":\n  extends: .rdc-deploy\n";
        assert!(rename_in_pipeline(broken, "dev", "sandbox", &envs(&["sandbox"])).is_err());
    }
}
