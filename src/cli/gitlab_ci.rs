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
use std::collections::BTreeMap;

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
}
