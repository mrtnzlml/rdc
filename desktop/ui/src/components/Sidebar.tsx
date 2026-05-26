import type { ConnectionSummary } from "../types";
import Button from "./Button";
import { startWindowDrag, toggleWindowMaximize } from "../window";

type Props = {
  connections: ConnectionSummary[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onAdd: () => void;
  onOpenExisting: () => void;
};

export default function Sidebar({
  connections,
  selectedId,
  onSelect,
  onAdd,
  onOpenExisting,
}: Props) {
  return (
    <aside className="flex flex-col border-r border-border-subtle/50">
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
            >
              {c.name}
            </div>
          );
        })}
      </div>
      <div className="flex flex-col gap-1.5 border-t border-border-subtle px-3 py-3">
        <Button onClick={onAdd} className="w-full">
          + Add Connection
        </Button>
        <Button variant="link" onClick={onOpenExisting} className="self-center">
          Open existing…
        </Button>
      </div>
    </aside>
  );
}
