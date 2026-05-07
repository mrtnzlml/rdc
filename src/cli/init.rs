use crate::config::{EnvConfig, ProjectConfig, ProjectMeta};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::Path;

pub async fn run(name: Option<String>, env_specs: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    if cfg_path.exists() {
        return Err(anyhow!(
            "directory is already initialized as an rdc project (rdc.toml exists at {})",
            cfg_path.display()
        ));
    }

    let (name, env_specs) = resolve_name_and_envs(name, env_specs)?;

    let mut envs = BTreeMap::new();
    for spec in &env_specs {
        let (env_name, env_cfg) = parse_env_spec(spec)?;
        envs.insert(env_name, env_cfg);
    }

    let cfg = ProjectConfig {
        project: ProjectMeta { name: name.clone() },
        envs: envs.clone(),
    };
    cfg.save(&cfg_path)?;

    write_gitignore(&cwd)?;
    std::fs::create_dir_all(cwd.join("secrets"))
        .with_context(|| format!("creating {}", cwd.join("secrets").display()))?;
    for env in envs.keys() {
        let paths = Paths::for_env(&cwd, env);
        std::fs::create_dir_all(paths.env_root())
            .with_context(|| format!("creating {}", paths.env_root().display()))?;
        std::fs::create_dir_all(paths.hooks_dir())
            .with_context(|| format!("creating {}", paths.hooks_dir().display()))?;
    }

    let env_list = envs.keys().cloned().collect::<Vec<_>>().join(", ");
    println!("Initialized rdc project '{name}' with envs: {env_list}");
    println!();
    println!("Next steps:");
    for env in envs.keys() {
        let upper = env.to_uppercase();
        println!("  • Set the API token for env '{env}':");
        println!("      rdc auth {env} --token <token>     # validates + writes secrets/{env}.secrets.json");
        println!("      # or: export RDC_TOKEN_{upper}=<token>");
    }
    for env in envs.keys() {
        println!("  • Pull a snapshot:  rdc pull {env}");
    }
    Ok(())
}

/// If both `name` and at least one env spec are provided, use them as-is.
/// Otherwise prompt interactively (TTY only). Without a TTY, fail with a
/// usage hint so CI gets a clear error rather than blocking on stdin.
fn resolve_name_and_envs(
    name: Option<String>,
    env_specs: Vec<String>,
) -> Result<(String, Vec<String>)> {
    if let (Some(n), false) = (&name, env_specs.is_empty()) {
        return Ok((n.clone(), env_specs));
    }
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "rdc init: --name and at least one --env are required when stdin is not a TTY. \
Example: rdc init --name myproj --env dev=https://api.elis.rossum.ai/v1:123456"
        ));
    }
    let name = match name {
        Some(n) => n,
        None => {
            let n = prompt("Project name: ")?;
            if n.is_empty() {
                return Err(anyhow!("project name cannot be empty"));
            }
            n
        }
    };
    let mut env_specs = env_specs;
    if env_specs.is_empty() {
        println!();
        println!("Define one or more environments. Press Enter on the env name to finish.");
        loop {
            let env_name = prompt("\n  env name: ")?;
            if env_name.is_empty() {
                break;
            }
            let api_base = prompt("  api_base (e.g. https://api.elis.rossum.ai/v1): ")?;
            if api_base.is_empty() {
                return Err(anyhow!("api_base cannot be empty for env '{env_name}'"));
            }
            let org_id = prompt("  org_id: ")?;
            if org_id.is_empty() {
                return Err(anyhow!("org_id cannot be empty for env '{env_name}'"));
            }
            // Light validation: ensure it parses as u64 here too so we
            // surface the error before scaffolding starts.
            org_id.parse::<u64>()
                .with_context(|| format!("org_id '{org_id}' must be a positive integer"))?;
            env_specs.push(format!("{env_name}={api_base}:{org_id}"));
        }
        if env_specs.is_empty() {
            return Err(anyhow!("at least one env is required"));
        }
    }
    Ok((name, env_specs))
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush().context("flushing stdout")?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).context("reading stdin")?;
    Ok(buf.trim().to_string())
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
