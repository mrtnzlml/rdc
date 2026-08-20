//! Generates the env-shaped parts of a project's `.gitlab-ci.yml`.
//!
//! rdc owns two named regions in that file and nothing else: the archive job's
//! `parallel:matrix` (one entry per env) and a block of drafted deploy jobs.
//! Everything outside the markers belongs to the user, which is why
//! regeneration splices instead of rewriting.
//!
//! There is deliberately no promotion chain here. `rdc.toml` records envs in a
//! `BTreeMap` with no ordering meaning, so a draft ships with `RDC_SRC` empty
//! and a guard in the template refuses to run until a human fills it in — a
//! plausible-but-wrong source feeding `migrate --mirror` + `sync
//! --allow-deletes` is worse than a blank.

use crate::config::EnvConfig;
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet};

/// The archive job's matrix: one entry per env.
pub const REGION_ARCHIVE_ENVS: &str = "rdc:archive-envs";
/// One drafted deploy button per env.
pub const REGION_DEPLOY_JOBS: &str = "rdc:deploy-jobs";

/// Every region rdc owns. Anything else spelled `# >>> rdc:…` is a typo.
pub const REGIONS: [&str; 2] = [REGION_ARCHIVE_ENVS, REGION_DEPLOY_JOBS];

/// Render both region bodies for `envs`. A body is `\n`-separated lines with no
/// leading or trailing newline; [`splice`] applies the marker's indentation.
pub fn render_regions(envs: &BTreeMap<String, EnvConfig>) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (REGION_ARCHIVE_ENVS, render_archive_envs(envs)),
        (REGION_DEPLOY_JOBS, render_deploy_jobs(envs)),
    ])
}

fn render_archive_envs(envs: &BTreeMap<String, EnvConfig>) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(envs.len() * 2);
    for name in envs.keys() {
        lines.push(format!("- RDC_ENV: {}", yaml_quote(name)));
        // Emitted rather than re-derived in shell: `tr -c '[:alnum:]' '_'` is
        // locale-dependent for non-ASCII, and we already know the name here.
        lines.push(format!(
            "  RDC_VAR_SUFFIX: {}",
            yaml_quote(&crate::secrets::env_var_suffix(name))
        ));
    }
    lines.join("\n")
}

fn render_deploy_jobs(envs: &BTreeMap<String, EnvConfig>) -> String {
    if envs.len() < 2 {
        return "# No deploy buttons: promoting needs a second env. Add one with\n\
                # `rdc init --env <env>=<api_base>:<org_id>` and this region fills in."
            .to_string();
    }
    let mut out = String::from(
        "# DRAFTS. Each button needs its RDC_SRC filled in before it can be pressed,\n\
         # and the jobs for hand-authored source envs (a dev env people edit) should\n\
         # be deleted -- an archive records those, a deploy would overwrite them.",
    );
    for name in envs.keys() {
        let quoted = yaml_quote(name);
        out.push_str(&format!(
            "\n\n{job}:\n  \
             extends: .rdc-deploy\n  \
             resource_group: {quoted}\n  \
             environment:\n    \
             name: {quoted}\n  \
             variables:\n    \
             RDC_ENV: {quoted}\n    \
             RDC_SRC: \"\"   # TODO: env to promote from",
            job = yaml_quote(&format!("deploy:{name}")),
        ));
    }
    out
}

/// Double-quote a YAML scalar. Env names reach us from `--env` or a
/// hand-written `rdc.toml`, neither of which validates them (only the
/// interactive prompt restricts them to `[A-Za-z0-9_-]`), so an unquoted name
/// could produce invalid YAML or a mis-parsed job name.
fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Names of the rdc regions whose opening marker appears in `existing`.
pub fn regions_present(existing: &str) -> BTreeSet<String> {
    existing.lines().filter_map(|l| marker_name(l, ">>>")).collect()
}

/// If `line` is an rdc region marker of `kind` (`">>>"` or `"<<<"`), its region
/// name. The marker may carry a trailing comment, and it may be indented.
fn marker_name(line: &str, kind: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(&format!("# {kind} rdc:"))?;
    let name: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    if name.is_empty() {
        return None;
    }
    Some(format!("rdc:{name}"))
}

/// Replace the body of every rdc region in `existing` with the rendered one,
/// leaving every other byte alone.
///
/// `Ok(None)` means the file carries no rdc markers — a hand-written pipeline,
/// which is left untouched rather than converted. Every error case (a region
/// never closed, closed without opening, duplicated, nested, or misspelled)
/// returns `Err` before producing any output, so the caller has nothing to
/// write: a half-spliced pipeline is worse than a diagnosed one.
pub fn splice(existing: &str, regions: &BTreeMap<&str, String>) -> Result<Option<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // (region name, line it opened on, its indentation)
    let mut open: Option<(String, usize, String)> = None;
    let mut any = false;

    for (idx, line) in existing.lines().enumerate() {
        let lineno = idx + 1;

        if let Some(name) = marker_name(line, ">>>") {
            let Some(body) = regions.get(name.as_str()) else {
                return Err(anyhow!(
                    "line {lineno}: unknown rdc region '{name}' (known: {})",
                    REGIONS.join(", ")
                ));
            };
            if let Some((open_name, open_line, _)) = &open {
                return Err(anyhow!(
                    "line {lineno}: region '{name}' opens while '{open_name}' from \
                     line {open_line} is still open"
                ));
            }
            if !seen.insert(name.clone()) {
                return Err(anyhow!(
                    "line {lineno}: region '{name}' appears more than once"
                ));
            }
            any = true;
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            out.push(line.to_string());
            for body_line in body.lines() {
                if body_line.is_empty() {
                    out.push(String::new());
                } else {
                    out.push(format!("{indent}{body_line}"));
                }
            }
            open = Some((name, lineno, indent));
            continue;
        }

        if let Some(name) = marker_name(line, "<<<") {
            match &open {
                Some((open_name, _, _)) if *open_name == name => {
                    out.push(line.to_string());
                    open = None;
                }
                Some((open_name, open_line, _)) => {
                    return Err(anyhow!(
                        "line {lineno}: region '{name}' closes while '{open_name}' \
                         from line {open_line} is open"
                    ));
                }
                None => {
                    return Err(anyhow!(
                        "line {lineno}: region '{name}' closes without opening"
                    ));
                }
            }
            continue;
        }

        // Lines inside an open region are the previously generated body: dropped.
        if open.is_none() {
            out.push(line.to_string());
        }
    }

    if let Some((name, lineno, _)) = open {
        return Err(anyhow!(
            "region '{name}' opened at line {lineno} is never closed by '# <<< {name}'"
        ));
    }
    if !any {
        return Ok(None);
    }
    let mut joined = out.join("\n");
    // `lines()` drops the final newline; put it back only if it was there.
    if existing.ends_with('\n') {
        joined.push('\n');
    }
    Ok(Some(joined))
}

/// Splice the embedded template for a brand-new (or `--force`d) project.
/// Unlike [`splice`], every region must be present: the template ships with us,
/// so a missing marker is a bug here rather than a user's edit.
pub fn generate(template: &str, envs: &BTreeMap<String, EnvConfig>) -> Result<String> {
    let present = regions_present(template);
    let missing: Vec<&str> = REGIONS.iter().copied().filter(|r| !present.contains(*r)).collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "the embedded templates/gitlab-ci.yml is missing region marker(s): {}",
            missing.join(", ")
        ));
    }
    splice(template, &render_regions(envs))?
        .ok_or_else(|| anyhow!("the embedded templates/gitlab-ci.yml has no rdc region markers"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvConfig;
    use std::collections::BTreeMap;

    fn envs(names: &[&str]) -> BTreeMap<String, EnvConfig> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                (
                    (*n).to_string(),
                    EnvConfig {
                        api_base: "https://example.rossum.app/api/v1".to_string(),
                        org_id: 100 + i as u64,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn archive_region_pairs_each_env_with_its_credential_suffix() {
        let r = render_regions(&envs(&["dev", "dev-us"]));
        assert_eq!(
            r[REGION_ARCHIVE_ENVS],
            "- RDC_ENV: \"dev\"\n  \
               RDC_VAR_SUFFIX: \"DEV\"\n\
             - RDC_ENV: \"dev-us\"\n  \
               RDC_VAR_SUFFIX: \"DEV_US\""
        );
    }

    #[test]
    fn archive_region_quotes_names_the_prompt_would_have_rejected() {
        // `--env` and a hand-written rdc.toml do not validate names.
        let r = render_regions(&envs(&["we ird", "a\"b"]));
        assert!(r[REGION_ARCHIVE_ENVS].contains("- RDC_ENV: \"we ird\""));
        assert!(r[REGION_ARCHIVE_ENVS].contains("- RDC_ENV: \"a\\\"b\""));
        assert!(r[REGION_ARCHIVE_ENVS].contains("RDC_VAR_SUFFIX: \"WE_IRD\""));
    }

    #[test]
    fn a_single_env_gets_no_deploy_drafts() {
        let r = render_regions(&envs(&["dev"]));
        assert!(r[REGION_DEPLOY_JOBS].starts_with('#'));
        assert!(!r[REGION_DEPLOY_JOBS].contains("extends: .rdc-deploy"));
    }

    #[test]
    fn each_env_gets_one_draft_with_an_empty_source() {
        let r = render_regions(&envs(&["dev", "prod", "test"]));
        let body = &r[REGION_DEPLOY_JOBS];
        assert_eq!(body.matches("extends: .rdc-deploy").count(), 3);
        assert!(body.contains("\"deploy:test\":\n  extends: .rdc-deploy"));
        assert!(body.contains("    RDC_ENV: \"test\""));
        assert_eq!(body.matches("RDC_SRC: \"\"").count(), 3);
        // BTreeMap order, so the render is stable for the same rdc.toml.
        let dev = body.find("\"deploy:dev\"").unwrap();
        let prod = body.find("\"deploy:prod\"").unwrap();
        let test = body.find("\"deploy:test\"").unwrap();
        assert!(dev < prod && prod < test);
    }

    #[test]
    fn region_bodies_never_start_or_end_with_a_blank_line() {
        // splice re-indents each body line; a stray blank would churn the file.
        for body in render_regions(&envs(&["dev", "test"])).values() {
            assert!(!body.starts_with('\n') && !body.ends_with('\n'), "{body:?}");
        }
    }

    const FILE: &str = "\
before
  parallel:
    matrix:
      # >>> rdc:archive-envs  (generated)
      - RDC_ENV: \"stale\"
      # <<< rdc:archive-envs
middle
# >>> rdc:deploy-jobs  (generated)
stale
# <<< rdc:deploy-jobs
after
";

    #[test]
    fn splice_replaces_bodies_and_keeps_everything_else() {
        let out = splice(FILE, &render_regions(&envs(&["dev", "test"])))
            .unwrap()
            .unwrap();
        assert!(out.starts_with("before\n  parallel:\n    matrix:\n"));
        assert!(out.ends_with("# <<< rdc:deploy-jobs\nafter\n"));
        assert!(out.contains("\nmiddle\n"));
        assert!(!out.contains("stale"));
        // the marker lines themselves, comments included, survive verbatim
        assert!(out.contains("      # >>> rdc:archive-envs  (generated)\n"));
    }

    #[test]
    fn splice_indents_a_body_to_its_marker() {
        let out = splice(FILE, &render_regions(&envs(&["dev"]))).unwrap().unwrap();
        assert!(out.contains("\n      - RDC_ENV: \"dev\"\n"));
        assert!(out.contains("\n        RDC_VAR_SUFFIX: \"DEV\"\n"));
    }

    #[test]
    fn splice_leaves_a_hand_written_pipeline_alone() {
        let hand = "stages:\n  - test\npytest:\n  script:\n    - pytest -q\n";
        assert!(splice(hand, &render_regions(&envs(&["dev"]))).unwrap().is_none());
    }

    #[test]
    fn splice_preserves_a_missing_trailing_newline() {
        let no_nl = FILE.trim_end_matches('\n');
        let out = splice(no_nl, &render_regions(&envs(&["dev"]))).unwrap().unwrap();
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn splice_rejects_a_region_that_is_never_closed() {
        let broken = "# >>> rdc:deploy-jobs\nbody\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("rdc:deploy-jobs"), "{err}");
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn splice_rejects_a_close_without_an_open() {
        let broken = "# <<< rdc:deploy-jobs\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("without opening"), "{err}");
    }

    #[test]
    fn splice_rejects_a_duplicated_region() {
        let broken = "# >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n\
                      # >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn splice_rejects_a_misspelled_region() {
        // Silently copying it would mean that region never updates again.
        let broken = "# >>> rdc:archive-env\n# <<< rdc:archive-env\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("unknown rdc region"), "{err}");
    }

    #[test]
    fn splice_rejects_a_region_opened_inside_another() {
        let broken = "# >>> rdc:deploy-jobs\n# >>> rdc:archive-envs\n\
                      # <<< rdc:archive-envs\n# <<< rdc:deploy-jobs\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("still open"), "{err}");
    }

    #[test]
    fn splice_is_idempotent() {
        let regions = render_regions(&envs(&["dev", "test"]));
        let once = splice(FILE, &regions).unwrap().unwrap();
        let twice = splice(&once, &regions).unwrap().unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn template_carries_both_regions() {
        // The template is ours; a missing marker is a bug in this repo.
        let present = regions_present(crate::cli::init::GITLAB_CI_TEMPLATE);
        for region in REGIONS {
            assert!(present.contains(region), "template is missing {region}");
        }
    }

    #[test]
    fn generate_fills_the_embedded_template() {
        let out = generate(crate::cli::init::GITLAB_CI_TEMPLATE, &envs(&["dev", "test"])).unwrap();
        assert!(out.contains("- RDC_ENV: \"dev\""));
        assert!(out.contains("\"deploy:test\":"));
        assert!(!out.contains("# TODO: the envs to archive"));
    }

    /// The committed regions must be exactly what the renderer produces for the
    /// canonical example, so the file on GitHub and the file the binary writes
    /// cannot drift. This is one half of what the old byte-for-byte assertion did.
    #[test]
    fn committed_template_regions_match_the_renderer() {
        let template = crate::cli::init::GITLAB_CI_TEMPLATE;
        let rendered = render_regions(&envs(&["dev", "prod", "test"]));
        // Re-splicing the canonical envs into the template must be a no-op.
        assert_eq!(
            splice(template, &rendered).unwrap().unwrap(),
            template,
            "templates/gitlab-ci.yml's regions differ from render_regions(dev, prod, test)"
        );
    }
}
