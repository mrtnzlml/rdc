//! Recovery of a crashed run's create journal (spec §4).
//!
//! A run that died mid-create leaves `pending` in the lockfile: creates that
//! may have reached the server without ever being recorded. The next sync —
//! which holds the env lock, so the run that wrote the journal is over —
//! settles each one before it classifies anything, so the object is neither
//! lost (an orphan nobody tracks) nor created a second time:
//!
//! - an op with an id: the object is fetched; gone → the op is dropped, found
//!   → the create is completed;
//! - an op without an id (the POST was in flight): the env is searched with
//!   the create guard's own matcher ([`RossumClient::find_create_match`]);
//!   none → dropped, the local file is created as any new one; one →
//!   completed; several → the user picks, or the sync stops naming them.
//!
//! Completing never overwrites a local env file. With the file present, the
//! object's lockfile entry and base-cache copy are what the driver would have
//! recorded, so the classifier sees the file as `Clean` or, if it differs
//! from the server (an edit after the crash, a store extension installed but
//! never PATCHed), as a `LocalEdit` the normal push sends. Without it (a
//! clone that never had the file) there is nothing to overwrite, and the
//! create is completed exactly as the driver does, file included.
//!
//! Recovery is deterministic, so two machines recovering the same journal
//! record the same ids.

use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What recovery did with each op of the journal.
#[derive(Debug, Default, PartialEq)]
pub struct RecoveryReport {
    /// `(kind, slug, id)` of every create completed — or, under `--dry-run`,
    /// that would be.
    pub completed: Vec<(String, String, u64)>,
    /// `(kind, slug)` of every op dropped: its object does not exist, so the
    /// local file (if any) is created again as a new one.
    pub dropped: Vec<(String, String)>,
}

/// Settle every op of `lockfile.pending` (taken out of it) against the env,
/// completing the creates that happened into `lockfile` and the env tree.
///
/// Nothing is persisted here: the caller commits `lockfile` once this
/// returns `Ok`, and on an error the lockfile on disk still holds the journal.
/// Under `dry_run` only reads are made and nothing is written; the report
/// says what would be done.
pub async fn recover(
    paths: &Paths,
    lockfile: &mut Lockfile,
    client: &RossumClient,
    interactive: bool,
    dry_run: bool,
    progress: &Arc<Log>,
) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    let Some(pending) = lockfile.pending.take() else {
        return Ok(report);
    };
    let would = if dry_run { "would " } else { "" };
    // Parents first, so a recovered queue's directory exists before the
    // schema or inbox that lives in it is placed; journal order otherwise.
    let rank = |kind: &str| ["workspaces", "queues", "schemas", "inboxes"].iter().position(|k| *k == kind).unwrap_or(4);
    let mut ops = pending.ops;
    ops.sort_by_key(|op| (rank(&op.kind), op.seq));
    // First find every op's object, then complete them: a recovered object's
    // recorded form names the others by `rdc://` ref only once they are all
    // in the lockfile — a workspace's `queues`, a queue's `schema` — or its
    // base would carry raw urls and read as remotely changed next to the
    // portable form the next listing yields.
    let mut found_objects: Vec<(crate::state::journal::CreateIntent, Value)> = Vec::new();
    for op in ops {
        let (kind, slug) = (op.kind.as_str(), op.slug.as_str());
        let recorded = lockfile.objects.get(kind).and_then(|m| m.get(slug)).map(|e| e.id);
        if op.id.is_some() && recorded == op.id {
            // The create was recorded after all: nothing is left to complete.
            progress.event(Action::Info, &format!("{kind}/{slug}: already recorded; the unfinished create is settled"));
            report.dropped.push((op.kind, op.slug));
            continue;
        }
        // A slug recorded under another id is a stale entry (a restore's
        // re-create commits its new entry only at the end of the run): the
        // object found below replaces it; with none, the entry stays.
        let object = match op.id {
            Some(id) => client
                .get_created(kind, id, None)
                .await
                .with_context(|| format!("looking up {kind}/{id}, created for '{slug}' by an unfinished run"))?,
            None => {
                // Taken: what the lockfile records, and what an earlier op found.
                let mut claimed: std::collections::HashSet<u64> =
                    lockfile.objects.get(kind).map(|m| m.values().map(|e| e.id).collect()).unwrap_or_default();
                let earlier = found_objects.iter().filter(|(o, _)| o.kind == kind).filter_map(|(_, v)| v["id"].as_u64());
                claimed.extend(earlier);
                let mut found = client
                    .find_create_match(kind, &op.fields, &claimed, None)
                    .await
                    .with_context(|| format!("searching {kind} for '{slug}', created by an unfinished run"))?;
                let chosen = match found.len() {
                    0 => None,
                    1 => found.pop(),
                    _ => Some(choose(paths.env(), kind, slug, found, interactive, progress).await?),
                };
                match chosen {
                    Some(listed) => Some(client.fetch_listed(kind, listed, None).await?),
                    None => None,
                }
            }
        };
        let Some(object) = object else {
            progress.event(
                Action::Info,
                &format!("{kind}/{slug}: an unfinished run's create left nothing on the env; it {would}be created anew"),
            );
            report.dropped.push((op.kind, op.slug));
            continue;
        };
        found_objects.push((op, object));
    }
    if !dry_run {
        for (op, object) in &found_objects {
            let id = object["id"].as_u64().with_context(|| format!("{}/{} has no id", op.kind, op.slug))?;
            // An id-only entry; completing the op below records the rest.
            let entry = ObjectEntry { id, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None };
            lockfile.upsert(&op.kind, &op.slug, entry);
        }
    }
    let mut recovered_queues = Vec::new();
    for (op, object) in found_objects {
        let (kind, slug) = (op.kind.as_str(), op.slug.as_str());
        if kind == "queues" {
            recovered_queues.push((slug.to_string(), object["workspace"].as_str().map(str::to_string)));
        }
        let id = object["id"].as_u64().with_context(|| format!("{kind} for '{slug}' has no id"))?;
        if !dry_run {
            match local_file(paths, kind, slug) {
                Some(path) => adopt(paths, lockfile, kind, slug, &path, object)?,
                None => {
                    let path = new_file_path(paths, lockfile, client, kind, slug, &object).await?;
                    complete(paths, lockfile, kind, slug, &path, object)?;
                }
            }
        }
        progress.event(Action::Info, &format!("{would}recover {kind}/{slug} id={id} from an unfinished run"));
        report.completed.push((op.kind, op.slug, id));
    }
    if !dry_run && !recovered_queues.is_empty() {
        adopt_provisioned_templates(paths, lockfile, client, &recovered_queues, progress).await?;
    }
    Ok(report)
}

/// A queue's create makes the server provision its default email templates,
/// which the email-template driver adopts later in the same run. A run that
/// died in between left them unrecorded next to local files the classifier
/// would read as a conflict with each, so each local template of a recovered
/// queue with no entry yet adopts its remote twin the way that driver
/// matches it (`pick_adoption_id`), recorded as [`adopt`] records a create.
async fn adopt_provisioned_templates(
    paths: &Paths,
    lockfile: &mut Lockfile,
    client: &RossumClient,
    queues: &[(String, Option<String>)],
    progress: &Arc<Log>,
) -> Result<()> {
    let recorded = |lf: &Lockfile| -> std::collections::HashSet<u64> {
        lf.objects.get("email_templates").map(|m| m.values().map(|e| e.id).collect()).unwrap_or_default()
    };
    let mut remotes: Option<std::collections::HashMap<u64, crate::model::EmailTemplate>> = None;
    for (queue, workspace) in queues {
        let Some(ws) = workspace.as_deref().and_then(|url| lockfile.slug_for_url("workspaces", url)).map(str::to_string) else {
            continue;
        };
        let dir = paths.queue_email_templates_dir(&ws, queue);
        let Ok(entries) = crate::vfs::read_dir(&dir) else { continue };
        for entry in entries {
            let path = entry.path();
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).filter(|_| path.extension().is_some_and(|e| e == "json")) else {
                continue;
            };
            let key = format!("{ws}/{queue}/{name}");
            if lockfile.objects.get("email_templates").is_some_and(|m| m.contains_key(&key)) {
                continue;
            }
            let mut payload: Value = serde_json::from_slice(&crate::vfs::read(&path)?)
                .with_context(|| format!("parsing {}", path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            let local: crate::model::EmailTemplate = serde_json::from_value(payload)
                .with_context(|| format!("deserializing local email template '{key}'"))?;
            if remotes.is_none() {
                let listed = client.list_email_templates(None).await.context("listing email templates")?;
                remotes = Some(listed.into_iter().map(|t| (t.id, t)).collect());
            }
            let remotes = remotes.as_ref().expect("listed above");
            let local_type = local.extra.get("type").and_then(|v| v.as_str());
            let claimed = recorded(lockfile);
            let Some(remote) = crate::cli::push::email_templates::pick_adoption_id(remotes, &local.queue, local_type, &local.name, &claimed)
                .and_then(|id| remotes.get(&id))
            else {
                continue;
            };
            let id = remote.id;
            adopt(paths, lockfile, "email_templates", &key, &path, serde_json::to_value(remote)?)?;
            progress.event(Action::Info, &format!("recover email_templates/{key} id={id}, provisioned with its queue"));
        }
    }
    Ok(())
}

/// Several objects match one lost create. Interactively the user picks one,
/// or aborts; otherwise the sync stops and names every candidate.
async fn choose(
    env: &str,
    kind: &str,
    slug: &str,
    candidates: Vec<Value>,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<Value> {
    use crate::cli::stdin_coord::{Prompt, PromptKey, PromptKind};
    use std::io::Write;
    let ids: Vec<u64> = candidates.iter().filter_map(|v| v["id"].as_u64()).collect();
    let listed = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    let question = format!(
        "{kind}/{slug}: an unfinished run's create may have made any of {} {kind} (ids {listed})",
        ids.len()
    );
    // One digit key per candidate, so more than nine cannot be offered.
    if !interactive || ids.len() > 9 {
        anyhow::bail!(
            "{question}; rdc cannot tell which. Delete the extra ones on env '{env}', \
             or run `rdc sync {env}` in a terminal to choose."
        );
    }
    progress.event(Action::Warn, &question);
    let mut keys: Vec<PromptKey> = ids
        .iter()
        .zip('1'..='9')
        .map(|(id, key)| PromptKey::new(key, &format!("it is id {id}")))
        .collect();
    keys.push(PromptKey::new('a', "abort the sync"));
    // Its own kind, with the whole question: a consumer titles its dialog by
    // kind, and no other prompt asks which object a lost create made.
    crate::cli::stdin_coord::announce(Prompt {
        kind: PromptKind::RecoverCreate,
        question: format!("{question}. Which one did it create?"),
        keys: keys.clone(),
    });
    let mode = crate::cli::resolve::detect_color_mode();
    let mut q = progress.writer();
    write!(q, "{}", crate::cli::change_view::render_menu(&keys, mode)).ok();
    q.flush().ok();
    let ans = crate::cli::stdin_coord::read_line_coordinated()
        .await?
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let picked = keys
        .iter()
        .position(|k| k.key != 'a' && ans.len() == 1 && ans.starts_with(k.key))
        .or_else(|| ids.iter().position(|id| ans == id.to_string()));
    match picked {
        Some(i) => Ok(candidates.into_iter().nth(i).expect("a key per candidate")),
        None => anyhow::bail!("recovery of {kind}/{slug} aborted; the unfinished run's journal is left as it was"),
    }
}

/// The local env file of `kind`/`slug`, if this tree has one. Laid out as
/// the push scanner finds it; queue-nested kinds are swept for by queue slug.
fn local_file(paths: &Paths, kind: &str, slug: &str) -> Option<PathBuf> {
    use crate::cli::push::scan::find_queue_nested_path;
    let flat = |dir: PathBuf| dir.join(format!("{slug}.json"));
    let path = match kind {
        "workspaces" => paths.workspace_dir(slug).join("workspace.json"),
        "hooks" => flat(paths.hooks_dir()),
        "rules" => flat(paths.rules_dir()),
        "labels" => flat(paths.labels_dir()),
        "saved_views" => flat(paths.saved_views_dir()),
        "engines" => paths.engine_dir(slug).join("engine.json"),
        "engine_fields" => {
            let (engine, field) = slug.split_once('/')?;
            paths.engine_fields_dir(engine).join(format!("{field}.json"))
        }
        "email_templates" => match slug.splitn(3, '/').collect::<Vec<_>>()[..] {
            [ws, queue, template] => paths.queue_email_templates_dir(ws, queue).join(format!("{template}.json")),
            _ => return None,
        },
        "queues" => return find_queue_nested_path(paths, slug, "queue.json"),
        "schemas" => return find_queue_nested_path(paths, slug, "schema.json"),
        "inboxes" => return find_queue_nested_path(paths, slug, "inbox.json"),
        _ => return None,
    };
    crate::vfs::exists(&path).then_some(path)
}

/// Where a recovered object with no local file goes: the layout the pull
/// driver gives it. A queue sits under the workspace it names; a schema or
/// inbox under its queue's directory — found locally (tracked, or written
/// earlier in this pass), else from the queue's workspace on the env. An
/// unplaceable object is an error, so the op stays in the journal.
async fn new_file_path(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    kind: &str,
    slug: &str,
    object: &Value,
) -> Result<PathBuf> {
    use crate::cli::push::scan::find_queue_nested_path;
    let flat = |dir: PathBuf| dir.join(format!("{slug}.json"));
    let in_workspace = |workspace: Option<&str>| {
        let ws = lockfile.slug_for_url("workspaces", workspace?)?;
        Some(paths.queue_dir(ws, slug))
    };
    let queue_dir = match kind {
        "schemas" | "inboxes" => match find_queue_nested_path(paths, slug, "queue.json") {
            Some(p) => p.parent().map(PathBuf::from),
            None => match lockfile.objects.get("queues").and_then(|m| m.get(slug)) {
                Some(q) => client
                    .get_created("queues", q.id, None)
                    .await?
                    .and_then(|queue| in_workspace(queue["workspace"].as_str())),
                None => None,
            },
        },
        _ => None,
    };
    let path = match kind {
        "workspaces" => Some(paths.workspace_dir(slug).join("workspace.json")),
        "hooks" => Some(flat(paths.hooks_dir())),
        "rules" => Some(flat(paths.rules_dir())),
        "labels" => Some(flat(paths.labels_dir())),
        "saved_views" => Some(flat(paths.saved_views_dir())),
        "engines" => Some(paths.engine_dir(slug).join("engine.json")),
        "engine_fields" => slug
            .split_once('/')
            .map(|(engine, field)| paths.engine_fields_dir(engine).join(format!("{field}.json"))),
        "email_templates" => match slug.splitn(3, '/').collect::<Vec<_>>()[..] {
            [ws, queue, template] => {
                Some(paths.queue_email_templates_dir(ws, queue).join(format!("{template}.json")))
            }
            _ => None,
        },
        "queues" => in_workspace(object["workspace"].as_str()).map(|d| d.join("queue.json")),
        "schemas" => queue_dir.map(|d| d.join("schema.json")),
        "inboxes" => queue_dir.map(|d| d.join("inbox.json")),
        _ => None,
    };
    path.with_context(|| {
        format!(
            "{kind}/{slug} (id {}) was created by an unfinished run, but rdc cannot tell where \
             it belongs in this tree; the journal is kept",
            object["id"]
        )
    })
}

/// Complete the create of `object` at `path`, which has no local file yet,
/// exactly as the `kind` driver does after a successful POST. A hook's
/// `secrets_hash` stays unset, as in [`adopt`].
fn complete(paths: &Paths, lockfile: &mut Lockfile, kind: &str, slug: &str, path: &Path, object: Value) -> Result<()> {
    use crate::cli::push;
    match kind {
        "workspaces" => push::workspaces::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "queues" => push::queues::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "schemas" => push::schemas::complete_create(lockfile, slug, path, &typed(kind, object)?),
        "inboxes" => push::inboxes::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "rules" => push::rules::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "labels" => push::labels::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "email_templates" => push::email_templates::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "saved_views" => push::saved_views::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "engines" => push::engines::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "engine_fields" => push::engine_fields::complete_create(paths, lockfile, slug, path, &typed(kind, object)?),
        "hooks" => push::hooks::complete_create(paths, lockfile, slug, path, &typed(kind, object)?, None),
        other => anyhow::bail!("an unfinished run journaled a create of '{other}', which rdc does not create"),
    }
}

/// `object` decoded into the model the `kind` create method returns.
fn typed<T: serde::de::DeserializeOwned>(kind: &str, object: Value) -> Result<T> {
    serde_json::from_value(object).with_context(|| format!("decoding a recovered {kind} object"))
}

/// Record `object` for the local file at `path` as the `kind` driver would
/// after a successful POST — the entry its `created_form` computes, and the
/// base-cache copies the driver keeps — while leaving the env file as it is.
///
/// A hook's `secrets_hash` stays unset: the crashed run may not have sent the
/// secrets (a store extension gets them only on its PATCH), so the hooks
/// secrets pass sends them again.
fn adopt(paths: &Paths, lockfile: &mut Lockfile, kind: &str, slug: &str, path: &Path, object: Value) -> Result<()> {
    use crate::cli::push;
    let cache = |file: &Path, bytes: &[u8]| {
        crate::state::base_cache::write(paths, file, bytes)
            .with_context(|| format!("caching the recovered base of {kind}/{slug}"))
    };
    let json_only = |form: push::CreatedForm| -> Result<ObjectEntry> {
        cache(path, &form.json)?;
        Ok(form.entry)
    };
    let entry = match kind {
        "workspaces" => json_only(push::workspaces::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        "queues" => json_only(push::queues::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        "inboxes" => json_only(push::inboxes::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        "labels" => json_only(push::labels::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        "email_templates" => json_only(push::email_templates::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        "saved_views" => json_only(push::saved_views::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        "engines" => json_only(push::engines::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        "engine_fields" => json_only(push::engine_fields::created_form(lockfile, slug, &typed(kind, object)?)?)?,
        // The create path keeps no base copy of a schema either.
        "schemas" => push::schemas::created_form(lockfile, slug, &typed(kind, object)?)?.entry,
        "rules" => {
            let form = push::rules::created_form(lockfile, slug, &typed(kind, object)?)?;
            push::rules::cache_created_code(paths, slug, &form)?;
            json_only(push::CreatedForm { entry: form.entry, json: form.json, sidecar: () })?
        }
        "hooks" => {
            let form = push::hooks::created_form(lockfile, slug, &typed(kind, object)?)?;
            push::hooks::cache_created_code(paths, slug, &form)?;
            json_only(push::CreatedForm { entry: form.entry, json: form.json, sidecar: () })?
        }
        other => anyhow::bail!("an unfinished run journaled a create of '{other}', which rdc does not create"),
    };
    lockfile.upsert(kind, slug, entry);
    Ok(())
}
