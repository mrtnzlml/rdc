use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::slug::slugify_unique;
use crate::snapshot::hook::write_hook;
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let hooks = client
        .list_hooks()
        .await
        .with_context(|| format!("listing hooks for env '{env}'"))?;

    std::fs::create_dir_all(paths.hooks_dir())
        .with_context(|| format!("creating {}", paths.hooks_dir().display()))?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    for hook in &hooks {
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        write_hook(&paths.hooks_dir(), &slug, hook)
            .with_context(|| format!("writing hook '{}' to disk", hook.name))?;

        lockfile.upsert(
            "hooks",
            &slug,
            ObjectEntry {
                id: hook.id,
                url: Some(hook.url.clone()),
                modified_at: hook
                    .extra
                    .get("modified_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                content_hash: None,
            },
        );
    }

    lockfile.save(&paths.lockfile())?;

    println!("Pulled {} hooks from env '{env}'", hooks.len());
    Ok(())
}
