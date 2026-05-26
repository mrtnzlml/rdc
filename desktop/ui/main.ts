declare const __TAURI__: {
  core: { invoke: <T>(cmd: string, args?: unknown) => Promise<T> };
  event: {
    listen: <T>(event: string, handler: (event: { payload: T }) => void) => Promise<() => void>;
  };
};
const invoke = __TAURI__.core.invoke;
const listen = __TAURI__.event.listen;

// Reveal in Finder: call our own Rust command, which shells out to
// `open <path>`. Avoids the plugin-globals trap (Tauri 2 doesn't auto-
// expose `__TAURI__.shell.open` even with `withGlobalTauri: true`).
async function revealInFinder(path: string): Promise<void> {
  await invoke("reveal_folder", { path });
}

// Copy to clipboard: WebView's built-in API. No Tauri plugin needed.
async function copyToClipboard(text: string): Promise<void> {
  await navigator.clipboard.writeText(text);
}

interface ConnectionSummary {
  id: string;
  name: string;
  api_base: string;
  org_id: number;
  folder: string;
  auth_kind: "token" | "password";
  last_sync_unix: number | null;
  last_status: "never" | "ok" | "error";
  last_status_message: string | null;
  file_count: number;
}

interface SyncProgressEvent {
  connection_id: string;
  phase: "started" | "done" | "error";
  message: string | null;
  file_count: number | null;
}

const syncState = new Map<string, "idle" | "running" | "error">();

async function setupEventListener() {
  await listen<SyncProgressEvent>("sync-progress", (e) => {
    const p = e.payload;
    if (p.phase === "started") {
      syncState.set(p.connection_id, "running");
    } else if (p.phase === "done") {
      syncState.set(p.connection_id, "idle");
      // Refresh from backend so last_sync_unix/file_count update.
      void load();
      return;
    } else if (p.phase === "error") {
      syncState.set(p.connection_id, "error");
      const c = connections.find((c) => c.id === p.connection_id);
      if (c) {
        c.last_status = "error";
        c.last_status_message = p.message ?? "Sync failed";
      }
    }
    if (selectedId === p.connection_id) renderDetail();
  });
}

let listenerAttached = false;

let connections: ConnectionSummary[] = [];
let selectedId: string | null = null;

async function load() {
  connections = await invoke<ConnectionSummary[]>("list_connections");
  if (connections.length > 0 && !selectedId) {
    selectedId = connections[0].id;
  }
  if (!listenerAttached) {
    await setupEventListener();
    listenerAttached = true;
  }
  render();
}

function render() {
  const root = document.getElementById("root")!;
  if (connections.length === 0) {
    root.innerHTML = renderEmpty();
    document.getElementById("add-btn")!.onclick = () => openAddSheet();
    return;
  }
  root.innerHTML = `
    <div class="app">
      <aside class="sidebar">
        <div class="sidebar-header">Connections</div>
        <div class="sidebar-list" id="sidebar-list"></div>
        <div class="sidebar-add"><button class="btn" id="add-btn">+ Add Connection</button></div>
      </aside>
      <main class="detail" id="detail"></main>
    </div>
  `;
  renderSidebar();
  renderDetail();
  document.getElementById("add-btn")!.onclick = () => openAddSheet();
}

function renderEmpty(): string {
  return `
    <div class="empty">
      <h1>Sync a Rossum organization</h1>
      <p>Pull your Rossum config locally so Claude can read it.</p>
      <button class="btn btn-primary" id="add-btn">Add Connection</button>
    </div>
  `;
}

function renderSidebar() {
  const list = document.getElementById("sidebar-list")!;
  list.innerHTML = connections
    .map(
      (c) => `
      <div class="sidebar-row ${c.id === selectedId ? "selected" : ""}" data-id="${c.id}">
        ${escapeHtml(c.name)}
      </div>`,
    )
    .join("");
  list.querySelectorAll(".sidebar-row").forEach((row) => {
    row.addEventListener("click", () => {
      selectedId = (row as HTMLElement).dataset.id!;
      render();
    });
  });
}

function renderDetail() {
  const detail = document.getElementById("detail")!;
  const c = connections.find((c) => c.id === selectedId);
  if (!c) {
    detail.innerHTML = "";
    return;
  }
  const state = syncState.get(c.id) ?? "idle";
  detail.innerHTML = `
    <h2>${escapeHtml(c.name)}</h2>
    <div class="subtitle">${escapeHtml(c.api_base)}</div>
    <div class="row"><span class="label">Last synced</span><span>${formatLastSync(c.last_sync_unix)}</span></div>
    <div class="row"><span class="label">Status</span><span class="${statusClass(c.last_status)}">${formatStatus(c)}</span></div>
    ${state === "running"
      ? `<div class="progress"><div class="progress-bar"></div></div>`
      : `<div class="row"><button class="btn btn-primary" id="sync-btn">Sync now</button></div>`}
    ${state === "error" && c.last_status_message
      ? `<div class="banner banner-error">${escapeHtml(c.last_status_message)}</div>`
      : ""}
    <div class="row"><span class="label">Folder</span><span>${escapeHtml(c.folder)}</span></div>
    <div class="row">
      <button class="btn" id="reveal-btn">Reveal in Finder</button>
      <button class="btn" id="copy-btn">Copy path</button>
    </div>
    <div class="row" style="margin-top:32px;">
      <button class="btn-link" id="edit-creds-btn">Edit credentials</button>
      <span style="flex:1"></span>
      <button class="btn btn-destructive" id="remove-btn">Remove…</button>
    </div>
  `;
  const syncBtn = document.getElementById("sync-btn");
  if (syncBtn) {
    syncBtn.onclick = async () => {
      try {
        await invoke("sync_connection", { connectionId: c.id });
      } catch (e) {
        c.last_status = "error";
        c.last_status_message = String(e);
        renderDetail();
      }
    };
  }
  const revealBtn = document.getElementById("reveal-btn");
  if (revealBtn) {
    revealBtn.onclick = async () => {
      try {
        await revealInFinder(c.folder);
      } catch (e) {
        console.error("reveal failed", e);
      }
    };
  }
  const copyBtn = document.getElementById("copy-btn");
  if (copyBtn) {
    copyBtn.onclick = async () => {
      try {
        await copyToClipboard(c.folder);
        copyBtn.textContent = "Copied!";
        setTimeout(() => { copyBtn.textContent = "Copy path"; }, 1200);
      } catch (e) {
        console.error("copy failed", e);
      }
    };
  }
  const editBtn = document.getElementById("edit-creds-btn");
  if (editBtn) editBtn.onclick = () => openEditCredentialsSheet(c);
  const removeBtn = document.getElementById("remove-btn");
  if (removeBtn) removeBtn.onclick = () => openRemoveSheet(c);
}

function formatLastSync(unix: number | null): string {
  if (unix === null) return "Never";
  const now = Math.floor(Date.now() / 1000);
  const delta = now - unix;
  if (delta < 60) return "just now";
  if (delta < 3600) return `${Math.floor(delta / 60)} min ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} hr ago`;
  return `${Math.floor(delta / 86400)} day ago`;
}

function statusClass(s: string): string {
  if (s === "ok") return "status-ok";
  if (s === "error") return "status-error";
  return "";
}

function formatStatus(c: ConnectionSummary): string {
  if (c.last_status === "never") return "Not synced yet";
  if (c.last_status === "ok") return `Up to date · ${c.file_count} files`;
  return `Error: ${escapeHtml(c.last_status_message || "unknown")}`;
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function openAddSheet() {
  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal" role="dialog" aria-label="Add Connection">
      <h3>Add Connection</h3>
      <div id="add-error"></div>
      <div class="field"><label>Name</label><input id="add-name" type="text" placeholder="Acme Corp — Production" /></div>
      <div class="field"><label>API URL</label><input id="add-api" type="url" placeholder="https://acme.app.rossum.ai/api/v1" /></div>
      <div class="field"><label>Org ID</label><input id="add-org" type="number" min="1" /></div>
      <div class="field">
        <label>Sign in with</label>
        <select id="add-auth">
          <option value="password">Email + password</option>
          <option value="token">API token</option>
        </select>
      </div>
      <div id="auth-fields"></div>
      <div class="modal-actions">
        <button class="btn" id="add-cancel">Cancel</button>
        <button class="btn btn-primary" id="add-submit">Add &amp; Sync</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);

  const authSel = document.getElementById("add-auth") as HTMLSelectElement;
  const renderAuthFields = () => {
    const c = document.getElementById("auth-fields")!;
    if (authSel.value === "token") {
      c.innerHTML = `<div class="field"><label>Token</label><input id="add-token" type="password" /></div>`;
    } else {
      c.innerHTML = `
        <div class="field"><label>Email</label><input id="add-username" type="email" /></div>
        <div class="field"><label>Password</label><input id="add-password" type="password" /></div>
      `;
    }
  };
  authSel.onchange = renderAuthFields;
  renderAuthFields();

  document.getElementById("add-cancel")!.onclick = () => overlay.remove();
  document.getElementById("add-submit")!.onclick = async () => {
    const errBox = document.getElementById("add-error")!;
    errBox.innerHTML = "";
    const input = {
      name: (document.getElementById("add-name") as HTMLInputElement).value.trim(),
      api_base: (document.getElementById("add-api") as HTMLInputElement).value.trim(),
      org_id: Number((document.getElementById("add-org") as HTMLInputElement).value),
      auth_kind: authSel.value,
      token: authSel.value === "token" ? (document.getElementById("add-token") as HTMLInputElement).value : null,
      username: authSel.value === "password" ? (document.getElementById("add-username") as HTMLInputElement).value : null,
      password: authSel.value === "password" ? (document.getElementById("add-password") as HTMLInputElement).value : null,
      folder: null,
    };
    if (!input.name || !input.api_base || !input.org_id) {
      errBox.innerHTML = `<div class="banner banner-error">Name, API URL, and Org ID are required.</div>`;
      return;
    }
    try {
      const created = await invoke<ConnectionSummary>("add_connection", { input });
      connections.push(created);
      selectedId = created.id;
      overlay.remove();
      render();
      // Trigger first sync immediately.
      await invoke("sync_connection", { connectionId: created.id });
    } catch (e) {
      errBox.innerHTML = `<div class="banner banner-error">${escapeHtml(String(e))}</div>`;
    }
  };
}

function openEditCredentialsSheet(c: ConnectionSummary) {
  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal">
      <h3>Edit credentials for ${escapeHtml(c.name)}</h3>
      <div id="edit-error"></div>
      <div class="field">
        <label>Sign in with</label>
        <select id="edit-auth">
          <option value="password" ${c.auth_kind === "password" ? "selected" : ""}>Email + password</option>
          <option value="token" ${c.auth_kind === "token" ? "selected" : ""}>API token</option>
        </select>
      </div>
      <div id="edit-fields"></div>
      <div class="modal-actions">
        <button class="btn" id="edit-cancel">Cancel</button>
        <button class="btn btn-primary" id="edit-save">Save</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);
  const authSel = document.getElementById("edit-auth") as HTMLSelectElement;
  const renderEditFields = () => {
    const c = document.getElementById("edit-fields")!;
    if (authSel.value === "token") {
      c.innerHTML = `<div class="field"><label>New token</label><input id="edit-token" type="password" placeholder="Enter new token" /></div>`;
    } else {
      c.innerHTML = `
        <div class="field"><label>Email</label><input id="edit-username" type="email" /></div>
        <div class="field"><label>New password</label><input id="edit-password" type="password" placeholder="Enter new password" /></div>
      `;
    }
  };
  authSel.onchange = renderEditFields;
  renderEditFields();

  document.getElementById("edit-cancel")!.onclick = () => overlay.remove();
  document.getElementById("edit-save")!.onclick = async () => {
    const errBox = document.getElementById("edit-error")!;
    errBox.innerHTML = "";
    const input = {
      connection_id: c.id,
      auth_kind: authSel.value,
      token: authSel.value === "token" ? (document.getElementById("edit-token") as HTMLInputElement).value : null,
      username: authSel.value === "password" ? (document.getElementById("edit-username") as HTMLInputElement).value : null,
      password: authSel.value === "password" ? (document.getElementById("edit-password") as HTMLInputElement).value : null,
    };
    try {
      await invoke("edit_credentials", { input });
      overlay.remove();
      await load();
    } catch (e) {
      errBox.innerHTML = `<div class="banner banner-error">${escapeHtml(String(e))}</div>`;
    }
  };
}

function openRemoveSheet(c: ConnectionSummary) {
  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal">
      <h3>Remove "${escapeHtml(c.name)}"?</h3>
      <p>This will delete the local folder (<code>${escapeHtml(c.folder)}</code>) and remove the stored sign-in. Rossum data is not affected.</p>
      <div class="modal-actions">
        <button class="btn" id="remove-cancel">Cancel</button>
        <button class="btn btn-destructive" id="remove-confirm">Remove</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);
  document.getElementById("remove-cancel")!.onclick = () => overlay.remove();
  document.getElementById("remove-confirm")!.onclick = async () => {
    try {
      await invoke("remove_connection", { connectionId: c.id });
      overlay.remove();
      selectedId = null;
      await load();
    } catch (e) {
      alert(String(e));
    }
  };
}

load();
