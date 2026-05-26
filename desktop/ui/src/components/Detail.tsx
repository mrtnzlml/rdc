import { useState } from "react";
import {
  Check,
  CircleAlert,
  Clipboard,
  FolderOpen,
  KeyRound,
  RefreshCw,
  Trash2,
} from "lucide-react";
import type { ConnectionSummary, SyncState } from "../types";
import Button from "./Button";
import IconButton from "./IconButton";

function formatLastSync(unix: number | null): string {
  if (unix === null) return "Never";
  const now = Math.floor(Date.now() / 1000);
  const delta = now - unix;
  if (delta < 60) return "just now";
  if (delta < 3600) return `${Math.floor(delta / 60)} min ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} hr ago`;
  return `${Math.floor(delta / 86400)} day ago`;
}

function StatusBadge({ c }: { c: ConnectionSummary }) {
  if (c.last_status === "never") {
    return <span className="text-fg-muted">Not synced yet</span>;
  }
  if (c.last_status === "ok") {
    return (
      <span className="inline-flex items-center gap-1.5 text-success">
        <Check size={14} strokeWidth={2.5} />
        Up to date · {c.file_count} files
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1.5 text-error">
      <CircleAlert size={14} strokeWidth={2} />
      {c.last_status_message || "Error"}
    </span>
  );
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

  const InfoRow = ({ label, value }: { label: string; value: React.ReactNode }) => (
    <div className="flex items-baseline justify-between gap-4 border-t border-border-subtle px-4 py-2.5 first:border-t-0">
      <span className="text-[13px] text-fg-muted">{label}</span>
      <span className="truncate text-[13px]">{value}</span>
    </div>
  );

  return (
    <div className="mx-auto max-w-[640px]">
      {/* Title block */}
      <div className="mb-5">
        <h2 className="m-0 mb-1 text-[22px] font-semibold tracking-tight">
          {c.name}
        </h2>
        <div className="text-[12px] text-fg-muted">{c.api_base}</div>
      </div>

      {/* Toolbar */}
      <div className="mb-5 flex items-center gap-2">
        <Button
          variant="primary"
          onClick={onSync}
          disabled={syncState === "running"}
          icon={
            <RefreshCw
              size={14}
              strokeWidth={2.5}
              className={syncState === "running" ? "animate-spin" : ""}
            />
          }
        >
          {syncState === "running" ? "Syncing…" : "Sync now"}
        </Button>
        <IconButton
          icon={<FolderOpen size={16} strokeWidth={2} />}
          onClick={onReveal}
          title="Reveal in Finder"
        />
        <IconButton
          icon={copied ? <Check size={16} strokeWidth={2.5} /> : <Clipboard size={16} strokeWidth={2} />}
          onClick={handleCopy}
          title={copied ? "Copied!" : "Copy folder path"}
        />
        <span className="flex-1" />
        <IconButton
          icon={<KeyRound size={16} strokeWidth={2} />}
          onClick={onEdit}
          title="Edit credentials"
        />
        <IconButton
          icon={<Trash2 size={16} strokeWidth={2} />}
          onClick={onRemove}
          title="Remove Connection"
          variant="destructive"
        />
      </div>

      {/* Progress (during sync) */}
      {syncState === "running" && (
        <div className="mb-5 h-1 overflow-hidden rounded-full bg-border-subtle">
          <div className="progress-bar h-full w-[30%] rounded-full bg-accent" />
        </div>
      )}

      {/* Error banner */}
      {syncState === "error" && c.last_status_message && (
        <div className="mb-5 flex items-start gap-2 rounded-xl border border-error/30 bg-error/10 px-4 py-3 text-[13px] text-error">
          <CircleAlert size={16} strokeWidth={2} className="mt-0.5 shrink-0" />
          <span>{c.last_status_message}</span>
        </div>
      )}

      {/* Info card */}
      <div className="overflow-hidden rounded-xl border border-border-subtle bg-bg-elev">
        <InfoRow label="Status" value={<StatusBadge c={c} />} />
        <InfoRow label="Last synced" value={formatLastSync(c.last_sync_unix)} />
        <InfoRow label="File count" value={`${c.file_count} files`} />
        <InfoRow label="Sign-in" value={c.auth_kind === "password" ? "Email + password" : "API token"} />
        <InfoRow label="Org ID" value={c.org_id} />
        <InfoRow
          label="Folder"
          value={
            <code
              className="cursor-default rounded bg-bg-sidebar px-1.5 py-0.5 text-[12px] text-fg-muted"
              title={c.folder}
            >
              {c.folder.replace(/^\/Users\/[^/]+/, "~")}
            </code>
          }
        />
      </div>
    </div>
  );
}
