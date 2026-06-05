//! Pre-flight + body-shaping for hook secrets during `rdc deploy`.
//!
//! Two-step contract:
//!
//! 1. **Pre-flight** (this module's [`precheck`]). Before any write hits
//!    the target, GET `secrets_keys` on each source hook in the deploy
//!    plan and confirm the target's local `secrets/<tgt>.hook-secrets.json`
//!    declares a value for every key. Missing values abort the deploy
//!    with a per-hook table — and crucially, the precheck first
//!    pre-populates the file with empty placeholders for every required
//!    key (preserving anything the user already filled in), so the
//!    rerun is a fill-in-the-blanks loop instead of a JSON-shape
//!    scavenger hunt. Keys present in the target file but not in
//!    source's `secrets_keys` are surfaced as warnings — the deploy
//!    proceeds, those keys are filtered out of the outbound body so the
//!    target hook gets the same shape of secrets as the source has.
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
use crate::secrets::{write_hook_secrets_template, HookSecrets};
use crate::state::Lockfile;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

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
/// Returns the filtered injection plan on success; on missing values it
/// pre-populates `secrets/<tgt_env>.hook-secrets.json` with empty
/// placeholders (preserving anything the user already filled in) and
/// aborts with an actionable per-hook report pointing at the file.
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
    tgt_root: &Path,
) -> Result<HookSecretsPlan> {
    let mut plan = HookSecretsPlan::default();
    // (slug, sorted missing key names). Collected so the abort message
    // can render every gap in one shot rather than failing on the first.
    let mut missing: Vec<(String, Vec<String>)> = Vec::new();
    // Full required-key list for every slug whose src hook declares any
    // secrets — used to pre-populate the target's hook-secrets file
    // when the abort fires. We collect even for slugs that pass the
    // diff so the template stays a complete inventory of what's in
    // scope.
    let mut required_per_slug: BTreeMap<String, Vec<String>> = BTreeMap::new();

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

        // Record the full required-key list so a later template-write
        // can render the complete shape, not just the missing-key
        // subset.
        required_per_slug.insert(slug.clone(), required.clone());

        let empty = BTreeMap::<String, String>::new();
        let tgt_kv = tgt_secrets.for_slug(&slug).unwrap_or(&empty);
        let diff = diff_for_slug(&required, tgt_kv);

        if !diff.missing.is_empty() {
            missing.push((slug.clone(), diff.missing));
            continue;
        }
        if !diff.extras.is_empty() {
            let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
            log.event(
                crate::log::Action::Warn,
                &format!(
                    "hook/{}: target secrets file has {} extra key(s) not declared by src — ignored: {}",
                    slug,
                    diff.extras.len(),
                    diff.extras.join(", "),
                ),
            );
        }
        plan.per_slug.insert(slug, diff.filtered);
    }

    if !missing.is_empty() {
        // Pre-populate the target's hook-secrets file with every required
        // key the user still needs to fill in — existing values stay put
        // (`write_hook_secrets_template` merges, never wipes). The deploy
        // still aborts, but the user gets a fill-in-the-blanks form
        // instead of a blank page.
        let template_path =
            write_hook_secrets_template(tgt_root, tgt_env, &required_per_slug, tgt_secrets)
                .with_context(|| {
                    format!("pre-populating hook-secrets template for env '{tgt_env}'")
                })?;
        bail!(
            "{}",
            format_missing_keys_message(tgt_env, &missing, Some(&template_path.display().to_string()))
        );
    }

    Ok(plan)
}

/// The user-facing message printed when one or more target hooks lack
/// values for keys declared on the source. Extracted so the format can
/// be asserted in unit tests without spinning up a mock HTTP server.
///
/// `template_path` is `Some(path)` when the precheck pre-populated the
/// target's hook-secrets file with empty placeholders for the missing
/// keys — the trailing line then points the user at that file instead
/// of asking them to invent the JSON shape from scratch. `None` is
/// reserved for callers that haven't done the template write (tests).
pub fn format_missing_keys_message(
    tgt_env: &str,
    missing: &[(String, Vec<String>)],
    template_path: Option<&str>,
) -> String {
    let mut lines = Vec::with_capacity(missing.len() + 2);
    lines.push(format!(
        "deploy refused: target env '{tgt_env}' is missing secret values for:"
    ));
    for (slug, keys) in missing {
        lines.push(format!("  - hooks/{slug:30}{}", keys.join(", ")));
    }
    match template_path {
        Some(p) => lines.push(format!(
            "pre-populated {p} (mode 0600, gitignored) with empty placeholders — fill in the missing values and retry."
        )),
        None => lines.push(format!(
            "populate secrets/{tgt_env}.hook-secrets.json (mode 0600, gitignored) and retry."
        )),
    }
    lines.join("\n")
}

/// Pure comparison between source's `secrets_keys` and the target's
/// local K/V map. Returns either the filtered K/V to inject OR the
/// sorted list of keys missing from target. Used by [`precheck`] for
/// real deploys and by unit tests to lock the contract.
///
/// A value equal to [`crate::secrets::UNFILLED_SENTINEL`] is treated as
/// "key present but not yet filled in" — the precheck refuses the
/// deploy, the filtered injection map excludes it, so the sentinel
/// never reaches the Rossum API. Other string values (including `""`
/// and `null` once we add that support) are deliberate user settings.
pub fn diff_for_slug(
    required: &[String],
    tgt_kv: &BTreeMap<String, String>,
) -> SlugDiff {
    use crate::secrets::UNFILLED_SENTINEL;

    let required_set: BTreeSet<&str> = required.iter().map(String::as_str).collect();
    let filled_set: BTreeSet<&str> = tgt_kv
        .iter()
        .filter(|(_, v)| v.as_str() != UNFILLED_SENTINEL)
        .map(|(k, _)| k.as_str())
        .collect();
    let all_keys_set: BTreeSet<&str> = tgt_kv.keys().map(String::as_str).collect();

    let mut missing: Vec<String> = required_set
        .difference(&filled_set)
        .map(|s| (*s).to_string())
        .collect();
    missing.sort();

    let mut extras: Vec<String> = all_keys_set
        .difference(&required_set)
        .map(|s| (*s).to_string())
        .collect();
    extras.sort();

    let filtered: BTreeMap<String, String> = required
        .iter()
        .filter_map(|k| {
            tgt_kv
                .get(k)
                .filter(|v| v.as_str() != UNFILLED_SENTINEL)
                .map(|v| (k.clone(), v.clone()))
        })
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
    fn diff_sentinel_value_counts_as_missing_not_filled() {
        // The bug this regression catches: the pre-populated template
        // used to write `""` for unfilled keys, then the precheck saw
        // the key as "present" and let the second run pass. Now the
        // template writes `UNFILLED_SENTINEL`, which `diff_for_slug`
        // must treat the same as a missing key.
        let d = diff_for_slug(
            &["password".into(), "type".into()],
            &kv(&[
                ("password", crate::secrets::UNFILLED_SENTINEL),
                ("type", crate::secrets::UNFILLED_SENTINEL),
            ]),
        );
        assert_eq!(d.missing, vec!["password".to_string(), "type".to_string()]);
        assert!(
            d.filtered.is_empty(),
            "sentinel-valued keys must never reach the injection map"
        );
    }

    #[test]
    fn diff_mixed_sentinel_and_filled_only_missing_for_sentinel() {
        let d = diff_for_slug(
            &["password".into(), "type".into()],
            &kv(&[
                ("password", "real-value"),
                ("type", crate::secrets::UNFILLED_SENTINEL),
            ]),
        );
        assert_eq!(d.missing, vec!["type".to_string()]);
        assert_eq!(d.filtered, kv(&[("password", "real-value")]));
    }

    #[test]
    fn diff_empty_string_value_counts_as_filled() {
        // Deliberate empty string is a user value (rare but allowed);
        // only the sentinel is treated as unfilled.
        let d = diff_for_slug(
            &["password".into()],
            &kv(&[("password", "")]),
        );
        assert!(d.missing.is_empty());
        assert_eq!(d.filtered, kv(&[("password", "")]));
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
            Some("/proj/secrets/prod.hook-secrets.json"),
        );
        assert!(msg.contains("deploy refused"));
        assert!(msg.contains("target env 'prod'"));
        assert!(msg.contains("hooks/master-data-hub"));
        assert!(msg.contains("mdh_api_token"));
        assert!(msg.contains("hooks/notify-slack"));
        assert!(msg.contains("signing_secret"));
        assert!(msg.contains("webhook_id"));
        assert!(msg.contains("/proj/secrets/prod.hook-secrets.json"));
        assert!(
            msg.contains("pre-populated") && msg.contains("empty placeholders"),
            "message must hint that the file already has the right shape: {msg}"
        );
    }

    #[test]
    fn format_missing_keys_message_falls_back_when_no_template_path() {
        // Defensive form — callers that haven't pre-populated still get a
        // useful message pointing at the conventional path.
        let msg = format_missing_keys_message(
            "test-eu",
            &[("h".to_string(), vec!["k".to_string()])],
            None,
        );
        assert!(msg.contains("secrets/test-eu.hook-secrets.json"));
        assert!(msg.contains("populate"));
    }
}
