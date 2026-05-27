use anyhow::Result;

/// Offline realign: rename any local file whose slug no longer matches
/// its JSON `name` field. Delegates to the existing within-env realign
/// flow. No API calls.
pub async fn run(env: &str, check: bool, yes: bool) -> Result<()> {
    // Delegates to the existing within-env realign flow. Stays offline:
    // no pull, no API call. The realign module knows the cascade rules
    // (workspace rename moves the whole queues subtree, etc.).
    crate::cli::deploy::realign::run_within_env(env, check, yes).await
}
