import { useState } from "react";
import { FolderOpen, KeyRound, RefreshCw, Trash2 } from "lucide-react";
import type { ConnectionSummary } from "../types";
import Button from "./Button";
import ContextMenu from "./ContextMenu";
import { startWindowDrag, toggleWindowMaximize } from "../window";

type Props = {
  connections: ConnectionSummary[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onAdd: () => void;
  onOpenExisting: () => void;
  onSyncConnection: (id: string) => void;
  onRevealConnection: (folder: string) => void;
  onEditConnection: (c: ConnectionSummary) => void;
  onRemoveConnection: (c: ConnectionSummary) => void;
};

type ContextState = { connection: ConnectionSummary; x: number; y: number };

export default function Sidebar({
  connections,
  selectedId,
  onSelect,
  onAdd,
  onOpenExisting,
  onSyncConnection,
  onRevealConnection,
  onEditConnection,
  onRemoveConnection,
}: Props) {
  const [ctx, setCtx] = useState<ContextState | null>(null);

  return (
    <aside className="flex flex-col border-r border-border-subtle/50 bg-bg-sidebar/55">
      <div
        onMouseDown={startWindowDrag}
        onDoubleClick={toggleWindowMaximize}
        className="select-none px-4 pb-1 pt-12 text-[11px] font-semibold uppercase tracking-wider text-fg-muted"
      >
        Connections
      </div>
      <div className="flex-1 overflow-y-auto px-2 py-1">
        {connections.map((c) => {
          const selected = c.id === selectedId;
          return (
            <div
              key={c.id}
              className={`mb-0.5 cursor-pointer rounded-lg px-3 py-1.5 text-[13px] transition-colors ${
                selected
                  ? "bg-row-selected font-medium text-fg"
                  : "hover:bg-row-hover"
              }`}
              onClick={() => onSelect(c.id)}
              onContextMenu={(e) => {
                e.preventDefault();
                onSelect(c.id);
                setCtx({ connection: c, x: e.clientX, y: e.clientY });
              }}
            >
              {c.name}
            </div>
          );
        })}
      </div>
      <div className="flex flex-col gap-1.5 border-t border-border-subtle/50 px-3 py-3">
        <Button onClick={onAdd} className="w-full">
          + Add Connection
        </Button>
        <Button variant="link" onClick={onOpenExisting} className="self-center">
          Open existing…
        </Button>
      </div>

      {ctx && (
        <ContextMenu
          x={ctx.x}
          y={ctx.y}
          onClose={() => setCtx(null)}
          items={[
            {
              label: "Sync now",
              icon: <RefreshCw size={14} strokeWidth={2} />,
              onClick: () => onSyncConnection(ctx.connection.id),
            },
            {
              label: "Reveal in Finder",
              icon: <FolderOpen size={14} strokeWidth={2} />,
              onClick: () => onRevealConnection(ctx.connection.folder),
            },
            { separator: true },
            {
              label: "Edit credentials…",
              icon: <KeyRound size={14} strokeWidth={2} />,
              onClick: () => onEditConnection(ctx.connection),
            },
            {
              label: "Remove…",
              icon: <Trash2 size={14} strokeWidth={2} />,
              onClick: () => onRemoveConnection(ctx.connection),
              destructive: true,
            },
          ]}
        />
      )}
    </aside>
  );
}
