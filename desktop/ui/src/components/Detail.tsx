import { useState } from "react";
import type { ConnectionSummary, SyncState } from "../types";
import Button from "./Button";

function formatLastSync(unix: number | null): string {
  if (unix === null) return "Never";
  const now = Math.floor(Date.now() / 1000);
  const delta = now - unix;
  if (delta < 60) return "just now";
  if (delta < 3600) return `${Math.floor(delta / 60)} min ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} hr ago`;
  return `${Math.floor(delta / 86400)} day ago`;
}

function statusColor(s: string): string {
  if (s === "ok") return "text-success";
  if (s === "error") return "text-error";
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

const row = "my-2.5 flex items-center gap-3 text-[13px]";
const label = "w-24 text-fg-muted";

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
      <h2 className="m-0 mb-1 text-[22px] font-semibold tracking-tight">{c.name}</h2>
      <div className="mb-6 text-[13px] text-fg-muted">{c.api_base}</div>
      <div className={row}>
        <span className={label}>Last synced</span>
        <span>{formatLastSync(c.last_sync_unix)}</span>
      </div>
      <div className={row}>
        <span className={label}>Status</span>
        <span className={statusColor(c.last_status)}>{formatStatus(c)}</span>
      </div>
      {syncState === "running" ? (
        <div className="my-4 h-1 overflow-hidden rounded-full bg-border-subtle">
          <div className="progress-bar h-full w-[30%] rounded-full bg-accent" />
        </div>
      ) : (
        <div className={row}>
          <Button variant="primary" onClick={onSync}>
            Sync now
          </Button>
        </div>
      )}
      {syncState === "error" && c.last_status_message && (
        <div className="mb-4 rounded-xl border border-error/30 bg-error/10 px-4 py-3 text-error">
          {c.last_status_message}
        </div>
      )}
      <div className={row}>
        <span className={label}>Folder</span>
        <span>{c.folder}</span>
      </div>
      <div className={row}>
        <Button onClick={onReveal}>Reveal in Finder</Button>
        <Button onClick={handleCopy}>{copied ? "Copied!" : "Copy path"}</Button>
      </div>
      <div className={`${row} mt-8`}>
        <Button variant="link" onClick={onEdit}>
          Edit credentials
        </Button>
        <span className="flex-1" />
        <Button variant="destructive" onClick={onRemove}>
          Remove…
        </Button>
      </div>
    </>
  );
}
