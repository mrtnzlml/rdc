import { useState } from "react";
import type { ConnectionSummary, SyncState } from "../types";

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
  return `Error: ${c.last_status_message || "unknown"}`;
}

type Props = {
  connection: ConnectionSummary;
  syncState: SyncState;
  onSync: () => void;
  onReveal: () => void;
  onEdit: () => void;
  onRemove: () => void;
};

export default function Detail({
  connection: c,
  syncState,
  onSync,
  onReveal,
  onEdit,
  onRemove,
}: Props) {
  const [copied, setCopied] = useState(false);
  const handleCopy = async () => {
    await navigator.clipboard.writeText(c.folder);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  return (
    <>
      <h2>{c.name}</h2>
      <div className="subtitle">{c.api_base}</div>
      <div className="row">
        <span className="label">Last synced</span>
        <span>{formatLastSync(c.last_sync_unix)}</span>
      </div>
      <div className="row">
        <span className="label">Status</span>
        <span className={statusClass(c.last_status)}>{formatStatus(c)}</span>
      </div>
      {syncState === "running" ? (
        <div className="progress">
          <div className="progress-bar" />
        </div>
      ) : (
        <div className="row">
          <button className="btn btn-primary" onClick={onSync}>
            Sync now
          </button>
        </div>
      )}
      {syncState === "error" && c.last_status_message && (
        <div className="banner banner-error">{c.last_status_message}</div>
      )}
      <div className="row">
        <span className="label">Folder</span>
        <span>{c.folder}</span>
      </div>
      <div className="row">
        <button className="btn" onClick={onReveal}>
          Reveal in Finder
        </button>
        <button className="btn" onClick={handleCopy}>
          {copied ? "Copied!" : "Copy path"}
        </button>
      </div>
      <div className="row" style={{ marginTop: "32px" }}>
        <button className="btn-link" onClick={onEdit}>
          Edit credentials
        </button>
        <span style={{ flex: 1 }} />
        <button className="btn btn-destructive" onClick={onRemove}>
          Remove…
        </button>
      </div>
    </>
  );
}
