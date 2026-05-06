use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::secrets::resolve_token;
use crate::slug::slugify_unique;
use crate::snapshot::hook::write_hook;
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)
        .with_context(|| format!("loading project config from {}", cfg_path.display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token);

    let hooks = client
        .list_hooks()
        .await
        .with_context(|| format!("listing hooks for env '{env}'"))?;

    let env_root = cwd.join("envs").join(env);
    let hooks_dir = env_root.join("hooks");
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating {}", hooks_dir.display()))?;

    let lockfile_path = cwd
        .join(".rdc")
        .join("state")
        .join(format!("{env}.lock.json"));
    let mut lockfile = Lockfile::load(&lockfile_path)?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    for hook in &hooks {
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        write_hook(&hooks_dir, &slug, hook)
            .with_context(|| format!("writing hook '{}' to disk", hook.name))?;

        lockfile.upsert(
            "hooks",
            &slug,
            ObjectEntry {
                id: hook.id,
                modified_at: hook
                    .extra
                    .get("modified_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            },
        );
    }

    lockfile.save(&lockfile_path)?;

    println!("Pulled {} hooks from env '{env}'", hooks.len());
    Ok(())
}
