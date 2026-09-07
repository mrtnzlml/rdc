pub mod data_storage;
pub mod error;
pub mod rate_limit;
pub mod retry;

pub use data_storage::DataStorageClient;
pub use error::{anyhow_has_status, anyhow_status_env, ApiError};
pub use rate_limit::RateLimiter;

use crate::model::{
    EmailTemplate, Engine, EngineField, Hook, HookTemplate, Inbox, Label, Organization, Queue,
    Rule, SavedView, Schema, User, Workflow, WorkflowStep, Workspace,
};
use crate::api::retry::ProgressHandle;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;

/// Rossum API client. Holds a base URL (e.g. `https://X.rossum.app/api/v1`)
/// and a static API token. Pagination is followed transparently for `list_*`
/// methods; PATCH and POST calls go through shared `patch_json`/`post_json`
/// helpers which retry on 429 / 502 / 503 / 504 via `retry::send_with_retry`.
pub struct RossumClient {
    base_url: String,
    token: String,
    http: Client,
    /// Env name (e.g. `"dev-eu"`) attached to errors as
    /// `ApiError::Status { env, .. }` so a caller juggling multiple clients
    /// can attribute a 401 back to the right env and refresh its token.
    /// `None` — the state every command is in today — means errors are not
    /// env-tagged. Set only via [`RossumClient::with_env_label`].
    env: Option<String>,
    /// Per-token client-side rate limiter. Defaults to the
    /// `default.core_api` policy Rossum enforces server-side (10 req/s,
    /// burst 10). `Arc` so all in-flight calls share one bucket; two
    /// clients (e.g. deploy's src + tgt) get independent buckets which
    /// matches the server's per-token scope.
    ///
    /// `None` when [`is_loopback_base`] says this client talks to a local
    /// mock, which enforces no policy to respect.
    limiter: Option<Arc<RateLimiter>>,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    pagination: Pagination,
    results: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    next: Option<String>,
    /// Rossum returns `total_pages` on every core list endpoint (verified
    /// 2026-06-01). Absent/0 ⇒ fall back to sequential `next`-cursor follow.
    #[serde(default)]
    total_pages: u64,
}

/// Rossum caps list `page_size` at 100 (default 20); always request the max to
/// minimize round-trips. Verified against the live API 2026-06-01.
const LIST_PAGE_SIZE: u64 = 100;

/// Bound on concurrently in-flight page fetches within one list call. True
/// throughput is capped by the per-token `RateLimiter` (10 req/s); this only
/// bounds how many page requests are outstanding at once.
const LIST_PAGE_FANOUT: usize = 5;

/// True when `base_url`'s host is a loopback address (or `localhost`) —
/// i.e. a `wiremock` server owned by a test, not a Rossum cluster.
///
/// Every wall-clock cost rdc pays on the wire models one specific remote
/// behavior: the core API's 10 req/s policy, Data Storage's 30 req/s, and
/// Data Storage's asynchronous index builds. A local mock has none of
/// them, so paying those costs against one buys nothing and just makes
/// the suite sleep — measured on `tests/cli_sync.rs`, pacing alone cost
/// 37 s of its 70 s and the materialization ceilings another 18 s.
///
/// Gating on the URL rather than an env var is deliberate: Rossum is never
/// on loopback, so this cannot relax pacing against a real cluster no
/// matter how rdc is invoked, and it adds no knob a deploy could set by
/// mistake. The tests that assert pacing *is* wired in opt back into a
/// real bucket explicitly (see `DataStorageClient::paced_for_test`).
pub(crate) fn is_loopback_base(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).ok().and_then(|u| u.host_str().map(str::to_string)).is_some_and(
        |h| {
            // `host_str` brackets IPv6 literals (`[::1]`), which `IpAddr`
            // will not parse; strip them before asking.
            let bare = h.strip_prefix('[').and_then(|b| b.strip_suffix(']')).unwrap_or(&h);
            bare == "localhost"
                || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        },
    )
}

/// Construct the shared reqwest Client used by every rdc HTTP path.
///
/// Single source of truth so a future change (timeout, user-agent, TLS
/// tweak, proxy config) lands in one place instead of three.
///
/// Nagle is disabled: rdc's hot path is many small JSON requests
/// (pagination + per-resource fetches), and the ~40 ms segment-delay
/// Nagle adds compounds noticeably across hundreds of round-trips
/// during a full sync.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .tcp_nodelay(true)
        .build()
        .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))
}

impl RossumClient {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = build_http_client()?;
        let limiter =
            (!is_loopback_base(&base_url)).then(|| Arc::new(RateLimiter::rossum_core_api()));
        Ok(Self { base_url, token, http, env: None, limiter })
    }

    /// Attach an env label so any non-2xx error this client produces
    /// carries the env name in its `ApiError::Status { env, .. }`, so a
    /// retry wrapper knows which env's token to refresh on a 401.
    ///
    /// Written for a command holding two clients (src + tgt). The command
    /// that did — `rdc deploy` — was replaced by the offline `rdc migrate`,
    /// which opens no client at all, so this has **no caller today**; it is
    /// kept for the next multi-env caller rather than as live behavior.
    pub fn with_env_label(mut self, env: impl Into<String>) -> Self {
        self.env = Some(env.into());
        self
    }

    // --- list endpoints (paginated) -----------------------------------

    pub async fn list_hooks(&self, progress: ProgressHandle) -> Result<Vec<Hook>> {
        self.list_paginated("/hooks", progress).await
    }

    pub async fn list_workspaces(&self, progress: ProgressHandle) -> Result<Vec<Workspace>> {
        self.list_paginated("/workspaces", progress).await
    }

    pub async fn list_queues(&self, progress: ProgressHandle) -> Result<Vec<Queue>> {
        self.list_paginated("/queues", progress).await
    }

    pub async fn list_rules(&self, progress: ProgressHandle) -> Result<Vec<Rule>> {
        self.list_paginated("/rules", progress).await
    }

    pub async fn list_labels(&self, progress: ProgressHandle) -> Result<Vec<Label>> {
        self.list_paginated("/labels", progress).await
    }

    pub async fn list_saved_views(&self, progress: ProgressHandle) -> Result<Vec<SavedView>> {
        self.list_paginated("/saved_views", progress).await
    }

    pub async fn list_engines(&self, progress: ProgressHandle) -> Result<Vec<Engine>> {
        self.list_paginated("/engines", progress).await
    }

    pub async fn list_engine_fields(&self, progress: ProgressHandle) -> Result<Vec<EngineField>> {
        self.list_paginated("/engine_fields", progress).await
    }

    pub async fn list_workflows(&self, progress: ProgressHandle) -> Result<Vec<Workflow>> {
        self.list_paginated("/workflows", progress).await
    }

    pub async fn list_workflow_steps(&self, progress: ProgressHandle) -> Result<Vec<WorkflowStep>> {
        self.list_paginated("/workflow_steps", progress).await
    }

    pub async fn list_email_templates(&self, progress: ProgressHandle) -> Result<Vec<EmailTemplate>> {
        self.list_paginated("/email_templates", progress).await
    }

    pub async fn list_hook_templates(&self, progress: ProgressHandle) -> Result<Vec<HookTemplate>> {
        self.list_paginated("/hook_templates", progress).await
    }

    pub async fn list_inboxes(&self, progress: ProgressHandle) -> Result<Vec<Inbox>> {
        self.list_paginated("/inboxes", progress).await
    }

    pub async fn list_users(&self, progress: ProgressHandle) -> Result<Vec<User>> {
        self.list_paginated("/users", progress).await
    }

    // --- get endpoints ------------------------------------------------

    pub async fn get_organization(&self, id: u64, progress: ProgressHandle) -> Result<Organization> {
        self.get_json(&format!("{}/organizations/{id}", self.base_url), progress).await
    }

    pub async fn get_hook(&self, id: u64, progress: ProgressHandle) -> Result<Hook> {
        self.get_json(&format!("{}/hooks/{id}", self.base_url), progress).await
    }

    pub async fn get_workspace(&self, id: u64, progress: ProgressHandle) -> Result<Workspace> {
        self.get_json(&format!("{}/workspaces/{id}", self.base_url), progress).await
    }

    pub async fn get_inbox(&self, id: u64, progress: ProgressHandle) -> Result<Inbox> {
        self.get_json(&format!("{}/inboxes/{id}", self.base_url), progress).await
    }

    pub async fn get_schema(&self, id: u64, progress: ProgressHandle) -> Result<Schema> {
        self.get_json(&format!("{}/schemas/{id}", self.base_url), progress).await
    }

    /// `GET /hooks/<id>/secrets_keys` — list the secret key names
    /// configured on a hook. The Rossum API returns the keys only, never
    /// the values (those are server-side encrypted). Used by deploy to
    /// check that the target env has values for every key the source
    /// hook depends on before any write hits the target.
    ///
    /// Path note: the Rossum endpoint is `/secrets_keys` (with `s` on
    /// `secrets`, no hyphen) — verified against the live API and the
    /// existing rossum-api MCP server source. The simpler-looking
    /// variants (`/secrets`, `/secret_keys`, `/secret-keys`) all 404.
    pub async fn get_hook_secrets_keys(&self, id: u64, progress: ProgressHandle) -> Result<Vec<String>> {
        self.get_json(&format!("{}/hooks/{id}/secrets_keys", self.base_url), progress).await
    }

    // --- create endpoints ---------------------------------------------

    pub async fn create_hook(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Hook> {
        self.post_json("/hooks", body, progress).await
    }

    /// POST `/hooks/create` — the Rossum store install endpoint. Unlike
    /// `create_hook` (which posts to `/hooks/`), this accepts a minimal body
    /// `{name, hook_template, events, queues, token_owner}` and the server
    /// fills in the rest from the referenced template (per the template's
    /// `install_action: "copy"`). Required for store extensions because
    /// `POST /hooks/` rejects them with 400 (`config.url` is required for
    /// webhook-type hooks, but store webhooks have `config.private: true`
    /// and no URL).
    pub async fn create_hook_via_install(
        &self,
        body: &serde_json::Value,
        progress: ProgressHandle,
    ) -> Result<Hook> {
        self.post_json("/hooks/create", body, progress).await
    }

    pub async fn create_workspace(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Workspace> {
        self.post_json("/workspaces", body, progress).await
    }

    pub async fn create_queue(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Queue> {
        self.post_json("/queues", body, progress).await
    }

    pub async fn create_schema(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Schema> {
        self.post_json("/schemas", body, progress).await
    }

    pub async fn create_inbox(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Inbox> {
        self.post_json("/inboxes", body, progress).await
    }

    pub async fn create_label(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Label> {
        self.post_json("/labels", body, progress).await
    }

    pub async fn create_saved_view(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<SavedView> {
        self.post_json("/saved_views", body, progress).await
    }

    pub async fn create_rule(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Rule> {
        self.post_json("/rules", body, progress).await
    }

    pub async fn create_email_template(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<EmailTemplate> {
        self.post_json("/email_templates", body, progress).await
    }

    pub async fn create_engine(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Engine> {
        self.post_json("/engines", body, progress).await
    }

    pub async fn create_engine_field(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<EngineField> {
        self.post_json("/engine_fields", body, progress).await
    }

    // --- update endpoints (PATCH) -------------------------------------

    pub async fn update_hook(&self, id: u64, hook: &Hook, progress: ProgressHandle) -> Result<Hook> {
        self.patch_json(&format!("/hooks/{id}"), hook, progress).await
    }

    /// `PATCH /hooks/<id>` with a raw JSON body. Used when the outbound
    /// payload contains fields not represented on the `Hook` model —
    /// notably the write-only top-level `secrets` map, which `GET /hooks`
    /// never returns and which therefore has no place on the typed
    /// model. The body is sent through the same retry pipeline as
    /// `update_hook` and the response is decoded back to a `Hook`.
    pub async fn update_hook_value(&self, id: u64, body: &serde_json::Value, progress: ProgressHandle) -> Result<Hook> {
        self.patch_json(&format!("/hooks/{id}"), body, progress).await
    }

    pub async fn update_workspace(&self, id: u64, workspace: &Workspace, progress: ProgressHandle) -> Result<Workspace> {
        self.patch_json(&format!("/workspaces/{id}"), workspace, progress).await
    }

    pub async fn update_queue(&self, id: u64, queue: &Queue, progress: ProgressHandle) -> Result<Queue> {
        self.patch_json(&format!("/queues/{id}"), queue, progress).await
    }

    pub async fn update_schema(&self, id: u64, schema: &Schema, progress: ProgressHandle) -> Result<Schema> {
        self.patch_json(&format!("/schemas/{id}"), schema, progress).await
    }

    pub async fn update_inbox(&self, id: u64, inbox: &Inbox, progress: ProgressHandle) -> Result<Inbox> {
        self.patch_json(&format!("/inboxes/{id}"), inbox, progress).await
    }

    /// Partial-body variant: PATCH with a hand-built `serde_json::Value`.
    /// Deploy uses this so it can strip per-env fields like `email`
    /// (auto-assigned at create, immutable in practice; sending the src
    /// env's value cross-env at best is ignored, at worst rewrites the
    /// tgt inbox's email). Mirror of [`update_hook_value`].
    pub async fn update_inbox_value(&self, id: u64, body: &serde_json::Value, progress: ProgressHandle) -> Result<Inbox> {
        self.patch_json(&format!("/inboxes/{id}"), body, progress).await
    }

    pub async fn update_email_template(&self, id: u64, t: &EmailTemplate, progress: ProgressHandle) -> Result<EmailTemplate> {
        self.patch_json(&format!("/email_templates/{id}"), t, progress).await
    }

    pub async fn update_rule(&self, id: u64, rule: &Rule, progress: ProgressHandle) -> Result<Rule> {
        self.patch_json(&format!("/rules/{id}"), rule, progress).await
    }

    pub async fn update_label(&self, id: u64, label: &Label, progress: ProgressHandle) -> Result<Label> {
        self.patch_json(&format!("/labels/{id}"), label, progress).await
    }

    /// `PATCH /saved_views/{id}`.
    ///
    /// There is no `delete_saved_view`: `push::deletes` issues DELETE through
    /// the generic `delete_path("/{kind}/{id}")`, and the kind string
    /// `saved_views` is already the correct path segment.
    pub async fn update_saved_view(&self, id: u64, view: &SavedView, progress: ProgressHandle) -> Result<SavedView> {
        self.patch_json(&format!("/saved_views/{id}"), view, progress).await
    }

    /// `PATCH /organizations/{id}`.
    ///
    /// The only write rdc ever makes to an organization: there is no POST (one
    /// org per env, created outside rdc) and no DELETE. `body` carries exactly
    /// the subtree rdc manages — `{"settings": …}` — because a partial
    /// `settings` PATCH REPLACES the object server-side, so a fragment would
    /// silently drop the sibling keys.
    pub async fn update_organization(
        &self,
        id: u64,
        body: &serde_json::Value,
        progress: ProgressHandle,
    ) -> Result<Organization> {
        self.patch_json(&format!("/organizations/{id}"), body, progress).await
    }

    pub async fn update_engine(&self, id: u64, engine: &Engine, progress: ProgressHandle) -> Result<Engine> {
        self.patch_json(&format!("/engines/{id}"), engine, progress).await
    }

    pub async fn update_engine_field(&self, id: u64, field: &EngineField, progress: ProgressHandle) -> Result<EngineField> {
        self.patch_json(&format!("/engine_fields/{id}"), field, progress).await
    }

    /// Partial-body variant: PATCH with a hand-built `serde_json::Value`.
    /// Useful when the caller needs to omit immutable fields like `name`
    /// (Rossum rejects renaming an existing engine field with 400). Mirror
    /// of [`update_hook_value`].
    pub async fn update_engine_field_value(&self, id: u64, body: &serde_json::Value, progress: ProgressHandle) -> Result<EngineField> {
        self.patch_json(&format!("/engine_fields/{id}"), body, progress).await
    }

    // --- delete endpoints (DELETE) ------------------------------------
    //
    // Reached only through `rdc sync`'s delete phase
    // (`cli::push::deletes`), which commits lockfile tombstones — a local
    // deletion, or the tgt-only objects `rdc migrate --mirror` pruned from
    // the target snapshot. Always gated: an interactive `[y/N]` on a TTY,
    // and `--allow-deletes` otherwise. Nothing else in rdc issues a DELETE.

    /// Generic DELETE `<base>/<path>`. Accepts 204 (deleted) and 404
    /// (already gone) as success; surfaces every other non-2xx.
    pub async fn delete_path(&self, path: &str, progress: ProgressHandle) -> Result<()> {
        let url = format!("{}{}", self.base_url, path);
        let resp = retry::send_with_retry(
            || self.http
                .delete(&url)
                .header("Authorization", format!("token {}", self.token)),
            &format!("DELETE {url}"),
            progress,
            self.limiter.as_ref(),
        ).await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into())
    }

    pub async fn delete_hook(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/hooks/{id}"), progress).await
    }
    pub async fn delete_workspace(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/workspaces/{id}"), progress).await
    }
    pub async fn delete_queue(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/queues/{id}"), progress).await
    }
    pub async fn delete_schema(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/schemas/{id}"), progress).await
    }
    pub async fn delete_inbox(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/inboxes/{id}"), progress).await
    }
    pub async fn delete_email_template(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/email_templates/{id}"), progress).await
    }
    pub async fn delete_rule(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/rules/{id}"), progress).await
    }
    pub async fn delete_label(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/labels/{id}"), progress).await
    }
    pub async fn delete_engine(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/engines/{id}"), progress).await
    }
    pub async fn delete_engine_field(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/engine_fields/{id}"), progress).await
    }

    // --- private helpers ----------------------------------------------

    /// Fetch every page of `<base>/<path>` and concatenate `results`.
    /// Used by every `list_*` method.
    async fn list_paginated<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        progress: ProgressHandle,
    ) -> Result<Vec<T>> {
        use futures::stream::{StreamExt, TryStreamExt};
        use serde_json::Value;

        let base = format!("{}{}", self.base_url, path);
        let page_url = |n: u64| format!("{base}?page_size={LIST_PAGE_SIZE}&ordering=id&page={n}");

        // Page 1 — learn total_pages. Collect as Value so we can dedupe by `url`
        // before deserializing into the typed model.
        let first: Page<Value> = self.get_json(&page_url(1), progress.clone()).await?;
        let total_pages = first.pagination.total_pages;
        let mut raw: Vec<Value> = first.results;
        if let Some(p) = &progress { p.bump(raw.len() as u64); }

        if total_pages > 1 {
            // Offset fan-out for pages 2..=total_pages (parallel, paced by the limiter).
            // NOTE: these page futures borrow `self` and are not `'static`; they must be
            // driven by `buffer_unordered` on the current task, never `tokio::spawn`ed.
            let mut rest: Vec<(u64, Vec<Value>)> = futures::stream::iter(2..=total_pages)
                .map(|n| {
                    let url = page_url(n);
                    let progress = progress.clone();
                    async move {
                        let pg: Page<Value> = self.get_json(&url, progress.clone()).await?;
                        if let Some(p) = &progress { p.bump(pg.results.len() as u64); }
                        anyhow::Ok((n, pg.results))
                    }
                })
                .buffer_unordered(LIST_PAGE_FANOUT)
                .try_collect()
                .await?;
            // `buffer_unordered` yields pages in COMPLETION order; restore the
            // API's page order (each page is itself `ordering=id`) so the merged
            // list is a deterministic, id-ordered sequence. Without this, a
            // same-named object's name-derived slug `-2` suffix would depend on
            // response timing on a first pull.
            rest.sort_by_key(|(n, _)| *n);
            for (_, page) in rest {
                raw.extend(page);
            }
        } else if total_pages == 0 {
            // Fallback: endpoint did not report total_pages — follow the `next`
            // cursor sequentially (legacy behavior; guards a non-conforming kind).
            let mut next = first.pagination.next.clone();
            while let Some(u) = next {
                let pg: Page<Value> = self.get_json(&u, progress.clone()).await?;
                if let Some(p) = &progress { p.bump(pg.results.len() as u64); }
                next = pg.pagination.next.clone();
                raw.extend(pg.results);
            }
        }

        // Dedupe by `url` (defensive vs offset paging under mid-pull mutation),
        // then deserialize to T.
        let mut seen = std::collections::HashSet::with_capacity(raw.len());
        let mut out = Vec::with_capacity(raw.len());
        for v in raw {
            let key = v
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string());
            if seen.insert(key) {
                out.push(
                    serde_json::from_value::<T>(v)
                        .with_context(|| format!("decoding list item from {base}"))?,
                );
            }
        }
        Ok(out)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, progress: ProgressHandle) -> Result<T> {
        let resp = retry::send_with_retry(
            || self.http.get(url).header("Authorization", format!("token {}", self.token)),
            &format!("GET {url}"),
            progress,
            self.limiter.as_ref(),
        ).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into());
        }
        resp.json::<T>().await
            .with_context(|| format!("decoding response from {url}"))
    }

    /// Public escape hatch for cross-env apply, which builds a stripped
    /// JSON body (no id/url/organization, no server-computed sub-collections
    /// like `queue.hooks`) and sends it via PATCH. The body has already been
    /// shaped by the caller so we don't go through a typed struct.
    pub async fn patch_value(&self, path: &str, body: &serde_json::Value, progress: ProgressHandle) -> Result<serde_json::Value> {
        self.patch_json(path, body, progress).await
    }

    /// Generic PATCH `<base>/<path>` with `body` as JSON. Used by every
    /// `update_*` method. Centralises 429 retry/backoff via `retry::send_with_retry`.
    async fn patch_json<TBody, TResp>(&self, path: &str, body: &TBody, progress: ProgressHandle) -> Result<TResp>
    where
        TBody: serde::Serialize,
        TResp: serde::de::DeserializeOwned,
    {
        // Serialise once so we can (a) drop the server-assigned self-identity
        // (`id`/`url`) — which a PATCH ignores anyway, and which may carry a
        // stale `rdc://` self-reference that `migrate` left behind (see
        // `strip_self_identity`) — and (b) guard the result against any
        // unresolved `rdc://` cross-reference before it reaches the wire. The
        // stripped value is what we send, so a stale self-url can neither trip
        // the guard nor 400 as an invalid hyperlink.
        let mut body_value =
            serde_json::to_value(body).context("serializing PATCH body for portable-ref check")?;
        strip_self_identity(&mut body_value);
        ensure_no_residual_refs(path, &body_value)?;
        let url = format!("{}{}", self.base_url, path);
        let resp = retry::send_with_retry(
            || self.http
                .patch(&url)
                .header("Authorization", format!("token {}", self.token))
                .json(&body_value),
            &format!("PATCH {url}"),
            progress,
            self.limiter.as_ref(),
        ).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into());
        }
        resp.json::<TResp>().await
            .with_context(|| format!("decoding PATCH response from {url}"))
    }

    /// Generic POST `<base>/<path>` with `body` as JSON. Used by every
    /// `create_*` method. Body is pre-stripped of server-managed fields
    /// by the caller (`strip_for_create` in `src/snapshot/create.rs`).
    async fn post_json<TResp>(&self, path: &str, body: &serde_json::Value, progress: ProgressHandle) -> Result<TResp>
    where
        TResp: serde::de::DeserializeOwned,
    {
        // Guard before any network call: never ship an unresolved `rdc://`
        // portable ref (it 400s opaquely as "Invalid hyperlink - No URL match").
        ensure_no_residual_refs(path, body)?;
        let url = format!("{}{}", self.base_url, path);
        let resp = retry::send_with_retry(
            || self.http
                .post(&url)
                .header("Authorization", format!("token {}", self.token))
                .json(body),
            &format!("POST {url}"),
            progress,
            self.limiter.as_ref(),
        ).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into());
        }
        resp.json::<TResp>().await
            .with_context(|| format!("decoding POST response from {url}"))
    }
}

/// Exchange username/password for an API token via
/// `POST /v1/auth/login`. Returns the issued `key`.
///
/// This is a free function rather than a method on [`RossumClient`]
/// because login doesn't take a token (it produces one). Used by
/// `secrets::resolve_token` to obtain a fresh token when the cache is
/// missing/expired and `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are set,
/// and by `cli::auth::run` to handle `rdc auth <env> --username <u>`.
///
/// Retries on transient 429/502/503/504 via [`retry::send_with_retry`].
/// 401 is **not** retried (a bad password isn't going to fix itself).
pub async fn login(api_base: &str, username: &str, password: &str) -> Result<String> {
    let http = build_http_client()?;
    let url = format!("{api_base}/auth/login");
    let body = serde_json::json!({
        "username": username,
        "password": password,
    });
    let progress: ProgressHandle = None;
    let resp = retry::send_with_retry(
        || http.post(&url).json(&body),
        &format!("POST {url}"),
        progress.clone(),
        None, // no rate limiter — login is rare
    )
    .await?;
    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return Err(ApiError::Status {
            status: status.as_u16(),
            body: body_text,
            env: None,
        }
        .into());
    }
    #[derive(Deserialize)]
    struct LoginResponse {
        key: String,
    }
    let parsed: LoginResponse = resp
        .json()
        .await
        .with_context(|| format!("decoding login response from {url}"))?;
    Ok(parsed.key)
}

/// Fail-loud guard run on every outbound POST/PATCH body. A body that still
/// contains an `rdc://<kind>/<slug>` portable reference is one whose push-side
/// resolution ([`crate::snapshot::refs::resolve_value`]) could not rewrite the
/// ref to an env URL — the target object's slug isn't in the lockfile yet.
/// Sent verbatim, the Rossum API parses `rdc://…` as a URL whose path matches
/// no object and returns the opaque `400 {"engine":["Invalid hyperlink - No URL
/// match."]}`. Refusing here turns that into a precise, actionable error that
/// names the request and the offending ref(s) instead of leaking a dangling
/// reference onto the wire.
/// Strip the server-assigned self-identity fields (`id`, `url`) from an
/// outgoing object body. Rossum assigns both; a PATCH addresses its object by
/// the URL path (`/email_templates/123`) and ignores them in the payload, and
/// create bodies drop them via [`crate::snapshot::create::strip_for_create`].
///
/// The reason this matters for correctness — not just tidiness: `migrate`
/// rewrites an object's *cross-references* (`queue`, `workspace`, …) when it
/// remaps slugs across orgs, but it does not rewrite an object's *own* `url`
/// self-reference for kinds outside its substitution set (email_templates,
/// engine_fields). After a queue-slug remap, such an object's typed `url` still
/// embeds the pre-migrate slug as `rdc://<kind>/…/<old-slug>/…`. That ref
/// resolves to nothing in the target env, so it survives push-side resolution
/// and reaches the wire — where [`ensure_no_residual_refs`] refuses it (and, if
/// it slipped through, Rossum would 400 it as an invalid hyperlink). Removing
/// self-identity here makes every PATCH robust against a stale self-url
/// regardless of how the on-disk snapshot was produced, while leaving genuine
/// cross-references intact for the guard to validate.
fn strip_self_identity(body: &mut serde_json::Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("id");
        obj.remove("url");
    }
}

fn ensure_no_residual_refs(path: &str, body: &serde_json::Value) -> Result<()> {
    let refs = crate::snapshot::refs::residual_rdc_refs(body);
    if refs.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to send {path}: body still contains unresolved portable reference(s) [{}]. \
         The referenced object(s) are not present in this environment yet (their slugs are \
         absent from the lockfile), so the Rossum API would reject the link with \
         \"Invalid hyperlink - No URL match\". This is the engine\u{2194}queue create-time cycle: \
         rdc pushes queues before engines, so a queue pointing at a not-yet-created engine \
         cannot resolve. Create or sync the referenced object first (or remove the reference), \
         then re-run.",
        refs.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ensure_no_residual_refs_errors_naming_every_ref_and_the_path() {
        let body = json!({
            "name": "1. Intake & Triage",
            "engine": "rdc://engines/1-intake-triage",
            "schema": "https://x.rossum.app/api/v1/schemas/5",
        });
        let err = ensure_no_residual_refs("/queues/5550440", &body).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rdc://engines/1-intake-triage"),
            "error must name the unresolved ref: {msg}"
        );
        assert!(
            msg.contains("/queues/5550440"),
            "error must name the request path: {msg}"
        );
    }

    #[test]
    fn ensure_no_residual_refs_ok_for_fully_resolved_body() {
        let body = json!({
            "name": "Q",
            "engine": "https://x.rossum.app/api/v1/engines/383",
        });
        assert!(ensure_no_residual_refs("/queues/1", &body).is_ok());
    }

    #[test]
    fn strip_self_identity_removes_top_level_id_and_url() {
        let mut body = json!({
            "id": 14081767,
            "url": "https://x.rossum.app/api/v1/email_templates/14081767",
            "name": "X",
            "queue": "https://x.rossum.app/api/v1/queues/5",
        });
        strip_self_identity(&mut body);
        assert!(body.get("id").is_none(), "id must be stripped");
        assert!(body.get("url").is_none(), "url must be stripped");
        assert_eq!(body.get("name").and_then(|v| v.as_str()), Some("X"));
        assert!(body.get("queue").is_some(), "cross-ref fields survive");
    }

    /// The regression that motivated this: `migrate` rewrites cross-references
    /// (`queue`, `workspace`) but not an object's OWN `url` self-reference, so
    /// an email_template's typed `url` can still embed a pre-migrate queue slug
    /// as `rdc://…`. On PATCH that stale self-url tripped the residual guard.
    /// Stripping self-identity first makes the guard pass — the self-url is
    /// server-assigned and never belongs on the wire.
    #[test]
    fn stale_self_url_is_stripped_so_patch_guard_passes() {
        let mut body = json!({
            "id": 14081767,
            "url": "rdc://email_templates/ws-a/old-queue-slug/status-change-confirmed",
            "name": "Status change",
            "queue": "https://x.rossum.app/api/v1/queues/5",
        });
        strip_self_identity(&mut body);
        assert!(
            ensure_no_residual_refs("/email_templates/14081767", &body).is_ok(),
            "a stale self-url must not block the PATCH once stripped"
        );
    }

    /// Stripping self-identity must NOT mask a genuine dangling cross-reference:
    /// a `queue`/`engine` still holding an unresolved `rdc://` ref must keep
    /// tripping the guard.
    #[test]
    fn strip_self_identity_does_not_mask_a_real_cross_ref() {
        let mut body = json!({
            "id": 5,
            "url": "https://x.rossum.app/api/v1/queues/5",
            "engine": "rdc://engines/not-created-yet",
        });
        strip_self_identity(&mut body);
        assert!(
            ensure_no_residual_refs("/queues/5", &body).is_err(),
            "a real dangling cross-ref must still be refused"
        );
    }

    /// Integration: the POST choke point must refuse a body whose `engine`
    /// is still an unresolved `rdc://` ref — BEFORE any network call (the
    /// base URL is non-routable; the guard fires first, so this is fast).
    #[tokio::test]
    async fn create_queue_refuses_unresolved_engine_ref() {
        let client =
            RossumClient::new("https://example.invalid/api/v1".to_string(), "t".to_string())
                .unwrap();
        let body = json!({ "name": "Q", "engine": "rdc://engines/missing-engine" });
        let err = client.create_queue(&body, None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rdc://engines/missing-engine"),
            "POST guard must name the ref: {msg}"
        );
    }

    #[tokio::test]
    async fn patch_value_refuses_unresolved_ref_before_network() {
        let client = RossumClient::new("https://example.invalid/api/v1".to_string(), "t".to_string()).unwrap();
        let body = json!({ "engine": "rdc://engines/missing" });
        let err = client.patch_value("/queues/1", &body, None).await.unwrap_err();
        assert!(format!("{err:#}").contains("rdc://engines/missing"), "got: {err:#}");
    }

    /// Integration: the PATCH choke point serialises the typed `Queue` (whose
    /// `engine` lives in the flattened `extra`) and must catch the residual
    /// ref there — this is the exact path that shipped the bad value in the
    /// reported `PATCH /queues/5550440` failure.
    #[tokio::test]
    async fn update_queue_refuses_unresolved_engine_ref_in_extra() {
        let client =
            RossumClient::new("https://example.invalid/api/v1".to_string(), "t".to_string())
                .unwrap();
        let queue: Queue = serde_json::from_value(json!({
            "id": 5550440,
            "url": "",
            "name": "1. Intake & Triage",
            "workspace": null,
            "schema": null,
            "engine": "rdc://engines/1-intake-triage"
        }))
        .unwrap();
        let err = client.update_queue(5550440, &queue, None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rdc://engines/1-intake-triage"),
            "PATCH guard must name the ref from extra: {msg}"
        );
    }

    /// The pacing bypass must key on "this is a local mock" and nothing
    /// else. A real Rossum host — or a base URL we cannot parse — has to
    /// stay paced: guessing wrong in that direction means hammering a
    /// customer's org until the server starts 429ing, so the predicate
    /// fails safe.
    #[test]
    fn only_loopback_bases_skip_the_rate_limiter() {
        for paced in [
            "https://api.elis.rossum.ai/v1",
            "https://acme.rossum.app/api/v1",
            "http://rossum.internal:8080/v1",
            "https://127.0.0.1.evil.example/v1",
            "not a url",
            "",
        ] {
            assert!(!is_loopback_base(paced), "{paced:?} must stay paced");
        }
        for mock in [
            "http://127.0.0.1:61916",
            "http://127.7.7.7/v1",
            "http://localhost:8080/api/v1",
            "http://[::1]:9000/v1",
        ] {
            assert!(is_loopback_base(mock), "{mock:?} is a local mock");
        }
    }

    /// Guards the wiring, not just the predicate: a client aimed at a real
    /// cluster keeps its bucket, one aimed at a mock has none.
    #[test]
    fn client_pacing_follows_the_base_url() {
        let real = RossumClient::new("https://api.elis.rossum.ai/v1".into(), "t".into()).unwrap();
        assert!(real.limiter.is_some(), "a real cluster must be paced");
        let mock = RossumClient::new("http://127.0.0.1:1234".into(), "t".into()).unwrap();
        assert!(mock.limiter.is_none(), "a loopback mock must not be paced");
    }
}
