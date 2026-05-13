//! Per-kind cross-env POST flows used by `rdc deploy` to bootstrap missing
//! resources in the target environment. Each function:
//!
//! 1. Reads the src on-disk file.
//! 2. Rewrites cross-reference URLs (src URLs → tgt URLs) using the
//!    `Mapping` and the *currently-known* `Lockfile` of the target — which
//!    grows during deploy as earlier kinds get created.
//! 3. Applies the tgt overlay (`apply_overrides`) so per-env values land.
//! 4. Strips server-managed fields with `strip_for_create`.
//! 5. POSTs and writes the server's canonical response back to the tgt
//!    on-disk snapshot.
//! 6. Inserts an entry into the in-memory tgt `Lockfile` and adds a
//!    same-slug entry to the in-memory `Mapping` so the NEXT kind's URL
//!    rewriter can reach it.
//!
//! The dependency order matches `rdc push` (`workspaces → schemas → queues →
//! inboxes → email templates → hooks → rules → labels → engines → engine
//! fields`) so each kind's `rewrite_urls` step always finds its peers
//! already in tgt.
//!
//! Side-effect note (Rossum behaviour): `POST /queues` auto-creates 5
//! default email templates per queue. `refresh_queue_email_templates`
//! captures them into the tgt lockfile + writes their local files so the
//! later email-templates phase sees them as "existing slugs" to PATCH
//! rather than trying (and failing) to POST duplicates.

use crate::api::RossumClient;
use crate::cli::deploy::common::rewrite_urls;
use crate::mapping::Mapping;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::create::strip_for_create;
use crate::snapshot::email_template::write_email_template;
use crate::snapshot::hook::{read_hook_value, write_hook};
use crate::snapshot::rule::{read_rule_value, serialize_rule, write_rule};
use crate::snapshot::schema::{read_schema_value, serialize_schema, write_schema_bytes};
use crate::snapshot::writer::write_atomic;
use crate::state::{content_hash, hook_combined_hash, rule_combined_hash, schema_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use serde_json::Value;

/// Common bundle threaded through every per-kind create function. Keeps
/// signatures readable.
pub struct CreateCtx<'a> {
    pub src_paths: &'a Paths,
    pub tgt_paths: &'a Paths,
    pub src_lockfile: &'a Lockfile,
    pub tgt_lockfile: &'a mut Lockfile,
    pub mapping: &'a mut Mapping,
    pub tgt_overlay: &'a Option<Overlay>,
    pub tgt_client: &'a RossumClient,
}

/// Walk a src payload, rewrite cross-refs to tgt URLs, apply overlay,
/// strip server fields. Returns the body ready to POST. Centralises the
/// pre-POST shaping so each kind's create function stays focused on its
/// I/O specifics.
fn shape_create_body(
    raw: Value,
    kind: &str,
    overlay_paths: Option<&std::collections::BTreeMap<String, Value>>,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
) -> Value {
    let mut payload = raw;
    rewrite_urls(&mut payload, src_lockfile, tgt_lockfile, mapping);
    if let Some(p) = overlay_paths {
        apply_overrides(&mut payload, p);
    }
    strip_for_create(&mut payload, kind);
    payload
}

pub async fn create_workspace(ctx: &mut CreateCtx<'_>, slug: &str) -> Result<()> {
    let path = ctx.src_paths.workspace_dir(slug).join("workspace.json");
    let raw: Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    // Workspaces aren't a section in the overlay schema today, so no
    // per-workspace overrides apply on create.
    let body = shape_create_body(raw, "workspaces", None, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping);
    let created = ctx
        .tgt_client
        .create_workspace(&body, None)
        .await
        .with_context(|| format!("POST /workspaces (creating '{slug}')"))?;
    let mut bytes = serde_json::to_vec_pretty(&created).context("serializing created workspace")?;
    bytes.push(b'\n');
    let tgt_file = ctx.tgt_paths.workspace_dir(slug).join("workspace.json");
    write_atomic(&tgt_file, &bytes)?;
    ctx.tgt_lockfile.upsert(
        "workspaces",
        slug,
        ObjectEntry {
            id: created.id,
            url: Some(created.url.clone()),
            modified_at: created.modified_at().map(|s| s.to_string()),
            content_hash: Some(content_hash(&bytes)),
        },
    );
    ctx.mapping.workspaces.insert(slug.to_string(), slug.to_string());
    Ok(())
}

pub async fn create_schema(ctx: &mut CreateCtx<'_>, queue_slug: &str) -> Result<()> {
    // Schemas live under the queue dir on disk; the slug is the queue slug.
    let Some(src_queue_dir) = locate_queue_dir(ctx.src_paths, queue_slug) else {
        anyhow::bail!("schema dir for '{queue_slug}' not found in src");
    };
    let mut payload = read_schema_value(&src_queue_dir)
        .with_context(|| format!("reading src schema '{queue_slug}'"))?;
    let overlay_paths = ctx.tgt_overlay.as_ref().and_then(|ov| ov.schema(queue_slug));
    payload = shape_create_body(payload, "schemas", overlay_paths, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping);
    let created = ctx
        .tgt_client
        .create_schema(&payload, None)
        .await
        .with_context(|| format!("POST /schemas (creating '{queue_slug}')"))?;
    // The schema lives under the *tgt* queue dir of the same slug. The
    // queue may not exist yet at this point — write_schema only needs the
    // queue dir to exist.
    let ws_slug = src_queue_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .context("locating src ws slug for schema")?;
    let tgt_queue_dir = ctx.tgt_paths.queue_dir(&ws_slug, queue_slug);
    std::fs::create_dir_all(&tgt_queue_dir)
        .with_context(|| format!("creating {}", tgt_queue_dir.display()))?;
    let (json_bytes, formula_parts) = serialize_schema(&created)?;
    write_schema_bytes(&tgt_queue_dir, &json_bytes, &formula_parts)?;
    let h = schema_combined_hash(&json_bytes, &formula_parts);
    ctx.tgt_lockfile.upsert(
        "schemas",
        queue_slug,
        ObjectEntry {
            id: created.id,
            url: Some(created.url.clone()),
            modified_at: created.modified_at().map(|s| s.to_string()),
            content_hash: Some(h),
        },
    );
    ctx.mapping.schemas.insert(queue_slug.to_string(), queue_slug.to_string());
    Ok(())
}

pub async fn create_queue(ctx: &mut CreateCtx<'_>, queue_slug: &str) -> Result<()> {
    let Some(src_queue_dir) = locate_queue_dir(ctx.src_paths, queue_slug) else {
        anyhow::bail!("queue dir for '{queue_slug}' not found in src");
    };
    let queue_path = src_queue_dir.join("queue.json");
    let raw: Value = serde_json::from_slice(
        &std::fs::read(&queue_path).with_context(|| format!("reading {}", queue_path.display()))?,
    )
    .with_context(|| format!("parsing {}", queue_path.display()))?;
    let overlay_paths = ctx.tgt_overlay.as_ref().and_then(|ov| ov.queue(queue_slug));
    let body = shape_create_body(raw, "queues", overlay_paths, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping);
    let created = ctx
        .tgt_client
        .create_queue(&body, None)
        .await
        .with_context(|| format!("POST /queues (creating '{queue_slug}')"))?;
    let ws_slug = src_queue_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .context("locating src ws slug for queue")?;
    let tgt_queue_dir = ctx.tgt_paths.queue_dir(&ws_slug, queue_slug);
    std::fs::create_dir_all(&tgt_queue_dir)
        .with_context(|| format!("creating {}", tgt_queue_dir.display()))?;
    let tgt_file = tgt_queue_dir.join("queue.json");
    let mut bytes = serde_json::to_vec_pretty(&created).context("serializing created queue")?;
    bytes.push(b'\n');
    write_atomic(&tgt_file, &bytes)?;
    ctx.tgt_lockfile.upsert(
        "queues",
        queue_slug,
        ObjectEntry {
            id: created.id,
            url: Some(created.url.clone()),
            modified_at: created.modified_at().map(|s| s.to_string()),
            content_hash: Some(content_hash(&bytes)),
        },
    );
    ctx.mapping.queues.insert(queue_slug.to_string(), queue_slug.to_string());

    // Rossum auto-creates 5 default email templates per new queue. Capture
    // them now so the later email-templates phase sees them as existing
    // (PATCH) rather than trying to POST duplicates.
    refresh_queue_email_templates(ctx, &ws_slug, queue_slug, created.id).await?;

    Ok(())
}

async fn refresh_queue_email_templates(
    ctx: &mut CreateCtx<'_>,
    ws_slug: &str,
    queue_slug: &str,
    queue_id: u64,
) -> Result<()> {
    let queue_url_path = format!("queues/{queue_id}");
    let all = ctx
        .tgt_client
        .list_email_templates(None)
        .await
        .context("listing tgt email templates after queue create")?;
    let templates_dir = ctx.tgt_paths.queue_email_templates_dir(ws_slug, queue_slug);
    std::fs::create_dir_all(&templates_dir)
        .with_context(|| format!("creating {}", templates_dir.display()))?;
    for t in all {
        let Some(q_url) = t.queue.as_deref() else { continue };
        if !q_url.contains(&queue_url_path) {
            continue;
        }
        let t_slug = crate::slug::slugify(&t.name);
        let key = format!("{ws_slug}/{queue_slug}/{t_slug}");
        let bytes = write_email_template(&templates_dir, &t_slug, &t)?;
        ctx.tgt_lockfile.upsert(
            "email_templates",
            &key,
            ObjectEntry {
                id: t.id,
                url: Some(t.url.clone()),
                modified_at: t.modified_at().map(|s| s.to_string()),
                content_hash: Some(content_hash(&bytes)),
            },
        );
        ctx.mapping
            .email_templates
            .insert(key.clone(), key);
    }
    Ok(())
}

pub async fn create_inbox(ctx: &mut CreateCtx<'_>, queue_slug: &str) -> Result<()> {
    let Some(src_queue_dir) = locate_queue_dir(ctx.src_paths, queue_slug) else {
        anyhow::bail!("inbox dir for '{queue_slug}' not found in src");
    };
    let inbox_path = src_queue_dir.join("inbox.json");
    let raw: Value = serde_json::from_slice(
        &std::fs::read(&inbox_path).with_context(|| format!("reading {}", inbox_path.display()))?,
    )
    .with_context(|| format!("parsing {}", inbox_path.display()))?;
    let overlay_paths = ctx.tgt_overlay.as_ref().and_then(|ov| ov.inbox(queue_slug));
    let body = shape_create_body(raw, "inboxes", overlay_paths, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping);
    let created = ctx
        .tgt_client
        .create_inbox(&body, None)
        .await
        .with_context(|| format!("POST /inboxes (creating '{queue_slug}')"))?;
    let ws_slug = src_queue_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .context("locating src ws slug for inbox")?;
    let tgt_inbox = ctx.tgt_paths.queue_dir(&ws_slug, queue_slug).join("inbox.json");
    let mut bytes = serde_json::to_vec_pretty(&created).context("serializing created inbox")?;
    bytes.push(b'\n');
    write_atomic(&tgt_inbox, &bytes)?;
    ctx.tgt_lockfile.upsert(
        "inboxes",
        queue_slug,
        ObjectEntry {
            id: created.id,
            url: Some(created.url.clone()),
            modified_at: created.modified_at().map(|s| s.to_string()),
            content_hash: Some(content_hash(&bytes)),
        },
    );
    ctx.mapping.inboxes.insert(queue_slug.to_string(), queue_slug.to_string());
    Ok(())
}

pub async fn create_hook(ctx: &mut CreateCtx<'_>, slug: &str) -> Result<()> {
    let payload = read_hook_value(&ctx.src_paths.hooks_dir(), slug)
        .with_context(|| format!("reading src hook '{slug}'"))?;
    let overlay_paths = ctx.tgt_overlay.as_ref().and_then(|ov| ov.hook(slug));
    let body = shape_create_body(payload, "hooks", overlay_paths, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping);
    let created = ctx
        .tgt_client
        .create_hook(&body, None)
        .await
        .with_context(|| format!("POST /hooks (creating '{slug}')"))?;
    let tgt_hooks_dir = ctx.tgt_paths.hooks_dir();
    std::fs::create_dir_all(&tgt_hooks_dir)
        .with_context(|| format!("creating {}", tgt_hooks_dir.display()))?;
    let json_bytes = write_hook(&tgt_hooks_dir, slug, &created)?;
    let code = created
        .config
        .get("code")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let h = hook_combined_hash(&json_bytes, &code);
    ctx.tgt_lockfile.upsert(
        "hooks",
        slug,
        ObjectEntry {
            id: created.id,
            url: Some(created.url.clone()),
            modified_at: created.modified_at().map(|s| s.to_string()),
            content_hash: Some(h),
        },
    );
    ctx.mapping.hooks.insert(slug.to_string(), slug.to_string());
    Ok(())
}

pub async fn create_rule(ctx: &mut CreateCtx<'_>, slug: &str) -> Result<()> {
    let payload = read_rule_value(&ctx.src_paths.rules_dir(), slug)
        .with_context(|| format!("reading src rule '{slug}'"))?;
    let overlay_paths = ctx.tgt_overlay.as_ref().and_then(|ov| ov.rule(slug));
    let body = shape_create_body(payload, "rules", overlay_paths, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping);
    let created = ctx
        .tgt_client
        .create_rule(&body, None)
        .await
        .with_context(|| format!("POST /rules (creating '{slug}')"))?;
    let tgt_rules_dir = ctx.tgt_paths.rules_dir();
    std::fs::create_dir_all(&tgt_rules_dir)
        .with_context(|| format!("creating {}", tgt_rules_dir.display()))?;
    let (json_bytes, code) = serialize_rule(&created)?;
    write_rule(&tgt_rules_dir, slug, &created)?;
    let h = rule_combined_hash(&json_bytes, &code);
    ctx.tgt_lockfile.upsert(
        "rules",
        slug,
        ObjectEntry {
            id: created.id,
            url: Some(created.url.clone()),
            modified_at: created.modified_at().map(|s| s.to_string()),
            content_hash: Some(h),
        },
    );
    ctx.mapping.rules.insert(slug.to_string(), slug.to_string());
    Ok(())
}

pub async fn create_label(ctx: &mut CreateCtx<'_>, slug: &str) -> Result<()> {
    let path = ctx.src_paths.labels_dir().join(format!("{slug}.json"));
    let raw: Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    let overlay_paths = ctx.tgt_overlay.as_ref().and_then(|ov| ov.label(slug));
    let body = shape_create_body(raw, "labels", overlay_paths, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping);
    let created = ctx
        .tgt_client
        .create_label(&body, None)
        .await
        .with_context(|| format!("POST /labels (creating '{slug}')"))?;
    let tgt_labels_dir = ctx.tgt_paths.labels_dir();
    std::fs::create_dir_all(&tgt_labels_dir)
        .with_context(|| format!("creating {}", tgt_labels_dir.display()))?;
    let tgt_file = tgt_labels_dir.join(format!("{slug}.json"));
    let mut bytes = serde_json::to_vec_pretty(&created).context("serializing created label")?;
    bytes.push(b'\n');
    write_atomic(&tgt_file, &bytes)?;
    ctx.tgt_lockfile.upsert(
        "labels",
        slug,
        ObjectEntry {
            id: created.id,
            url: Some(created.url.clone()),
            modified_at: created.modified_at().map(|s| s.to_string()),
            content_hash: Some(content_hash(&bytes)),
        },
    );
    ctx.mapping.labels.insert(slug.to_string(), slug.to_string());
    Ok(())
}

/// Helper: find queue dir under either env's `workspaces/<ws>/queues/<q>/`.
pub fn locate_queue_dir(paths: &Paths, queue_slug: &str) -> Option<std::path::PathBuf> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return None;
    }
    for ws_entry in std::fs::read_dir(&ws_dir).ok()? {
        let Ok(ws_entry) = ws_entry else { continue };
        if !ws_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let queue_dir = ws_entry.path().join("queues").join(queue_slug);
        if queue_dir.join("queue.json").exists() || queue_dir.is_dir() {
            return Some(queue_dir);
        }
    }
    None
}
