//! Pre-flight + body-shaping for hook secrets during `rdc deploy`.
//!
//! Two-step contract:
//!
//! 1. **Pre-flight** (this module's [`precheck`]). Before any write hits
//!    the target, GET `secrets_keys` on each source hook in the deploy
//!    plan and confirm the target's local `secrets/<tgt>.hook-secrets.json`
//!    declares a value for every key. Missing values abort the deploy
//!    with a per-hook table; the user populates the file and reruns.
//!    Keys present in the target file but not in source's `secrets_keys`
//!    are surfaced as warnings — the deploy proceeds, those keys are
//!    filtered out of the outbound body so the target hook gets the same
//!    shape of secrets as the source has.
//!
//! 2. **Inject** (the returned [`HookSecretsPlan`]). The plan carries
//!    one filtered K/V map per slug — exactly the bytes to splice into
//!    each `POST /hooks` and `PATCH /hooks/<id>` body. Callers in
//!    `create.rs` and `apply.rs` consult the plan rather than re-reading
//!    the secrets file. Slugs with no source `secrets_keys` map to an
//!    empty value (no `secrets` field sent).
//!
//! Values themselves never appear in logs or progress output. The
//! warning lines reference key names only.

use crate::api::{anyhow_has_status, RossumClient};
use crate::secrets::HookSecrets;
use crate::state::Lockfile;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Per-slug filtered secrets ready to inject. The key set matches the
/// source hook's `secrets_keys` — extras in the target file are dropped.
#[derive(Debug, Default, Clone)]
pub struct HookSecretsPlan {
    pub per_slug: BTreeMap<String, BTreeMap<String, String>>,
}

impl HookSecretsPlan {
    /// Look up the K/V map to inject for `slug`. Returns `None` when
    /// the source hook has no `secrets_keys` (nothing to send).
    pub fn for_slug(&self, slug: &str) -> Option<&BTreeMap<String, String>> {
        self.per_slug.get(slug).filter(|m| !m.is_empty())
    }
}

/// Pre-flight check. For each source slug, fetch `secrets_keys` and
/// validate the target's local secrets file has every required value.
/// Returns the filtered injection plan on success; aborts with an
/// actionable per-hook report on missing values.
///
/// Hooks not yet POSTed to the source (no lockfile entry, no `id`) are
/// silently skipped — there's nothing to GET. The downstream create
/// path on the source would have caught that on its own sync.
pub async fn precheck(
    src_client: &RossumClient,
    src_lockfile: &Lockfile,
    tgt_secrets: &HookSecrets,
    src_slugs: impl IntoIterator<Item = String>,
    tgt_env: &str,
) -> Result<HookSecretsPlan> {
    let mut plan = HookSecretsPlan::default();
    // (slug, sorted missing key names). Collected so the abort message
    // can render every gap in one shot rather than failing on the first.
    let mut missing: Vec<(String, Vec<String>)> = Vec::new();

    for slug in src_slugs {
        let Some(entry) = src_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(&slug))
        else {
            // Not synced on src yet — nothing the API can report on.
            continue;
        };
        // A 404 here means the hook id exists on src but the endpoint
        // doesn't (test fixtures, older API versions, hooks that were
        // just deleted server-side). Treat it the same as "no secrets
        // configured" — the deploy precheck doesn't need anything more.
        let required: Vec<String> = match src_client
            .get_hook_secrets_keys(entry.id, None)
            .await
        {
            Ok(v) => v,
            Err(e) if anyhow_has_status(&e, 404) => Vec::new(),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("GET /hooks/{}/secrets_keys (src '{}')", entry.id, slug)
                });
            }
        };

        if required.is_empty() {
            plan.per_slug.insert(slug, BTreeMap::new());
            continue;
        }

        let empty = BTreeMap::<String, String>::new();
        let tgt_kv = tgt_secrets.for_slug(&slug).unwrap_or(&empty);
        let diff = diff_for_slug(&required, tgt_kv);

        if !diff.missing.is_empty() {
            missing.push((slug.clone(), diff.missing));
            continue;
        }
        if !diff.extras.is_empty() {
            eprintln!(
                "! hooks/{}: target secrets file has {} extra key(s) not declared by src — ignored: {}",
                slug,
                diff.extras.len(),
                diff.extras.join(", ")
            );
        }
        plan.per_slug.insert(slug, diff.filtered);
    }

    if !missing.is_empty() {
        bail!("{}", format_missing_keys_message(tgt_env, &missing));
    }

    Ok(plan)
}

/// The user-facing message printed when one or more target hooks lack
/// values for keys declared on the source. Extracted so the format can
/// be asserted in unit tests without spinning up a mock HTTP server.
pub fn format_missing_keys_message(tgt_env: &str, missing: &[(String, Vec<String>)]) -> String {
    let mut lines = Vec::with_capacity(missing.len() + 2);
    lines.push(format!(
        "deploy refused: target env '{tgt_env}' is missing secret values for:"
    ));
    for (slug, keys) in missing {
        lines.push(format!("  - hooks/{slug:30}{}", keys.join(", ")));
    }
    lines.push(format!(
        "populate secrets/{tgt_env}.hook-secrets.json (mode 0600, gitignored) and retry."
    ));
    lines.join("\n")
}

/// Pure comparison between source's `secrets_keys` and the target's
/// local K/V map. Returns either the filtered K/V to inject OR the
/// sorted list of keys missing from target. Used by [`precheck`] for
/// real deploys and by unit tests to lock the contract.
pub fn diff_for_slug(
    required: &[String],
    tgt_kv: &BTreeMap<String, String>,
) -> SlugDiff {
    let required_set: BTreeSet<&str> = required.iter().map(String::as_str).collect();
    let tgt_set: BTreeSet<&str> = tgt_kv.keys().map(String::as_str).collect();

    let mut missing: Vec<String> = required_set
        .difference(&tgt_set)
        .map(|s| (*s).to_string())
        .collect();
    missing.sort();

    let mut extras: Vec<String> = tgt_set
        .difference(&required_set)
        .map(|s| (*s).to_string())
        .collect();
    extras.sort();

    let filtered: BTreeMap<String, String> = required
        .iter()
        .filter_map(|k| tgt_kv.get(k).map(|v| (k.clone(), v.clone())))
        .collect();

    SlugDiff { missing, extras, filtered }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SlugDiff {
    pub missing: Vec<String>,
    pub extras: Vec<String>,
    pub filtered: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn diff_all_required_present_no_extras() {
        let d = diff_for_slug(
            &["a".into(), "b".into()],
            &kv(&[("a", "1"), ("b", "2")]),
        );
        assert!(d.missing.is_empty());
        assert!(d.extras.is_empty());
        assert_eq!(d.filtered, kv(&[("a", "1"), ("b", "2")]));
    }

    #[test]
    fn diff_missing_keys_reported_sorted() {
        let d = diff_for_slug(
            &["c".into(), "a".into(), "b".into()],
            &kv(&[("a", "1")]),
        );
        assert_eq!(d.missing, vec!["b".to_string(), "c".to_string()]);
        // Only the satisfied key shows up in the filtered map.
        assert_eq!(d.filtered, kv(&[("a", "1")]));
    }

    #[test]
    fn diff_extras_are_filtered_not_sent() {
        let d = diff_for_slug(
            &["a".into()],
            &kv(&[("a", "1"), ("unused", "x"), ("also_unused", "y")]),
        );
        assert!(d.missing.is_empty());
        assert_eq!(d.extras, vec!["also_unused".to_string(), "unused".to_string()]);
        assert_eq!(
            d.filtered,
            kv(&[("a", "1")]),
            "filtered map must contain only keys required by src"
        );
    }

    #[test]
    fn diff_no_required_keys_yields_empty_filtered_no_warnings() {
        let d = diff_for_slug(&[], &kv(&[("a", "1")]));
        assert!(d.missing.is_empty());
        // Target file has unused keys, but with nothing required they
        // can't be flagged as "extras" relative to a non-existent set
        // — we want them surfaced. Adjust the expectation accordingly.
        // (`extras` is everything in tgt not in required.)
        assert_eq!(d.extras, vec!["a".to_string()]);
        assert!(d.filtered.is_empty());
    }

    #[test]
    fn format_missing_keys_message_lists_each_hook() {
        let msg = format_missing_keys_message(
            "prod",
            &[
                ("master-data-hub".to_string(), vec!["mdh_api_token".to_string()]),
                (
                    "notify-slack".to_string(),
                    vec!["signing_secret".to_string(), "webhook_id".to_string()],
                ),
            ],
        );
        assert!(msg.contains("deploy refused"));
        assert!(msg.contains("target env 'prod'"));
        assert!(msg.contains("hooks/master-data-hub"));
        assert!(msg.contains("mdh_api_token"));
        assert!(msg.contains("hooks/notify-slack"));
        assert!(msg.contains("signing_secret"));
        assert!(msg.contains("webhook_id"));
        assert!(msg.contains("secrets/prod.hook-secrets.json"));
    }
}
