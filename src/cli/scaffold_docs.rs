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
/// The `rdc sync <env>` command list, one line per env (README only).
pub const REGION_SYNC: &str = "rdc:sync";

/// Every doc region rdc can fill. A file need not declare all of them —
/// `CLAUDE.md` has no `rdc:sync` — but a name it *does* declare must be in
/// here, or [`crate::cli::regions::splice`] rejects the file as a typo.
pub fn render_doc_regions(envs: &BTreeMap<String, EnvConfig>) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (REGION_ENVS, render_envs(envs)),
        (REGION_PROMOTE, render_promote(envs)),
        (REGION_SYNC, render_sync(envs)),
    ])
}

/// One `rdc sync <env>` per env. Generated rather than static because it is
/// env-derived, and an env-derived line outside a region never updates again
/// (only the regions are spliced on an existing file).
fn render_sync(envs: &BTreeMap<String, EnvConfig>) -> String {
    if envs.is_empty() {
        return "_No environments defined yet. Add one with \
                `rdc init --env <env>=<api_base>:<org_id>`._"
            .to_string();
    }
    let mut out = String::from("```sh\n");
    for name in envs.keys() {
        out.push_str(&format!("rdc sync {name}\n"));
    }
    out.push_str("```");
    out
}

fn render_envs(envs: &BTreeMap<String, EnvConfig>) -> String {
    if envs.is_empty() {
        return "_No environments defined yet. Add one with \
                `rdc init --env <env>=<api_base>:<org_id>`._"
            .to_string();
    }
    // The index column names the real path, so an agent opens it without
    // first resolving `<env>` itself.
    let mut out = String::from(
        "| Env | API base | Org id | Credential suffix | Object index |\n|---|---|---|---|---|\n",
    );
    for (name, cfg) in envs {
        out.push_str(&format!(
            "| `{}` | `{}` | {} | `{}` | `envs/{}/_index.md` |\n",
            md_cell(name),
            md_cell(&cfg.api_base),
            cfg.org_id,
            md_cell(&crate::secrets::env_var_suffix(name)),
            md_cell(name),
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
    // Leads with the chain because that is how a promotion is actually run —
    // and names its destructive half plainly, because `--mirror` plus
    // `--allow-deletes` is exactly what the pipeline's deploy button presses
    // unattended. A reader who learns an additive recipe here and then presses
    // that button has been taught the wrong workflow.
    format!(
        "The usual promotion is one chain:\n\
         \n\
         ```sh\n\
         rdc sync {src} && rdc migrate {src} {tgt} --mirror && rdc sync {tgt} --allow-deletes\n\
         ```\n\
         \n\
         - `rdc sync {src}` refreshes the source snapshot, so you promote {src}'s\n  \
         current state rather than a stale one.\n\
         - `rdc migrate {src} {tgt} --mirror` copies {src}'s snapshot into\n  \
         `envs/{tgt}/` — renaming slugs per `.rdc/mapping.toml` (one hand-editable\n  \
         file where each env names its own slug for an object; identical slugs need\n  \
         no entry), rewriting portable `rdc://` refs, applying {tgt}'s\n  \
         `overlay.toml` — and, because of `--mirror`, pruning target files {src} no\n  \
         longer has. Still zero remote calls.\n\
         - `rdc sync {tgt} --allow-deletes` pushes the result: it creates missing\n  \
         objects in dependency order, and turns those pruned files into remote\n  \
         deletions.\n\
         \n\
         **The chain deletes.** `--mirror` plus `--allow-deletes` removes objects from\n\
         {tgt} that {src} does not have. Rehearse it before the first run against a\n\
         new target:\n\
         \n\
         ```sh\n\
         rdc migrate {src} {tgt} --mirror --dry-run\n\
         rdc sync {tgt} --dry-run\n\
         ```\n\
         \n\
         On a real run, stop between the migrate and the sync and read `git diff` —\n\
         that diff is exactly what the sync will push.\n\
         \n\
         The pipeline's `deploy:{tgt}` button runs the same two writing steps with\n\
         `--yes`; it skips `rdc sync {src}` only because the scheduled `archive` job\n\
         keeps the committed snapshot current.\n\
         \n\
         Additive alternative: drop `--mirror` and `--allow-deletes` when {tgt}\n\
         legitimately holds objects {src} lacks — nothing is then pruned locally or\n\
         deleted remotely.\n\
         \n\
         **Renaming an object.** Rename it in {src} (or let a tenant-side rename\n\
         arrive with the next sync), then run `rdc doctor {src}`: it realigns the\n\
         local slug and records the new name in `.rdc/mapping.toml`, so the\n\
         promotion above renames {tgt}'s object instead of deleting it and creating\n\
         a replacement. Renaming a queue any other way costs it its documents. If a\n\
         rename ever reaches `--mirror` unrecorded, migrate refuses and says which\n\
         row to add."
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
    use crate::cli::gitlab_ci::test_envs as envs;

    #[test]
    fn env_table_carries_api_base_org_and_credential_suffix() {
        let r = render_doc_regions(&envs(&["dev", "dev-us"]));
        let table = &r[REGION_ENVS];
        assert!(table.contains("| Env | API base | Org id | Credential suffix | Object index |"));
        assert!(table.contains(
            "| `dev` | `https://example.rossum.app/api/v1` | 100 | `DEV` | `envs/dev/_index.md` |"
        ));
        // the suffix is the thing people derive wrong by hand
        assert!(table.contains(
            "| `dev-us` | `https://example.rossum.app/api/v1` | 101 | `DEV_US` | `envs/dev-us/_index.md` |"
        ));
        assert!(table.contains("RDC_TOKEN_<suffix>"));
    }

    #[test]
    fn env_table_escapes_a_pipe_so_it_cannot_break_the_table() {
        let mut e = envs(&["dev"]);
        e.get_mut("dev").unwrap().api_base = "https://example.rossum.app/a|b".to_string();
        let table = &render_doc_regions(&e)[REGION_ENVS];
        assert!(table.contains(r"a\|b"), "{table}");
    }

    /// The recipe must teach the chain people actually run, name what it
    /// deletes, and be recognisable as the pipeline's deploy button.
    #[test]
    fn promote_recipe_leads_with_the_real_chain_and_names_the_deletes() {
        let r = render_doc_regions(&envs(&["dev", "prod", "test"]));
        let promote = &r[REGION_PROMOTE];
        // the copy-pasteable line, with this project's own envs
        assert!(
            promote.contains(
                "rdc sync dev && rdc migrate dev prod --mirror && rdc sync prod --allow-deletes"
            ),
            "{promote}"
        );
        // the destructive half, named and rehearsable
        assert!(promote.contains("`--mirror` plus `--allow-deletes` removes objects"), "{promote}");
        assert!(promote.contains("rdc migrate dev prod --mirror --dry-run"), "{promote}");
        assert!(promote.contains("rdc sync prod --dry-run"), "{promote}");
        assert!(promote.contains("`git diff`"), "{promote}");
        // same workflow as the button, and the additive escape hatch
        assert!(promote.contains("`deploy:prod` button"), "{promote}");
        assert!(promote.contains("Additive alternative"), "{promote}");
        assert!(!promote.contains("<src>"), "placeholders must be resolved: {promote}");
    }

    #[test]
    fn a_single_env_says_promotion_needs_a_second() {
        let r = render_doc_regions(&envs(&["dev"]));
        assert!(r[REGION_PROMOTE].contains("second env"));
        assert!(!r[REGION_PROMOTE].contains("rdc migrate dev"));
    }

    #[test]
    fn sync_region_lists_one_command_per_env() {
        let r = render_doc_regions(&envs(&["dev", "prod-eu"]));
        assert_eq!(r[REGION_SYNC], "```sh\nrdc sync dev\nrdc sync prod-eu\n```");
    }

    #[test]
    fn no_envs_still_renders_every_region() {
        // A hand-emptied rdc.toml must not produce a broken table -- and must
        // still fill every region a scaffold declares, because splicing a
        // template with no markers is a hard error.
        let r = render_doc_regions(&BTreeMap::new());
        assert!(r[REGION_ENVS].contains("No environments"));
        assert!(r[REGION_PROMOTE].contains("second env"));
        assert!(r[REGION_SYNC].contains("No environments"));
    }

    #[test]
    fn region_bodies_never_start_or_end_with_a_blank_line() {
        for body in render_doc_regions(&envs(&["dev", "test"])).values() {
            assert!(!body.starts_with('\n') && !body.ends_with('\n'), "{body:?}");
        }
    }
}
