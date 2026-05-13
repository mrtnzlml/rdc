use crate::config::{EnvConfig, ProjectConfig};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use std::io::{IsTerminal, Write};
use std::path::Path;

/// Whether `rdc init` is bootstrapping a new project or extending an
/// existing one by adding new env(s).
enum InitMode {
    New,
    Extend,
}

pub async fn run(env_specs: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");

    let (mut cfg, mode) = if cfg_path.exists() {
        let existing = ProjectConfig::load(&cfg_path).with_context(|| {
            format!("loading existing project config from {}", cfg_path.display())
        })?;
        (existing, InitMode::Extend)
    } else {
        (ProjectConfig::default(), InitMode::New)
    };

    // Get env specs (interactive when none provided + TTY available).
    let env_specs = resolve_env_specs(env_specs, &cfg, &mode)?;

    let mut new_env_names: Vec<String> = Vec::new();
    for spec in &env_specs {
        let (env_name, env_cfg) = parse_env_spec(spec)?;
        if cfg.envs.contains_key(&env_name) {
            return Err(anyhow!(
                "env '{env_name}' already exists in this project; \
                 to update it, edit {} directly",
                cfg_path.display()
            ));
        }
        cfg.envs.insert(env_name.clone(), env_cfg);
        new_env_names.push(env_name);
    }

    cfg.save(&cfg_path)?;

    write_gitignore(&cwd)?;
    write_claude_md(&cwd)?;
    std::fs::create_dir_all(cwd.join("secrets"))
        .with_context(|| format!("creating {}", cwd.join("secrets").display()))?;
    for env in &new_env_names {
        let paths = Paths::for_env(&cwd, env);
        std::fs::create_dir_all(paths.env_root())
            .with_context(|| format!("creating {}", paths.env_root().display()))?;
        std::fs::create_dir_all(paths.hooks_dir())
            .with_context(|| format!("creating {}", paths.hooks_dir().display()))?;
    }

    let env_list = new_env_names.join(", ");
    match mode {
        InitMode::New => {
            println!("Initialized rdc project with envs: {env_list}");
        }
        InitMode::Extend => {
            println!("Added env(s): {env_list}");
        }
    }
    println!();
    println!("Next steps:");
    for env in &new_env_names {
        let upper = env.to_uppercase();
        println!("  • Set the API token for env '{env}':");
        println!("      rdc auth {env} --token <token>     # validates + writes secrets/{env}.secrets.json");
        println!("      # or: export RDC_TOKEN_{upper}=<token>");
    }
    for env in &new_env_names {
        println!("  • Pull a snapshot:  rdc pull {env}");
    }
    Ok(())
}

/// Resolve the list of env specs to use. When `env_specs` is empty,
/// prompts interactively (TTY-only). The prompt copy differs by mode
/// so users adding to an existing project see context about what's
/// already defined.
fn resolve_env_specs(
    env_specs: Vec<String>,
    cfg: &ProjectConfig,
    mode: &InitMode,
) -> Result<Vec<String>> {
    if !env_specs.is_empty() {
        return Ok(env_specs);
    }
    if !std::io::stdin().is_terminal() {
        let example = "rdc init --env dev=https://api.elis.rossum.ai/v1:123456";
        let msg = match mode {
            InitMode::New => format!(
                "rdc init: at least one --env is required when stdin is not a TTY. \
                 Example: {example}"
            ),
            InitMode::Extend => format!(
                "rdc init: at least one --env is required when stdin is not a TTY \
                 (extending existing project). Example: {example}"
            ),
        };
        return Err(anyhow!(msg));
    }

    println!();
    match mode {
        InitMode::New => {
            println!(
                "Define one or more environments. Press Enter on the env name to finish."
            );
        }
        InitMode::Extend => {
            let existing = if cfg.envs.is_empty() {
                "(none)".to_string()
            } else {
                cfg.envs.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            println!("Adding env(s) to existing project. Existing envs: {existing}.");
            println!(
                "Define one or more new environments. Press Enter on the env name to finish."
            );
        }
    }

    let mut env_specs: Vec<String> = Vec::new();
    loop {
        let env_name = prompt("\n  env name: ")?;
        if env_name.is_empty() {
            break;
        }
        if cfg.envs.contains_key(&env_name) {
            return Err(anyhow!(
                "env '{env_name}' already exists in this project; \
                 to update it, edit rdc.toml directly"
            ));
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
        org_id
            .parse::<u64>()
            .with_context(|| format!("org_id '{org_id}' must be a positive integer"))?;
        env_specs.push(format!("{env_name}={api_base}:{org_id}"));
    }
    if env_specs.is_empty() {
        return Err(anyhow!("at least one env is required"));
    }
    Ok(env_specs)
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
  rules/<slug>.json                       + sibling <slug>.py for trigger_condition
  labels/<slug>.json
  engines/<engine_slug>/
    engine.json
    fields/<field_slug>.json                each engine field nests under its engine
  workflows/<workflow_slug>/
    workflow.json
    steps/<step_slug>.json                  each step nests under its workflow
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
| Rule's trigger condition (Python) | `envs/<env>/rules/<slug>.py` | `rdc push <env>` |
| Rule config (name, queues) | `envs/<env>/rules/<slug>.json` | `rdc push <env>` |
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

## Deleting an object

The snapshot is the declared state of the environment, including
absence. Remove the local file (`rm envs/<env>/labels/foo.json`) and
the next `rdc push <env>` will detect the tombstone — the lockfile
entry remains, signalling "this object was tracked and is now gone."

Two intentional acts are required before the DELETE hits the remote:

1. Removing the file (you did this).
2. Either answering `y` to the interactive batch prompt on a TTY, or
   passing `--allow-deletes`. `--yes` does NOT bypass — destruction
   needs its own authorisation.

In non-TTY (CI) mode, `--allow-deletes` is mandatory; without it the
push refuses with a clear list of pending tombstones.

`rdc status <env>` lists pending tombstones in a `deletes:` section.
`rdc push --dry-run` previews them without sending anything;
`--diff` adds the full remote body for each as a deleted-file diff.

Deletes run after creates / updates in reverse dependency order
(`engine_fields → engines → labels → rules → hooks →
email_templates → inboxes → queues → schemas → workspaces`) so a
queue is gone before the workspace that contained it. The Rossum
DELETE endpoint accepts 404 as success, so an object already absent
on the remote just gets its lockfile entry cleaned up.

If the remote has been modified since the last pull, an inline
resolver opens — `[k]eep delete` / `[s]kip` / `[a]bort` — so a
tombstoned delete can't silently overwrite a remote update.

## Common commands

- `rdc pull <env>` — fetch remote into local snapshot
- `rdc push <env>` — push local edits back; creates new objects too;
  `--allow-deletes` to also remove remote objects whose local files
  you've deleted
- `rdc diff <env>` — local-vs-remote diff (no writes)
- `rdc diff <a> <b>` — diff two local snapshots
- `rdc status [<env>]` — auth + lockfile health
- `rdc deploy <src> <tgt>` — promote one env to another in one shot
  (POST missing + PATCH deltas, URL rewrites included); `--dry-run`
  previews without writing
- `rdc repair <env> --rename-slugs` — realign stale local slugs after
  a remote rename (offline)
- `rdc repair <env> --rebuild-lock` — recover from a corrupted
  lockfile by re-pulling everything

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
2. `rdc deploy dev prod --dry-run` — preview the plan.
3. `rdc deploy dev prod` — execute. Per-env values stay in each env's
   `overlay.toml`; the slug-to-slug mapping is built automatically and
   stored at `.rdc/map/dev→prod.toml` (hand-edit for renames if needed).

## What NOT to edit

- `_index.md` (auto-regenerated by every pull/push)
- `.rdc/state/<env>.lock.json` (slug↔id and base hashes; rdc owns this)
- `*.remote` files (shadow files from conflict resolution; either
  consume the changes or delete the file)
- Contents of `secrets/` (gitignored API tokens)

## Known limitations

- **One schema per queue, one inbox per queue.** The Rossum API
  technically allows a single schema or inbox to be referenced by many
  queues (`schema.queues` / `inbox.queues` are arrays). rdc's on-disk
  layout assumes each queue owns its own schema and inbox — the files
  live at `workspaces/<ws>/queues/<q>/schema.json` and `inbox.json`.
  If a tenant shares a schema or inbox across queues, rdc will write
  duplicate copies (one per consuming queue) and the lockfile will
  carry an entry per queue slug pointing at the same remote id. Push
  to that shared resource works (id is the truth), but cross-env
  deploys via `rdc apply` may surface confusing diffs.
"#;
