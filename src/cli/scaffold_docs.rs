//! Renders the generated regions of the two scaffolded Markdown docs.
//!
//! Only facts that come from `rdc.toml` go in here, because `rdc init` — the
//! command that refreshes these regions — is also the command that changes
//! `rdc.toml`. Remote structure (workspaces, queues, hooks) deliberately stays
//! out: `envs/<env>/_index.md` already carries it and is regenerated on every
//! sync, so a copy here would be a second, staler answer to one question.

use crate::config::EnvConfig;
use std::collections::BTreeMap;

/// Table of every env with its API base, org id, and credential suffix.
pub const REGION_ENVS: &str = "rdc:envs";
/// The promote walkthrough, naming this project's own envs.
pub const REGION_PROMOTE: &str = "rdc:promote";

pub fn render_doc_regions(envs: &BTreeMap<String, EnvConfig>) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (REGION_ENVS, render_envs(envs)),
        (REGION_PROMOTE, render_promote(envs)),
    ])
}

fn render_envs(envs: &BTreeMap<String, EnvConfig>) -> String {
    if envs.is_empty() {
        return "_No environments defined yet. Add one with \
                `rdc init --env <env>=<api_base>:<org_id>`._"
            .to_string();
    }
    let mut out =
        String::from("| Env | API base | Org id | Credential suffix |\n|---|---|---|---|\n");
    for (name, cfg) in envs {
        out.push_str(&format!(
            "| `{}` | `{}` | {} | `{}` |\n",
            md_cell(name),
            md_cell(&cfg.api_base),
            cfg.org_id,
            md_cell(&crate::secrets::env_var_suffix(name)),
        ));
    }
    // The suffix rule is the thing people derive wrong by hand (dev-us -> DEV_US).
    out.push_str(
        "\nCredentials for an env are read from `RDC_TOKEN_<suffix>`, or from \
         `RDC_USER_<suffix>` + `RDC_PASS_<suffix>` when rdc should exchange a login \
         for a token itself. A validated token is cached in \
         `secrets/<env>.secrets.json`, and hook secret values live in \
         `secrets/<env>.hook-secrets.json` — both gitignored.",
    );
    out.trim_end().to_string()
}

fn render_promote(envs: &BTreeMap<String, EnvConfig>) -> String {
    let mut names = envs.keys();
    let (Some(src), Some(tgt)) = (names.next(), names.next()) else {
        return "Promoting needs a second env. Add one with \
                `rdc init --env <env>=<api_base>:<org_id>`, and this section fills in \
                with the real commands."
            .to_string();
    };
    format!(
        "1. `rdc sync {src}` and `rdc sync {tgt}` so both lockfiles are populated.\n\
         2. `rdc migrate {src} {tgt} --dry-run` — preview the local file transform.\n\
         3. `rdc migrate {src} {tgt}` — copy {src}'s snapshot into `envs/{tgt}/`, renaming\n   \
            slugs per `.rdc/mapping.toml` (one hand-editable file where each env names its\n   \
            own slug for an object; identical slugs need no entry), rewriting portable\n   \
            `rdc://` refs, and applying {tgt}'s `overlay.toml`.\n\
         4. Review the result with `git diff`, then `rdc sync {tgt}` to push — sync creates\n   \
            missing objects in dependency order."
    )
}

/// Escape a value so it cannot break out of a Markdown table cell. An env name
/// or `api_base` reaches us from `--env` or a hand-written `rdc.toml`, neither
/// of which validates them.
fn md_cell(s: &str) -> String {
    s.replace('|', r"\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvConfig;

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
    fn env_table_carries_api_base_org_and_credential_suffix() {
        let r = render_doc_regions(&envs(&["dev", "dev-us"]));
        let table = &r[REGION_ENVS];
        assert!(table.contains("| Env | API base | Org id | Credential suffix |"));
        assert!(table.contains("| `dev` | `https://example.rossum.app/api/v1` | 100 | `DEV` |"));
        // the suffix is the thing people derive wrong by hand
        assert!(table.contains("| `dev-us` | `https://example.rossum.app/api/v1` | 101 | `DEV_US` |"));
        assert!(table.contains("RDC_TOKEN_<suffix>"));
    }

    #[test]
    fn env_table_escapes_a_pipe_so_it_cannot_break_the_table() {
        let mut e = envs(&["dev"]);
        e.get_mut("dev").unwrap().api_base = "https://example.rossum.app/a|b".to_string();
        let table = &render_doc_regions(&e)[REGION_ENVS];
        assert!(table.contains(r"a\|b"), "{table}");
    }

    #[test]
    fn promote_recipe_names_the_first_two_envs() {
        let r = render_doc_regions(&envs(&["dev", "prod", "test"]));
        let promote = &r[REGION_PROMOTE];
        assert!(promote.contains("rdc migrate dev prod --dry-run"));
        assert!(promote.contains("rdc sync prod"));
        assert!(!promote.contains("<src>"), "placeholders must be resolved: {promote}");
    }

    #[test]
    fn a_single_env_says_promotion_needs_a_second() {
        let r = render_doc_regions(&envs(&["dev"]));
        assert!(r[REGION_PROMOTE].contains("second env"));
        assert!(!r[REGION_PROMOTE].contains("rdc migrate dev"));
    }

    #[test]
    fn no_envs_still_renders_both_regions() {
        // A hand-emptied rdc.toml must not produce a broken table.
        let r = render_doc_regions(&BTreeMap::new());
        assert!(r[REGION_ENVS].contains("No environments"));
        assert!(r[REGION_PROMOTE].contains("second env"));
    }

    #[test]
    fn region_bodies_never_start_or_end_with_a_blank_line() {
        for body in render_doc_regions(&envs(&["dev", "test"])).values() {
            assert!(!body.starts_with('\n') && !body.ends_with('\n'), "{body:?}");
        }
    }
}
