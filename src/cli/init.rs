use crate::config::{EnvConfig, ProjectConfig, ProjectMeta};
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

pub async fn run(name: &str, env_specs: &[String]) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    if cfg_path.exists() {
        return Err(anyhow!(
            "directory is already initialized as an rdc project (rdc.toml exists at {})",
            cfg_path.display()
        ));
    }

    let mut envs = BTreeMap::new();
    for spec in env_specs {
        let (env_name, env_cfg) = parse_env_spec(spec)?;
        envs.insert(env_name, env_cfg);
    }

    let cfg = ProjectConfig {
        project: ProjectMeta { name: name.to_string() },
        envs: envs.clone(),
    };
    cfg.save(&cfg_path)?;

    write_gitignore(&cwd)?;
    std::fs::create_dir_all(cwd.join("secrets"))
        .with_context(|| format!("creating {}", cwd.join("secrets").display()))?;
    for env in envs.keys() {
        let env_dir = cwd.join("envs").join(env);
        std::fs::create_dir_all(&env_dir)
            .with_context(|| format!("creating {}", env_dir.display()))?;
        std::fs::create_dir_all(env_dir.join("hooks"))
            .with_context(|| format!("creating {}", env_dir.join("hooks").display()))?;
    }

    println!(
        "Initialized rdc project '{name}' with envs: {}",
        envs.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

fn parse_env_spec(spec: &str) -> Result<(String, EnvConfig)> {
    let (env_name, rest) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': expected `<env>=<api_base>:<org_id>`"))?;
    let last_colon = rest
        .rfind(':')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': missing :<org_id>"))?;
    let api_base = &rest[..last_colon];
    let org_id_str = &rest[last_colon + 1..];
    let org_id: u64 = org_id_str
        .parse()
        .with_context(|| format!("parsing org_id '{org_id_str}' in spec '{spec}'"))?;
    Ok((
        env_name.to_string(),
        EnvConfig {
            api_base: api_base.to_string(),
            org_id,
            workspace_filter: None,
        },
    ))
}

fn write_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let body = "/target\n/secrets\n/.rdc/cache\n";
    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if existing.contains("/secrets") && existing.contains("/.rdc/cache") {
            return Ok(());
        }
        let mut combined = existing;
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(body);
        write_atomic(&path, combined.as_bytes())?;
    } else {
        write_atomic(&path, body.as_bytes())?;
    }
    Ok(())
}
