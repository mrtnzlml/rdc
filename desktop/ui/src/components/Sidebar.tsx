import type { ConnectionSummary } from "../types";

type Props = {
  connections: ConnectionSummary[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onAdd: () => void;
};

export default function Sidebar({ connections, selectedId, onSelect, onAdd }: Props) {
  return (
    <aside className="sidebar">
      <div className="sidebar-header">Connections</div>
      <div className="sidebar-list">
        {connections.map((c) => (
          <div
            key={c.id}
            className={`sidebar-row${c.id === selectedId ? " selected" : ""}`}
            onClick={() => onSelect(c.id)}
          >
            {c.name}
          </div>
        ))}
      </div>
      <div className="sidebar-add">
        <button className="btn" onClick={onAdd}>
          + Add Connection
        </button>
      </div>
    </aside>
  );
}
