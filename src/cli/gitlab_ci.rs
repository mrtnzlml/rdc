//! Generates the env-shaped parts of a project's `.gitlab-ci.yml`.
//!
//! rdc owns two named regions in that file and nothing else: the archive job's
//! `parallel:matrix` (one entry per env) and a block of drafted deploy jobs.
//! Everything outside the markers belongs to the user, which is why
//! regeneration splices instead of rewriting -- see [`crate::cli::regions`]
//! for the marker mechanics that make the splice possible.
//!
//! The two regions do NOT have the same policy, because they are not derived
//! the same way:
//!
//! - `rdc:archive-envs` is **fully** derived from `rdc.toml`, so it is
//!   re-rendered on every run and tracks the env set exactly.
//! - `rdc:deploy-jobs` is only **half** derived -- rdc knows the job name, the
//!   user supplies `RDC_SRC` (and any `needs` / `rules` / `RDC_CONFLICT` of
//!   their own). Re-rendering it would revert every finished button to a draft
//!   and resurrect the drafts a reader deliberately deleted, so on an existing
//!   file it is **additive**, like `.gitignore`: existing content survives
//!   verbatim and a draft is only appended for an env this file has not been
//!   offered one for. See [`render_regions_for_existing`].
//!
//! There is deliberately no promotion chain here. `rdc.toml` records envs in a
//! `BTreeMap` with no ordering meaning, so a draft ships with `RDC_SRC` empty
//! and a guard in the template refuses to run until a human fills it in — a
//! plausible-but-wrong source feeding `migrate --mirror` + `sync
//! --allow-deletes` is worse than a blank.

use crate::cli::regions;
use crate::config::EnvConfig;
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;

/// The archive job's matrix: one entry per env.
pub const REGION_ARCHIVE_ENVS: &str = "rdc:archive-envs";
/// One drafted deploy button per env.
pub const REGION_DEPLOY_JOBS: &str = "rdc:deploy-jobs";

/// Every region rdc owns. Anything else spelled `# >>> rdc:…` is a typo.
pub const REGIONS: [&str; 2] = [REGION_ARCHIVE_ENVS, REGION_DEPLOY_JOBS];

/// Render both region bodies from scratch for `envs`. A body is
/// `\n`-separated lines with no leading or trailing newline;
/// [`regions::splice`] applies the marker's indentation.
///
/// This is the **create** shape: it is what a brand-new (or wholesale
/// regenerated) pipeline gets, where there is no user content to preserve.
/// For a pipeline that already exists use [`render_regions_for_existing`],
/// which keeps the deploy jobs the file already carries.
pub fn render_regions(envs: &BTreeMap<String, EnvConfig>) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (REGION_ARCHIVE_ENVS, render_archive_envs(envs)),
        (REGION_DEPLOY_JOBS, render_deploy_jobs(envs)),
    ])
}

/// Render both region bodies for a pipeline that already exists on disk.
///
/// The archive matrix is re-rendered (it is fully derived). The deploy jobs
/// are **appended to**: whatever the region already holds is returned
/// verbatim -- filled-in `RDC_SRC` values, extra keys, comments, formatting --
/// plus one fresh draft for each env of `rdc.toml` that this file has not been
/// offered a draft for yet. Nothing is ever removed, so a draft the reader
/// deleted stays deleted and a job for an env no longer in `rdc.toml` is left
/// alone.
pub fn render_regions_for_existing(
    existing: &str,
    envs: &BTreeMap<String, EnvConfig>,
) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (REGION_ARCHIVE_ENVS, render_archive_envs(envs)),
        (REGION_DEPLOY_JOBS, merge_deploy_jobs(existing, envs)),
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

/// The body a project with fewer than two envs gets: there is nothing to
/// promote between yet. rdc's own text -- which is why [`merge_deploy_jobs`]
/// may replace it once a second env shows up, while it leaves anything a
/// reader wrote alone.
const DEPLOY_PLACEHOLDER: &str =
    "# No deploy buttons: promoting needs a second env. Add one with\n\
     # `rdc init --env <env>=<api_base>:<org_id>` and this region fills in.";

/// Introduces a set of freshly appended drafts.
const DEPLOY_HEADER: &str =
    "# DRAFTS. Each button needs its RDC_SRC filled in before it can be pressed,\n\
     # and the jobs for hand-authored source envs (a dev env people edit) should\n\
     # be deleted -- an archive records those, a deploy would overwrite them.";

/// One drafted deploy button, `RDC_SRC` deliberately empty (see the module doc).
fn deploy_draft(name: &str) -> String {
    let quoted = yaml_quote(name);
    format!(
        "{job}:\n  \
         extends: .rdc-deploy\n  \
         resource_group: {quoted}\n  \
         environment:\n    \
         name: {quoted}\n  \
         variables:\n    \
         RDC_ENV: {quoted}\n    \
         RDC_SRC: \"\"   # TODO: env to promote from",
        job = yaml_quote(&format!("deploy:{name}")),
    )
}

fn render_deploy_jobs(envs: &BTreeMap<String, EnvConfig>) -> String {
    if envs.len() < 2 {
        return DEPLOY_PLACEHOLDER.to_string();
    }
    let mut out = String::from(DEPLOY_HEADER);
    for name in envs.keys() {
        out.push_str("\n\n");
        out.push_str(&deploy_draft(name));
    }
    out
}

/// The additive deploy-jobs body: what the file already says, plus a draft for
/// each env it has not been offered one for.
///
/// "Has been offered one" is read off the file itself, with no extra syntax and
/// no state outside it: the **archive** region still on disk was rendered from
/// `rdc.toml` by the previous run, so it names exactly the envs rdc knew about
/// then. An env missing from it is new and gets a draft; an env listed there
/// whose draft is gone was deleted on purpose and stays gone. A `deploy:<env>`
/// job key anywhere in the file -- inside the region or outside it, since a
/// finished job may have been moved out -- also suppresses the draft, so a
/// duplicate job key can never be produced.
fn merge_deploy_jobs(existing: &str, envs: &BTreeMap<String, EnvConfig>) -> String {
    let Some(current) = regions::region_body(existing, REGION_DEPLOY_JOBS, regions::YAML) else {
        // No such region, or one that is never closed: splice either ignores
        // the name or fails the run, so this body is never written. Render the
        // from-scratch one so the value is still meaningful.
        return render_deploy_jobs(envs);
    };
    let untouched_placeholder = current.trim() == DEPLOY_PLACEHOLDER;
    let offered = archived_envs(existing);
    let missing: Vec<&str> = if envs.len() < 2 {
        // Same rule as the from-scratch render: one env has nothing to promote.
        Vec::new()
    } else {
        envs.keys()
            .map(String::as_str)
            // The untouched placeholder means this file has never carried a
            // draft, so every env is due one.
            .filter(|e| untouched_placeholder || !offered.contains(*e))
            .filter(|e| !has_deploy_job(existing, e))
            .collect()
    };
    if missing.is_empty() {
        // Byte-for-byte what the file already had, so the splice is a no-op.
        return current;
    }
    let mut chunks: Vec<String> = Vec::new();
    if untouched_placeholder || current.trim().is_empty() {
        chunks.push(DEPLOY_HEADER.to_string());
    } else {
        chunks.push(current.trim_end().to_string());
    }
    chunks.extend(missing.iter().map(|e| deploy_draft(e)));
    chunks.join("\n\n")
}

/// The envs named by the archive region still on disk -- the env set as of the
/// previous `rdc init`. Empty when that region is absent, which makes every env
/// look new; the `deploy:<env>` job-key check is what keeps that from
/// duplicating a job.
fn archived_envs(existing: &str) -> std::collections::BTreeSet<String> {
    let Some(body) = regions::region_body(existing, REGION_ARCHIVE_ENVS, regions::YAML) else {
        return std::collections::BTreeSet::new();
    };
    body.lines()
        .filter_map(|line| {
            let t = line.trim();
            let t = t.strip_prefix("- ").unwrap_or(t);
            let value = t.strip_prefix("RDC_ENV:")?;
            Some(yaml_unquote(value))
        })
        .collect()
}

/// Whether the file already declares a `deploy:<env>` job, in any of the three
/// spellings YAML allows for the key. Comment lines don't count: a reader who
/// commented a job out has removed it.
fn has_deploy_job(existing: &str, env: &str) -> bool {
    let job = format!("deploy:{env}");
    let forms = [
        format!("{}:", yaml_quote(&job)),
        format!("'{job}':"),
        format!("{job}:"),
    ];
    existing.lines().any(|line| {
        let t = line.trim_start();
        !t.starts_with('#') && forms.iter().any(|f| t.starts_with(f.as_str()))
    })
}

/// Read back a scalar [`yaml_quote`] wrote. Stops at the closing quote, so a
/// trailing comment on the line is ignored.
fn yaml_unquote(s: &str) -> String {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let mut out = String::with_capacity(rest.len());
        let mut escaped = false;
        for c in rest.chars() {
            if escaped {
                out.push(match c {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                break;
            } else {
                out.push(c);
            }
        }
        return out;
    }
    if let Some(rest) = s.strip_prefix('\'') {
        return rest.split('\'').next().unwrap_or("").to_string();
    }
    s.split(" #").next().unwrap_or(s).trim().to_string()
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

/// Splice the embedded template for a brand-new (or `--force`d) project.
/// Unlike [`regions::splice`], every region must be present: the template
/// ships with us, so a missing marker is a bug here rather than a user's edit.
pub fn generate(template: &str, envs: &BTreeMap<String, EnvConfig>) -> Result<String> {
    let present = regions::regions_present(template, regions::YAML);
    let missing: Vec<&str> = REGIONS.iter().copied().filter(|r| !present.contains(*r)).collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "the embedded templates/gitlab-ci.yml is missing region marker(s): {}",
            missing.join(", ")
        ));
    }
    regions::splice(template, &render_regions(envs), regions::YAML)?
        .ok_or_else(|| anyhow!("the embedded templates/gitlab-ci.yml has no rdc region markers"))
}

/// `rdc.toml` envs for a test, one per name, in `BTreeMap` order. Shared with
/// the [`crate::cli::regions`] and [`crate::cli::scaffold_docs`] test modules,
/// which render these same regions.
#[cfg(test)]
pub(crate) fn test_envs(names: &[&str]) -> BTreeMap<String, EnvConfig> {
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

#[cfg(test)]
mod tests {
    use super::test_envs as envs;
    use super::*;

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

    #[test]
    fn template_carries_both_regions() {
        // The template is ours; a missing marker is a bug in this repo.
        let present = regions::regions_present(crate::cli::init::GITLAB_CI_TEMPLATE, regions::YAML);
        for region in render_regions(&envs(&["dev"])).keys() {
            assert!(present.contains(*region), "template is missing {region}");
        }
    }

    #[test]
    fn generate_fills_the_embedded_template() {
        let out = generate(crate::cli::init::GITLAB_CI_TEMPLATE, &envs(&["dev", "test"])).unwrap();
        assert!(out.contains("- RDC_ENV: \"dev\""));
        assert!(out.contains("\"deploy:test\":"));
        assert!(!out.contains("# TODO: the envs to archive"));
    }

    /// A pipeline exactly as `rdc init` writes it for `names`, so the
    /// merge tests below start from the real template rather than a stub.
    fn pipeline(names: &[&str]) -> String {
        generate(crate::cli::init::GITLAB_CI_TEMPLATE, &envs(names)).unwrap()
    }

    /// What `rdc init` does to an existing pipeline: splice with the additive
    /// deploy body and the re-rendered archive matrix.
    fn reinit(file: &str, names: &[&str]) -> String {
        regions::splice(
            file,
            &render_regions_for_existing(file, &envs(names)),
            regions::YAML,
        )
        .unwrap()
        .unwrap()
    }

    /// The whole point of the additive region, in one scenario: a source filled
    /// in, a draft deleted, an env added. Only the third may change the file.
    #[test]
    fn deploy_jobs_survive_an_env_add_verbatim() {
        let file = pipeline(&["dev", "test"]);
        // 1. the reader finishes the `test` button
        let file = file.replace(
            "    RDC_ENV: \"test\"\n    RDC_SRC: \"\"   # TODO: env to promote from",
            "    RDC_ENV: \"test\"\n    RDC_SRC: \"dev\"\n  needs: [\"pytest\"]",
        );
        // 2. and deletes the draft for the env they author by hand
        let dev_draft = format!("{}\n\n", deploy_draft("dev"));
        assert!(file.contains(&dev_draft));
        let file = file.replace(&dev_draft, "");

        // 3. `rdc init --env prod`
        let out = reinit(&file, &["dev", "prod", "test"]);

        assert!(out.contains("RDC_SRC: \"dev\""), "a filled-in source must survive: {out}");
        assert!(out.contains("  needs: [\"pytest\"]"), "an added key must survive: {out}");
        assert!(!out.contains("\"deploy:dev\":"), "a deleted draft must stay deleted: {out}");
        assert!(out.contains("\"deploy:prod\":"), "the new env must get a draft: {out}");
        assert_eq!(out.matches("extends: .rdc-deploy").count(), 2);
        // the archive matrix is fully derived, so it tracks rdc.toml exactly
        assert!(out.contains("- RDC_ENV: \"prod\""));
    }

    /// Nothing new in `rdc.toml` -> not one byte changes, which is what makes
    /// `rdc init --force` safe to run on a project with finished buttons.
    #[test]
    fn re_init_with_the_same_envs_is_a_byte_no_op() {
        let file = pipeline(&["dev", "test"]);
        assert_eq!(reinit(&file, &["dev", "test"]), file);

        // ...including once the reader has edited the region.
        let edited = file.replace("RDC_SRC: \"\"   # TODO: env to promote from", "RDC_SRC: \"dev\"");
        assert_ne!(edited, file);
        assert_eq!(reinit(&edited, &["dev", "test"]), edited);
    }

    /// rdc's own "nothing to promote yet" note is not user content: once a
    /// second env exists it becomes the first drafts.
    #[test]
    fn the_single_env_note_becomes_drafts_when_a_second_env_arrives() {
        let file = pipeline(&["dev"]);
        assert!(file.contains("# No deploy buttons"));

        let out = reinit(&file, &["dev", "test"]);
        assert!(!out.contains("# No deploy buttons"), "{out}");
        assert!(out.contains("\"deploy:dev\":") && out.contains("\"deploy:test\":"));

        // and a project that still has one env keeps the note, unchanged
        assert_eq!(reinit(&file, &["dev"]), file);
    }

    /// A finished job may have been moved out of the region entirely; drafting
    /// it again would produce a duplicate YAML key.
    #[test]
    fn a_job_outside_the_region_suppresses_its_draft() {
        let file = pipeline(&["dev"]);
        let file = format!(
            "{file}\n\"deploy:test\":\n  extends: .rdc-deploy\n  variables:\n    \
             RDC_ENV: \"test\"\n    RDC_SRC: \"dev\"\n"
        );
        let out = reinit(&file, &["dev", "test"]);
        assert_eq!(out.matches("\"deploy:test\":").count(), 1, "{out}");
        // dev has never been drafted here, so it still gets its draft
        assert!(out.contains("\"deploy:dev\":"));
    }

    /// Dropping an env from `rdc.toml` prunes the derived matrix, and only that:
    /// the deploy region never loses a line.
    #[test]
    fn dropping_an_env_prunes_the_matrix_but_not_its_deploy_job() {
        let file = pipeline(&["dev", "test"]);
        let out = reinit(&file, &["dev"]);
        assert!(!out.contains("- RDC_ENV: \"test\""), "the matrix is fully derived: {out}");
        assert!(out.contains("\"deploy:test\":"), "nothing here is ever removed: {out}");
    }

    #[test]
    fn has_deploy_job_reads_every_key_spelling_and_ignores_comments() {
        assert!(has_deploy_job("\"deploy:dev\":\n  extends: .rdc-deploy\n", "dev"));
        assert!(has_deploy_job("'deploy:dev':\n", "dev"));
        assert!(has_deploy_job("deploy:dev:\n", "dev"));
        // commented out == removed
        assert!(!has_deploy_job("# \"deploy:dev\":\n", "dev"));
        // and a longer env name is not a prefix match
        assert!(!has_deploy_job("\"deploy:dev-eu\":\n", "dev"));
    }

    /// The archive region on disk is how the merge knows which envs have
    /// already been offered a draft, so reading it back must be exact.
    #[test]
    fn archived_envs_reads_the_matrix_back() {
        let got = archived_envs(&pipeline(&["dev", "prod-eu"]));
        assert_eq!(got.iter().map(String::as_str).collect::<Vec<_>>(), ["dev", "prod-eu"]);
        // no archive region at all -> nothing is known to have been offered
        assert!(archived_envs("stages:\n  - test\n").is_empty());
        // a hand-written entry, unquoted and commented
        let hand = "# >>> rdc:archive-envs\n- RDC_ENV: dev  # ours\n# <<< rdc:archive-envs\n";
        assert!(archived_envs(hand).contains("dev"));
    }

    #[test]
    fn yaml_unquote_round_trips_yaml_quote() {
        for name in ["dev", "prod-eu", "we ird", "a\"b", "a\\b", "a#b"] {
            assert_eq!(yaml_unquote(&yaml_quote(name)), name, "{name}");
        }
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
            regions::splice(template, &rendered, regions::YAML).unwrap().unwrap(),
            template,
            "templates/gitlab-ci.yml's regions differ from render_regions(dev, prod, test)"
        );
    }

    /// The committed pipeline floats on `latest`, and carries no version
    /// literal at all.
    ///
    /// Both halves are deliberate. A scaffold's job is to install a working rdc
    /// on a fresh project; a project that wants the release decided by a commit
    /// sets `RDC_RELEASE` to `tags/vX.Y.Z` itself. And with no literal
    /// anywhere, `.github/scripts/bump-version.sh` has nothing to rewrite in
    /// this file -- it edits three files, not four -- so no comment here can be
    /// silently falsified by a release.
    #[test]
    fn the_committed_template_floats_and_names_no_version() {
        let template = crate::cli::init::GITLAB_CI_TEMPLATE;
        let pins: Vec<&str> = template
            .lines()
            .filter_map(|line| line.strip_prefix("  RDC_RELEASE: "))
            .collect();
        assert_eq!(pins, ["\"latest\""], "the committed RDC_RELEASE must be \"latest\"");
        assert!(
            !template.contains(env!("CARGO_PKG_VERSION")),
            "the full version literal must not appear in templates/gitlab-ci.yml; \
             a release would falsify whatever prose carries it"
        );
    }
}
