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

/// What a scaffold writer did to its file, for the `--force` summary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scaffolded {
    Created,
    /// Existing file replaced with the current template (`--force` only).
    Rewritten,
    /// Missing canonical lines appended, user lines kept
    /// (`.gitignore` / `.gitattributes`).
    Merged,
    Unchanged,
}

impl Scaffolded {
    fn label(self) -> &'static str {
        match self {
            Scaffolded::Created => "created",
            Scaffolded::Rewritten => "rewritten",
            Scaffolded::Merged => "updated",
            Scaffolded::Unchanged => "unchanged",
        }
    }
}

pub async fn run(env_specs: Vec<String>, force: bool) -> Result<()> {
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

    // `--force` with no `--env` is regenerate-only: refresh the scaffold files
    // from this binary's templates and stop — no env wizard, no auth, no sync,
    // and `rdc.toml` is left exactly as it is. There is nothing to regenerate
    // without a project, so that combination is a usage error rather than a
    // silent fall-through into the bootstrap wizard.
    let regenerate_only = force && env_specs.is_empty();
    if regenerate_only && matches!(mode, InitMode::New) {
        return Err(anyhow!(
            "rdc init --force: no rdc.toml in {} — nothing to regenerate. \
             Bootstrap the project first: rdc init --env <env>=<api_base>:<org_id>",
            cwd.display()
        ));
    }

    // Get env specs (interactive when none provided + TTY available).
    let env_specs = if regenerate_only {
        Vec::new()
    } else {
        resolve_env_specs(env_specs, &cfg, &mode)?
    };

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
        // Reject names that normalize to the same `RDC_TOKEN_<...>`
        // variable as an existing env. E.g. adding `dev_us` when
        // `dev-us` is already defined would silently share env-var
        // resolution; refuse rather than let one steal the other's
        // token.
        let candidate_var = crate::secrets::env_var_for(&env_name, "TOKEN");
        if let Some(clash) = cfg
            .envs
            .keys()
            .find(|existing| crate::secrets::env_var_for(existing, "TOKEN") == candidate_var)
        {
            return Err(anyhow!(
                "env '{env_name}' would share API-token env var '{candidate_var}' with \
                 existing env '{clash}' (rdc normalizes non-alphanumerics to '_' so \
                 the shell can export it). Pick a distinct name."
            ));
        }
        cfg.envs.insert(env_name.clone(), env_cfg);
        new_env_names.push(env_name);
    }

    // Nothing was added in regenerate-only mode, so don't rewrite `rdc.toml`:
    // a save round-trips it through `ProjectConfig` and would drop any key this
    // version doesn't model.
    if !regenerate_only {
        cfg.save(&cfg_path)?;
    }

    let scaffold: Vec<(&str, Scaffolded)> = vec![
        (".gitignore", write_gitignore(&cwd)?),
        (".gitattributes", write_gitattributes(&cwd)?),
        ("CLAUDE.md", write_claude_md(&cwd, force)?),
        ("README.md", write_readme(&cwd, &cfg, force)?),
        (".gitlab-ci.yml", write_gitlab_ci(&cwd, force)?),
    ];
    std::fs::create_dir_all(cwd.join("secrets"))
        .with_context(|| format!("creating {}", cwd.join("secrets").display()))?;
    for env in &new_env_names {
        let paths = Paths::for_env(&cwd, env);
        std::fs::create_dir_all(paths.env_root())
            .with_context(|| format!("creating {}", paths.env_root().display()))?;
        std::fs::create_dir_all(paths.hooks_dir())
            .with_context(|| format!("creating {}", paths.hooks_dir().display()))?;
    }

    // The per-file summary is the whole output of a regenerate-only run, and
    // the receipt a `--force` user needs elsewhere ("did it touch my README?").
    // Silent otherwise, so a plain init keeps its one-line output.
    if force {
        println!("Scaffold files:");
        for (name, outcome) in &scaffold {
            println!("  {name:<16} {}", outcome.label());
        }
    }

    let env_list = new_env_names.join(", ");
    match mode {
        InitMode::New => {
            println!("Initialized rdc project with envs: {env_list}");
        }
        InitMode::Extend if !regenerate_only => {
            println!("Added env(s): {env_list}");
        }
        InitMode::Extend => {}
    }

    // Auth-on-init: for each new env, try to authenticate up front so the
    // user doesn't have to make a second pass through `rdc auth`. Token
    // sources, in order: `RDC_TOKEN_<UPPER>` env var (non-interactive,
    // CI-friendly), then masked TTY prompt (when stdin is a terminal).
    // Validation goes through the same `validate_and_save_token` helper
    // `rdc auth` uses, so the on-disk state and printed feedback match.
    let mut auth_succeeded: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for env in &new_env_names {
        let env_var_name = crate::secrets::env_var_for(env, "TOKEN");
        let token_source = match std::env::var(&env_var_name) {
            Ok(t) if !t.trim().is_empty() => Some((t.trim().to_string(), env_var_name.clone())),
            _ => {
                if std::io::stdin().is_terminal() {
                    prompt_token_for_env(env)?.map(|t| (t, "interactive prompt".to_string()))
                } else {
                    None
                }
            }
        };

        let Some((token, source_label)) = token_source else {
            continue;
        };
        let env_cfg = cfg
            .envs
            .get(env)
            .expect("just inserted into cfg above");
        match crate::cli::auth::validate_and_save_token(env_cfg, &cwd, env, &token)
            .await
        {
            Ok(_org_name) => {
                auth_succeeded.insert(env.clone());
            }
            Err(e) => {
                // Project files stay; user can rerun `rdc auth` once
                // they've sorted out the credential issue.
                let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
                log.event(
                    crate::log::Action::Warn,
                    &format!("token for env '{env}' (from {source_label}) failed validation: {e:#}; re-run `rdc auth {env}` to retry"),
                );
            }
        }
    }

    // Sync-on-init: once auth has succeeded for one or more new envs,
    // the very next manual step the user would run is `rdc sync <env>`.
    // Offer to do that here so the happy path is a single command. TTY
    // gated only, matching the rest of the init wizard. Default Yes —
    // the user just configured the env, sync is what they came for.
    //
    // Each env is synced independently; one failure doesn't abort the
    // others (or invalidate the project files already on disk). Envs
    // whose sync failed fall back to the manual `rdc sync <env>` line
    // in the Next steps block below.
    let syncable: Vec<String> = new_env_names
        .iter()
        .filter(|e| auth_succeeded.contains(*e))
        .cloned()
        .collect();
    let mut synced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if !syncable.is_empty() && std::io::stdin().is_terminal() {
        let prompt_label = if syncable.len() == 1 {
            format!("Sync '{}' now?", syncable[0])
        } else {
            format!("Sync the {} new env(s) now?", syncable.len())
        };
        let want_sync = match Confirm::new(&prompt_label)
            .with_default(true)
            .with_help_message("pulls the remote snapshot into envs/<env>/ — you can always run `rdc sync` later")
            .prompt()
        {
            Ok(b) => b,
            // Esc / Ctrl+C on the confirm = "don't sync now". Falls
            // through to the next-steps message; project files stay.
            Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => false,
            Err(e) => return Err(anyhow!("prompt failed: {e}")),
        };
        if want_sync {
            for env in &syncable {
                println!();
                match crate::cli::sync::run(env, true, false, false, false, false, None).await {
                    Ok(_outcome) => {
                        synced.insert(env.clone());
                    }
                    Err(e) => {
                        let log =
                            crate::log::Log::new(crate::cli::resolve::detect_color_mode());
                        log.event(
                            crate::log::Action::Warn,
                            &format!(
                                "sync of env '{env}' failed: {e:#}; re-run `rdc sync {env}` later"
                            ),
                        );
                    }
                }
            }
        }
    }

    // Only show Next steps when there's actually something left to do.
    // After a clean "init → auth → sync" run, every env is fully set up
    // and a trailing empty header would feel like a dangling todo list.
    let needs_token_step = new_env_names.iter().any(|e| !auth_succeeded.contains(e));
    let needs_sync_step = new_env_names.iter().any(|e| !synced.contains(e));
    if needs_token_step || needs_sync_step {
        println!();
        println!("Next steps:");
        for env in &new_env_names {
            if auth_succeeded.contains(env) {
                continue;
            }
            let env_var_name = crate::secrets::env_var_for(env, "TOKEN");
            println!("  - Set the API token for env '{env}':");
            println!("      rdc auth {env} --token <token>     # validates + writes secrets/{env}.secrets.json");
            println!("      # or: export {env_var_name}=<token>");
        }
        for env in &new_env_names {
            if synced.contains(env) {
                continue;
            }
            println!("  - Sync the snapshot:  rdc sync {env}");
        }
    }
    Ok(())
}

/// Prompt for an API token on TTY (masked). Returns `Ok(None)` when the
/// user cancels (Ctrl+C / Esc) or submits an empty value — both mean
/// "don't auth right now; fall back to the next-steps message and let
/// `rdc auth` handle it later". Hard errors (real I/O failures from the
/// prompt library) propagate.
fn prompt_token_for_env(env: &str) -> Result<Option<String>> {
    use inquire::error::InquireError;
    use inquire::{Password, PasswordDisplayMode};

    println!();
    let result = Password::new(&format!("API token for '{env}'"))
        .with_display_mode(PasswordDisplayMode::Masked)
        .without_confirmation()
        .with_help_message("Esc / Ctrl+C to skip — you can run `rdc auth` later")
        .prompt();
    match result {
        Ok(t) => {
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(anyhow!("token prompt failed: {e}")),
    }
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

        // Ask org id before api_base: env name + org id are the identity
        // facts a user knows offhand, so leading with them feels more
        // natural than the connection URL. The API token is gathered later
        // (the auth phase in `run`), so the user-facing question order is
        // env name -> org id -> api_base -> token.
        let org_id = match prompt_org_id() {
            Ok(n) => n,
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
        Err(anyhow!("init cancelled; no environments defined"))
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

/// Ensure the canonical ignore patterns are present. Additive by design —
/// including under `--force`, which is why it takes no `force` flag: the merge
/// already restores every rdc-owned line without discarding the user's own.
fn write_gitignore(root: &Path) -> Result<Scaffolded> {
    let path = root.join(".gitignore");
    // Canonical patterns this template ensures. Each line is checked
    // independently so re-running `rdc init` on a project that already
    // has *some* of these (or has them with surrounding annotation)
    // adds only the missing lines instead of dumping the whole block
    // and creating duplicates.
    //
    // `/.rdc/state/*.lock` — sibling of the `<env>.lock.json` lockfile.
    // The `.lock` file is the empty advisory lock fs4 grabs an OS-level
    // exclusive lock on (see `cli::sync::lock::EnvLock`). The body is
    // always empty by design and the contents are per-machine, so it
    // shouldn't be committed. `*.lock` is narrow enough not to match
    // `.lock.json` (that's the actual committable lockfile state).
    //
    // `/.rdc/state/*.base` — sync's 3-way merge keeps a per-machine
    // sidecar cache at `.rdc/state/<env>.base/` mirroring the env
    // tree at the moment of the last successful sync (see
    // `state::base_cache`). Like the advisory lock file, it's
    // local-only and regenerated on the next sync, so it must not
    // be committed.
    //
    // `/.rdc/conflicts` — sync parks the remote side of an unresolved
    // conflict (and its `-deleted` marker) under `.rdc/conflicts/<env>/`
    // (see `paths::Paths::conflict_shadow_path`). These are transient
    // review artifacts consumed when the conflict is resolved, so they
    // must not be committed.
    const PATTERNS: &[&str] = &[
        "/target",
        "/secrets",
        "/.rdc/cache",
        "/.rdc/state/*.lock",
        "/.rdc/state/*.base",
        "/.rdc/conflicts",
    ];

    if !path.exists() {
        let body: String = PATTERNS.iter().map(|p| format!("{p}\n")).collect();
        write_atomic(&path, body.as_bytes())?;
        return Ok(Scaffolded::Created);
    }

    let existing = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let existing_lines: std::collections::HashSet<&str> =
        existing.lines().map(str::trim).collect();

    let missing: Vec<&str> = PATTERNS
        .iter()
        .copied()
        .filter(|p| !existing_lines.contains(p))
        .collect();
    if missing.is_empty() {
        return Ok(Scaffolded::Unchanged);
    }

    let mut combined = existing;
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    for p in missing {
        combined.push_str(p);
        combined.push('\n');
    }
    write_atomic(&path, combined.as_bytes())?;
    Ok(Scaffolded::Merged)
}

/// Mark rdc-owned files under `.rdc/` as generated so GitHub collapses
/// their diffs by default and excludes them from language stats. The
/// state lockfile (`state/<env>.lock.json`) and cross-env mappings
/// (`map/<src>-to-<tgt>.toml`) are produced by the tool; reviewers
/// shouldn't have to scroll past them.
/// Additive like [`write_gitignore`], and for the same reason.
fn write_gitattributes(root: &Path) -> Result<Scaffolded> {
    let path = root.join(".gitattributes");
    let body = ".rdc/** linguist-generated=true\n";
    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if existing.contains(".rdc/** linguist-generated") {
            return Ok(Scaffolded::Unchanged);
        }
        let mut combined = existing;
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(body);
        write_atomic(&path, combined.as_bytes())?;
        Ok(Scaffolded::Merged)
    } else {
        write_atomic(&path, body.as_bytes())?;
        Ok(Scaffolded::Created)
    }
}

/// Write `body` at `path` with the scaffold contract: a missing file is
/// created, an existing one is left alone unless `force`. Under `--force` the
/// bytes are compared first, so re-running on an untouched project reports
/// `Unchanged` instead of churning mtimes (and the comparison is byte-wise, so
/// a hand-edited file that isn't valid UTF-8 doesn't fail the run).
fn write_template_file(path: &Path, body: &str, force: bool) -> Result<Scaffolded> {
    if path.exists() {
        if !force {
            return Ok(Scaffolded::Unchanged);
        }
        let existing =
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if existing == body.as_bytes() {
            return Ok(Scaffolded::Unchanged);
        }
        write_atomic(path, body.as_bytes())?;
        return Ok(Scaffolded::Rewritten);
    }
    write_atomic(path, body.as_bytes())?;
    Ok(Scaffolded::Created)
}

/// Write the GitLab CI pipeline at `<root>/.gitlab-ci.yml`. Same scaffold
/// contract as [`write_claude_md`]: created when absent, replaced only under
/// `--force`, so a hand-tuned pipeline is never clobbered by an ordinary init.
///
/// The body is the repo's `templates/gitlab-ci.yml`, embedded at compile time
/// so the copy users read on GitHub and the copy this binary writes cannot
/// drift. It ships with placeholder env names (`dev` -> `test` -> `prod`) and
/// TODO markers rather than the project's real envs: `rdc.toml` stores envs in a
/// `BTreeMap`, so their order is alphabetical and a promotion chain cannot be
/// derived from it (`dev`, `prod`, `test` would chain dev -> prod -> test).
fn write_gitlab_ci(root: &Path, force: bool) -> Result<Scaffolded> {
    write_template_file(&root.join(".gitlab-ci.yml"), GITLAB_CI_TEMPLATE, force)
}

/// Write an agent guide at `<root>/CLAUDE.md`. Unlike `_index.md`, this
/// is a once-only file — `rdc init` creates it, but sync never
/// overwrites it. Existing files (e.g. when re-running init on an
/// already-bootstrapped repo, or when the user has hand-edited the
/// guide) are left untouched unless `force` is set.
fn write_claude_md(root: &Path, force: bool) -> Result<Scaffolded> {
    write_template_file(&root.join("CLAUDE.md"), CLAUDE_MD_TEMPLATE, force)
}

/// Write a human-facing `README.md` at the project root listing the
/// run-the-project commands: one `rdc sync <env>` per env defined in
/// `cfg`, and — when there are at least two envs — a promote example
/// (`rdc migrate` + `rdc sync`) using the first two envs alphabetically. Same
/// once-only contract as [`write_claude_md`]: skipped when README.md
/// already exists (unless `force`), so the user's content is never
/// clobbered by an ordinary init.
///
/// Title is the project root's basename (matches the user's mental
/// model of "what is this repo called"); falls back to a generic title
/// when the basename isn't valid UTF-8 or is empty.
fn write_readme(root: &Path, cfg: &ProjectConfig, force: bool) -> Result<Scaffolded> {
    let path = root.join("README.md");
    // Cheap exit before building the body, which the non-force path throws away.
    if path.exists() && !force {
        return Ok(Scaffolded::Unchanged);
    }
    let title = root
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "Rossum configuration".to_string());

    let mut md = String::new();
    md.push_str(&format!("# {title}\n\n"));
    md.push_str(
        "Rossum.ai configuration managed by \
         [rdc](https://github.com/mrtnzlml/rdc). Each env's live state is \
         mirrored as files under `envs/<env>/` so it can be reviewed, edited, \
         and deployed like code.\n\n",
    );

    if !cfg.envs.is_empty() {
        md.push_str("## Sync each environment\n\n");
        md.push_str(
            "`rdc sync <env>` reconciles the local snapshot with the remote \
             env in one pass — pulls remote edits, pushes local edits, \
             creates new objects. Run it after cloning, after pulling new \
             commits, and whenever you've edited files under `envs/<env>/`.\n\n",
        );
        md.push_str("```sh\n");
        for env in cfg.envs.keys() {
            md.push_str(&format!("rdc sync {env}\n"));
        }
        md.push_str("```\n\n");
    }

    // Promote section only when there are two envs to promote between.
    // BTreeMap iteration is already sorted, so the first two keys give
    // a deterministic example without an extra sort step.
    if cfg.envs.len() >= 2 {
        let mut iter = cfg.envs.keys();
        let src = iter.next().expect("len >= 2");
        let tgt = iter.next().expect("len >= 2");
        md.push_str("## Promote changes between environments\n\n");
        md.push_str(
            "`rdc migrate <src> <tgt>` copies one env's snapshot into \
             another's, locally — slug remap, portable-ref rewrite, target \
             overlay; zero remote calls. Review the transform with `git \
             diff`, then push it with `rdc sync <tgt>`.\n\n",
        );
        md.push_str("```sh\n");
        md.push_str(&format!(
            "rdc migrate {src} {tgt} --dry-run   # preview the local transform\n"
        ));
        md.push_str(&format!("rdc migrate {src} {tgt}\n"));
        md.push_str("git diff                          # review what would be pushed\n");
        md.push_str(&format!(
            "rdc sync {tgt}                      # push to the remote\n"
        ));
        md.push_str("```\n\n");
    }

    md.push_str("## See also\n\n");
    md.push_str(
        "- `CLAUDE.md` — editing recipes, repo layout, common commands, \
         conflict + promote workflows.\n\
         - `envs/<env>/_index.md` — auto-generated map of every object in \
         `<env>` with paths and cross-references.\n\
         - `.gitlab-ci.yml` — scheduled archive + one manual deploy button per \
         env; its header lists the CI variables to set.\n",
    );

    write_template_file(&path, &md, force)
}

/// Write the five init-time scaffold files (`.gitignore`, `.gitattributes`,
/// `CLAUDE.md`, `README.md`, `.gitlab-ci.yml`) at `cwd`, given a single-env
/// project shape. Idempotent: each underlying writer skips when the file
/// already exists.
///
/// Exposed for embedders (e.g. the Rossum Local desktop app) that need
/// a Connection folder to look identical to one produced by `rdc init`
/// — including the agent guide that Claude Code reads.
pub fn write_scaffold_files(
    cwd: &Path,
    env_name: &str,
    api_base: &str,
    org_id: u64,
) -> Result<()> {
    write_gitignore(cwd)?;
    write_gitattributes(cwd)?;
    write_claude_md(cwd, false)?;
    let mut cfg = ProjectConfig::default();
    cfg.envs.insert(
        env_name.to_string(),
        crate::config::EnvConfig {
            api_base: api_base.to_string(),
            org_id,
        },
    );
    write_readme(cwd, &cfg, false)?;
    write_gitlab_ci(cwd, false)?;
    Ok(())
}

/// The GitLab CI pipeline `rdc init` drops into a project, embedded from the
/// repo's `templates/gitlab-ci.yml` (see [`write_gitlab_ci`]).
const GITLAB_CI_TEMPLATE: &str = include_str!("../../templates/gitlab-ci.yml");

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
.gitlab-ci.yml                            scheduled archive + deploy buttons; fill in its TODOs
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
section without sending anything.

Deletes run after creates / updates in reverse dependency order
(`engine_fields → engines → labels → rules → hooks →
email_templates → inboxes → queues → schemas → workspaces`) so a
queue is gone before the workspace that contained it. The Rossum
DELETE endpoint accepts 404 as success, so an object already absent
on the remote just gets its lockfile entry cleaned up.

If the remote has been modified since the last sync, an inline
resolver opens — `[k]eep delete` / `[s]kip` / `[a]bort` — so a
tombstoned delete can't silently overwrite a remote update.

## Redacted fields

To keep git diffs quiet across syncs, rdc replaces a few server-set
runtime fields with a sentinel string on disk. The key stays visible
so you (and any agent reading the snapshot) see that the field exists
in Rossum — only its value is suppressed.

| Kind | Field | Why |
|---|---|---|
| queues | `counts` | Per-status document counts. Updates whenever a document moves through review states; the real value is always live in the Rossum UI / API. |

`rdc` strips these fields from outgoing PATCH bodies automatically,
so the sentinel never reaches the server.

## Common commands

- `rdc sync <env>` — reconcile local snapshot and remote in one pass;
  pulls remote edits + pushes local edits + creates new objects;
  `--dry-run` previews changes without writing; `--allow-deletes` to
  also remove remote objects whose local files you've deleted;
  `--no-push` for read-only audit; `--no-pull` to deploy local edits
  without overwriting local files
- `rdc migrate <src> <tgt>` — copy one env's snapshot into another's,
  locally (slug remap + `rdc://` ref rewrite + target overlay; zero
  remote calls); review with `git diff`, then `rdc sync <tgt>` to push;
  `--dry-run` previews, `--only <kind>/<slug>` narrows scope
- `rdc doctor <env>` — realign stale local slugs after
  a remote rename and prune orphan base-cache entries (offline)

## Conflicts & drift

A three-way merge (local · base · remote) runs on every sync. When
both sides have diverged, `rdc sync` prompts an inline resolver:
`[k] keep local · [r] use <env> · [e] edit · [s] skip (shadow file) ·
[a] abort` (plus `[h] hunk-by-hunk` for multi-hunk bodies). In non-TTY
(CI / `--yes`) mode, conflicts produce a `<file>.<env-name>` shadow and
keep the local on disk.

The same drift check runs before each PATCH on the push side. The
prompt is `[k]` (force-push), `[r]` (adopt remote), `[s]` (skip),
`[a]` (abort).

## Promoting changes between environments (e.g. dev → prod)

1. `rdc sync dev` and `rdc sync prod` so both lockfiles are populated.
2. `rdc migrate dev prod --dry-run` — preview the local file transform.
3. `rdc migrate dev prod` — copy dev's snapshot into `envs/prod/`,
   renaming slugs per the mapping (one hand-editable file,
   `.rdc/mapping.toml`, where each environment names its own slug for
   an object; objects with identical slugs need no entry), rewriting
   portable `rdc://` refs, and applying prod's `overlay.toml`.
4. Review the result with `git diff`, then `rdc sync prod` to push —
   sync creates missing objects in dependency order.

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
  promotion via `rdc migrate` may surface confusing diffs.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedder entry point (desktop Connection folders) must produce the
    /// same scaffold `rdc init` does — a Connection folder is meant to be
    /// indistinguishable from a hand-initialised project.
    #[test]
    fn write_scaffold_files_writes_every_scaffold_file() {
        let dir = tempfile::TempDir::new().unwrap();
        write_scaffold_files(dir.path(), "main", "https://example.rossum.app/api/v1", 1).unwrap();
        for name in [
            ".gitignore",
            ".gitattributes",
            "CLAUDE.md",
            "README.md",
            ".gitlab-ci.yml",
        ] {
            assert!(dir.path().join(name).exists(), "{name} should be written");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(),
            GITLAB_CI_TEMPLATE
        );
    }

    /// Embedders call this on every sync, so it must stay non-destructive.
    #[test]
    fn write_scaffold_files_never_clobbers_existing_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitlab-ci.yml"), "mine\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "mine\n").unwrap();

        write_scaffold_files(dir.path(), "main", "https://example.rossum.app/api/v1", 1).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(),
            "mine\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "mine\n"
        );
    }

    /// Create → skip → (force) rewrite only when the bytes actually differ, so
    /// `rdc init --force` on an untouched project churns no mtimes.
    #[test]
    fn write_template_file_force_semantics() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("scaffold.txt");

        assert_eq!(
            write_template_file(&p, "body\n", false).unwrap(),
            Scaffolded::Created
        );
        assert_eq!(
            write_template_file(&p, "body\n", false).unwrap(),
            Scaffolded::Unchanged
        );
        assert_eq!(
            write_template_file(&p, "body\n", true).unwrap(),
            Scaffolded::Unchanged
        );

        std::fs::write(&p, "drifted\n").unwrap();
        assert_eq!(
            write_template_file(&p, "body\n", false).unwrap(),
            Scaffolded::Unchanged,
            "without --force a drifted file is left alone"
        );
        assert_eq!(
            write_template_file(&p, "body\n", true).unwrap(),
            Scaffolded::Rewritten
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "body\n");
    }

    /// A hand-edited file that isn't valid UTF-8 must not fail the run: the
    /// force comparison reads bytes, not a `String`.
    #[test]
    fn write_template_file_force_handles_non_utf8() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("scaffold.txt");
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();

        assert_eq!(
            write_template_file(&p, "body\n", true).unwrap(),
            Scaffolded::Rewritten
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "body\n");
    }

    /// Smoke-check the `include_str!` target: the constant is the real pipeline,
    /// not an empty or hand-copied stand-in.
    #[test]
    fn embedded_gitlab_ci_template_is_the_repo_pipeline() {
        assert!(GITLAB_CI_TEMPLATE.starts_with("# GitLab CI for a Rossum project managed with rdc"));
        assert!(GITLAB_CI_TEMPLATE.contains("rdc sync \"$RDC_ENV\" --no-push --yes --conflict use-remote"));
        assert!(GITLAB_CI_TEMPLATE.contains("rdc migrate \"$RDC_SRC\" \"$RDC_ENV\" --mirror --yes"));
    }
}
