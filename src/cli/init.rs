use crate::config::{EnvConfig, ProjectConfig};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use inquire::error::InquireError;
use inquire::validator::Validation;
use inquire::{Confirm, CustomType, Text};
use std::io::IsTerminal;
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
        println!("  • Sync the snapshot:  rdc sync {env}");
    }
    Ok(())
}

/// Resolve the list of env specs to use. When `env_specs` is empty,
/// prompts interactively (TTY-only). Invalid input re-prompts inline
/// rather than aborting; Esc or Ctrl+C cancels.
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
            println!("Set up a new rdc project.");
        }
        InitMode::Extend => {
            let existing = if cfg.envs.is_empty() {
                "(none)".to_string()
            } else {
                cfg.envs.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            println!("Add environments to existing project. Existing: {existing}.");
        }
    }
    println!();

    let mut specs: Vec<String> = Vec::new();
    let mut taken: Vec<String> = cfg.envs.keys().cloned().collect();

    loop {
        let env_name = match prompt_env_name(&taken) {
            Ok(s) => s,
            Err(PromptOutcome::Cancelled) => {
                return finish_or_cancel(specs);
            }
            Err(PromptOutcome::Failed(e)) => return Err(e),
        };

        let api_base = match prompt_api_base() {
            Ok(s) => s,
            Err(PromptOutcome::Cancelled) => {
                return finish_or_cancel(specs);
            }
            Err(PromptOutcome::Failed(e)) => return Err(e),
        };

        let org_id = match prompt_org_id() {
            Ok(n) => n,
            Err(PromptOutcome::Cancelled) => {
                return finish_or_cancel(specs);
            }
            Err(PromptOutcome::Failed(e)) => return Err(e),
        };

        taken.push(env_name.clone());
        specs.push(format!("{env_name}={api_base}:{org_id}"));

        match Confirm::new("Add another environment?")
            .with_default(false)
            .prompt()
        {
            Ok(true) => continue,
            Ok(false) => break,
            // Esc / Ctrl+C on the confirm = "I'm done, don't add another".
            Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => break,
            Err(e) => return Err(anyhow!("prompt failed: {e}")),
        }
    }

    if specs.is_empty() {
        return Err(anyhow!("at least one env is required"));
    }
    Ok(specs)
}

enum PromptOutcome {
    Cancelled,
    Failed(anyhow::Error),
}

fn finish_or_cancel(specs: Vec<String>) -> Result<Vec<String>> {
    if specs.is_empty() {
        Err(anyhow!("init cancelled — no environments defined"))
    } else {
        Ok(specs)
    }
}

fn map_prompt_err(e: InquireError) -> PromptOutcome {
    match e {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            PromptOutcome::Cancelled
        }
        other => PromptOutcome::Failed(anyhow!("prompt failed: {other}")),
    }
}

fn prompt_env_name(taken: &[String]) -> std::result::Result<String, PromptOutcome> {
    let taken_owned: Vec<String> = taken.to_vec();
    let value = Text::new("Env name")
        .with_help_message("short slug, e.g. dev, staging, prod (Esc to finish)")
        .with_validator(move |input: &str| {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                return Ok(Validation::Invalid("env name cannot be empty".into()));
            }
            if !trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Ok(Validation::Invalid(
                    "only letters, digits, '-', and '_' are allowed".into(),
                ));
            }
            if taken_owned.iter().any(|n| n == trimmed) {
                return Ok(Validation::Invalid(
                    format!("'{trimmed}' is already defined").into(),
                ));
            }
            Ok(Validation::Valid)
        })
        .prompt()
        .map_err(map_prompt_err)?;
    Ok(value.trim().to_string())
}

fn prompt_api_base() -> std::result::Result<String, PromptOutcome> {
    let value = Text::new("API base URL")
        .with_help_message("e.g. https://api.elis.rossum.ai/v1")
        .with_validator(|input: &str| {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                return Ok(Validation::Invalid("api_base cannot be empty".into()));
            }
            if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                return Ok(Validation::Invalid(
                    "must start with http:// or https://".into(),
                ));
            }
            Ok(Validation::Valid)
        })
        .prompt()
        .map_err(map_prompt_err)?;
    Ok(value.trim().to_string())
}

fn prompt_org_id() -> std::result::Result<u64, PromptOutcome> {
    CustomType::<u64>::new("Organization ID")
        .with_help_message("Rossum organization ID (positive integer)")
        .with_error_message("must be a positive integer")
        .prompt()
        .map_err(map_prompt_err)
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
/// is a once-only file — `rdc init` creates it, but sync never
/// overwrites it. Existing files (e.g. when re-running init on an
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
  Regenerated on every `rdc sync` — never hand-edit.
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
  hooks/<slug>.json                       + sibling <slug>.py or <slug>.js for function hooks
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
  map/<src>-to-<tgt>.toml                 cross-env slug mappings
```

## Editing recipes

| To change… | Edit | Then run |
|---|---|---|
| Hook code (Python or Node.js) | `envs/<env>/hooks/<slug>.py` or `…/<slug>.js` | `rdc sync <env>` |
| Hook config (events, queues, name) | `envs/<env>/hooks/<slug>.json` | `rdc sync <env>` |
| Schema fields | `envs/<env>/workspaces/<ws>/queues/<q>/schema.json` | `rdc sync <env>` |
| A formula | `envs/<env>/workspaces/<ws>/queues/<q>/formulas/<field>.py` | `rdc sync <env>` |
| Queue settings | `envs/<env>/workspaces/<ws>/queues/<q>/queue.json` | `rdc sync <env>` |
| Rule's trigger condition (Python) | `envs/<env>/rules/<slug>.py` | `rdc sync <env>` |
| Rule config (name, queues) | `envs/<env>/rules/<slug>.json` | `rdc sync <env>` |
| Label name / colour | `envs/<env>/labels/<slug>.json` | `rdc sync <env>` |
| Email template | `envs/<env>/workspaces/<ws>/queues/<q>/email-templates/<slug>.json` | `rdc sync <env>` |
| Per-env override only | `envs/<env>/overlay.toml` | `rdc sync <env>` |

## Adding a new object

Create the JSON (and `.py` if the kind has executable code) under the
right directory. `rdc sync <env>` detects files with no lockfile entry,
POSTs them, and writes the server-assigned `id` / `url` back into the
local file. Cross-references must use URLs already known to the
lockfile (e.g. a new hook's `queues` field must point at queues that
already exist on the remote).

## Deleting an object

The snapshot is the declared state of the environment, including
absence. Remove the local file (`rm envs/<env>/labels/foo.json`) and
the next `rdc sync <env>` will detect the tombstone — the lockfile
entry remains, signalling "this object was tracked and is now gone."

Two intentional acts are required before the DELETE hits the remote:

1. Removing the file (you did this).
2. Either answering `y` to the interactive batch prompt on a TTY, or
   passing `--allow-deletes`. `--yes` does NOT bypass — destruction
   needs its own authorisation.

In non-TTY (CI) mode, `--allow-deletes` is mandatory; without it the
sync refuses with a clear list of pending tombstones.

`rdc sync <env> --dry-run` lists pending tombstones in a `deletes:`
section without sending anything; `--diff` adds the full remote body
for each as a deleted-file diff.

Deletes run after creates / updates in reverse dependency order
(`engine_fields → engines → labels → rules → hooks →
email_templates → inboxes → queues → schemas → workspaces`) so a
queue is gone before the workspace that contained it. The Rossum
DELETE endpoint accepts 404 as success, so an object already absent
on the remote just gets its lockfile entry cleaned up.

If the remote has been modified since the last sync, an inline
resolver opens — `[k]eep delete` / `[s]kip` / `[a]bort` — so a
tombstoned delete can't silently overwrite a remote update.

## Common commands

- `rdc sync <env>` — reconcile local snapshot and remote in one pass;
  pulls remote edits + pushes local edits + creates new objects;
  `--allow-deletes` to also remove remote objects whose local files
  you've deleted; `--no-push` for read-only audit; `--no-pull` to
  deploy local edits without overwriting local files
- `rdc diff <env>` — local-vs-remote diff (no writes)
- `rdc diff <a> <b>` — diff two local snapshots
- `rdc deploy <src> <tgt>` — promote one env to another in one shot
  (POST missing + PATCH deltas, URL rewrites included); `--dry-run`
  previews without writing
- `rdc repair <env> --rename-slugs` — realign stale local slugs after
  a remote rename (offline)
- `rdc repair <env> --rebuild-lock` — recover from a corrupted
  lockfile by re-syncing everything

## Conflicts & drift

A three-way merge (local · base · remote) runs on every sync. When
both sides have diverged, `rdc sync` prompts an inline resolver:
`[k]eep local · [r]emote · [e]dit · [s]kip · [a]bort`. In non-TTY
(CI / `--yes`) mode, conflicts produce a `<file>.<env-name>` shadow and
keep the local on disk.

The same drift check runs before each PATCH on the push side. The
prompt is `[k]` (force-push), `[r]` (adopt remote), `[s]` (skip),
`[a]` (abort).

## Cross-env deploys (e.g. dev → prod)

1. `rdc sync dev` and `rdc sync prod` so both lockfiles are populated.
2. `rdc deploy dev prod --dry-run` — preview the plan.
3. `rdc deploy dev prod` — execute. Per-env values stay in each env's
   `overlay.toml`; the slug-to-slug mapping is built automatically and
   stored at `.rdc/map/dev-to-prod.toml` (hand-edit for renames if needed).

## What NOT to edit

- `_index.md` (auto-regenerated by every sync)
- `.rdc/state/<env>.lock.json` (slug↔id and base hashes; rdc owns this)
- `*.<env-name>` files (shadow files from conflict resolution; either
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
  deploys via `rdc deploy` may surface confusing diffs.
"#;
