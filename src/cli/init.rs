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
    write_claude_md(&cwd)?;
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

/// Write an agent guide at `<root>/CLAUDE.md`. Unlike `_index.md`, this
/// is a once-only file — `rdc init` creates it, but pull/push never
/// overwrite it. Existing files (e.g. when re-running init on an
/// already-bootstrapped repo, or when the user has hand-edited the
/// guide) are left untouched.
fn write_claude_md(root: &Path) -> Result<()> {
    let path = root.join("CLAUDE.md");
    if path.exists() {
        return Ok(());
    }
    write_atomic(&path, CLAUDE_MD_TEMPLATE.as_bytes())?;
    Ok(())
}

const CLAUDE_MD_TEMPLATE: &str = r#"# Agent guide

This project is managed with **rdc** (Rossum Deployment as Code). It
snapshots a Rossum.ai tenant's configuration (hooks, queues, schemas,
…) as plain files so they can be reviewed, edited, and deployed like
code.

## Where to look first

- **`envs/<env>/_index.md`** — inventory of every object in `<env>`
  with its on-disk path, human name, type-specific signals, and the
  related objects it points at (or that point at it). Start here when
  you need to find something or understand the shape of an env.
  Regenerated on every `rdc pull` / `rdc push` — never hand-edit.
- **`rdc.toml`** — project name and per-env API base URL + org id.

## Repo layout

```
rdc.toml                                  project + env definitions
secrets/<env>.secrets.json                API tokens (gitignored)
envs/<env>/
  _index.md                               auto-regenerated; do not edit
  organization.json
  overlay.toml                            optional per-env field overrides
  workspaces/<ws>/
    workspace.json
    queues/<q>/
      queue.json
      schema.json
      formulas/<field_id>.py              extracted from schema content
      inbox.json
      email-templates/<slug>.json
  hooks/<slug>.json                       + sibling <slug>.py for function hooks
  rules/<slug>.json
  labels/<slug>.json
  engines/<slug>.json
  engine-fields/<slug>.json
  workflows/<slug>.json
  workflow-steps/<slug>.json
  mdh/<dataset>/                          Master Data Hub (if enabled)
.rdc/
  state/<env>.lock.json                   slug↔id + base hashes; never edit
  map/<src>→<tgt>.toml                    cross-env slug mappings
```

## Editing recipes

| To change… | Edit | Then run |
|---|---|---|
| Hook code | `envs/<env>/hooks/<slug>.py` | `rdc push <env>` |
| Hook config (events, queues, name) | `envs/<env>/hooks/<slug>.json` | `rdc push <env>` |
| Schema fields | `envs/<env>/workspaces/<ws>/queues/<q>/schema.json` | `rdc push <env>` |
| A formula | `envs/<env>/workspaces/<ws>/queues/<q>/formulas/<field>.py` | `rdc push <env>` |
| Queue settings | `envs/<env>/workspaces/<ws>/queues/<q>/queue.json` | `rdc push <env>` |
| Rule logic | `envs/<env>/rules/<slug>.json` | `rdc push <env>` |
| Label name / colour | `envs/<env>/labels/<slug>.json` | `rdc push <env>` |
| Email template | `envs/<env>/workspaces/<ws>/queues/<q>/email-templates/<slug>.json` | `rdc push <env>` |
| Per-env override only | `envs/<env>/overlay.toml` | `rdc push <env>` |

## Adding a new object

Create the JSON (and `.py` if the kind has executable code) under the
right directory. `rdc push <env>` detects files with no lockfile entry,
POSTs them, and writes the server-assigned `id` / `url` back into the
local file. Cross-references must use URLs already known to the
lockfile (e.g. a new hook's `queues` field must point at queues that
already exist on the remote).

## Common commands

- `rdc pull <env>` — fetch remote into local snapshot
- `rdc push <env>` — push local edits back; creates new objects too
- `rdc diff <env>` — local-vs-remote diff (no writes)
- `rdc diff <a> <b>` — diff two local snapshots
- `rdc status [<env>]` — auth + lockfile health
- `rdc map <env>` — realign stale slugs after a remote rename
- `rdc map <src> <tgt>` — auto-match for cross-env deploys
- `rdc plan --from <src> --to <tgt>` — preview a deploy
- `rdc apply --from <src> --to <tgt>` — execute a deploy
- `rdc repair --rebuild-lock <env>` — recover a corrupted lockfile

## Conflicts & drift

A three-way merge (local · base · remote) runs on every pull. When
both sides have diverged, `rdc pull` prompts an inline resolver:
`[k]eep local · [r]emote · [e]dit · [s]kip · [a]bort`. In non-TTY
(CI / `--yes`) mode, conflicts produce a `<file>.remote` shadow and
keep the local on disk.

`rdc push` runs the same drift check before each PATCH. The prompt is
`[k]` (force-push), `[r]` (adopt remote), `[s]` (skip), `[a]` (abort).

## Cross-env deploys (e.g. dev → prod)

1. `rdc pull dev` and `rdc pull prod` so both lockfiles are populated.
2. `rdc map dev prod` — auto-matches by slug, writes
   `.rdc/map/dev→prod.toml`.
3. Hand-edit the mapping for renames or skips.
4. `rdc plan --from dev --to prod` — shows what will change without
   PATCHing.
5. `rdc apply --from dev --to prod` — executes. Per-env values stay in
   each env's `overlay.toml`.

## What NOT to edit

- `_index.md` (auto-regenerated by every pull/push)
- `.rdc/state/<env>.lock.json` (slug↔id and base hashes; rdc owns this)
- `*.remote` files (shadow files from conflict resolution; either
  consume the changes or delete the file)
- Contents of `secrets/` (gitignored API tokens)
"#;
