import { useCallback, useEffect, useState } from "react";
import { api, listenSyncProgress, pickFolder } from "./api";
import type { ConnectionSummary, SyncState } from "./types";
import EmptyState from "./components/EmptyState";
import Sidebar from "./components/Sidebar";
import Detail from "./components/Detail";
import AddConnectionSheet from "./components/AddConnectionSheet";
import EditCredentialsSheet from "./components/EditCredentialsSheet";
import RemoveConfirmSheet from "./components/RemoveConfirmSheet";

export default function App() {
  const [connections, setConnections] = useState<ConnectionSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [syncState, setSyncState] = useState<Map<string, SyncState>>(new Map());
  const [showAdd, setShowAdd] = useState(false);
  const [editTarget, setEditTarget] = useState<ConnectionSummary | null>(null);
  const [removeTarget, setRemoveTarget] = useState<ConnectionSummary | null>(null);

  const reload = useCallback(async (): Promise<ConnectionSummary[]> => {
    const list = await api.listConnections();
    setConnections(list);
    setSelectedId((cur) => cur ?? list[0]?.id ?? null);
    return list;
  }, []);

  const openExisting = useCallback(async () => {
    const path = await pickFolder("Open existing rdc project");
    if (!path) return;
    try {
      const created = await api.openExistingProject(path);
      await reload();
      setSelectedId(created.id);
    } catch (e) {
      alert(String(e));
    }
  }, [reload]);

  useEffect(() => {
    void reload();
    const unlistenP = listenSyncProgress((p) => {
      setSyncState((cur) => {
        const next = new Map(cur);
        if (p.phase === "started") {
          next.set(p.connection_id, "running");
        } else if (p.phase === "done") {
          next.set(p.connection_id, "idle");
          void reload();
        } else if (p.phase === "error") {
          next.set(p.connection_id, "error");
          setConnections((conns) =>
            conns.map((c) =>
              c.id === p.connection_id
                ? {
                    ...c,
                    last_status: "error" as const,
                    last_status_message: p.message ?? "Sync failed",
                  }
                : c,
            ),
          );
        }
        return next;
      });
    });
    return () => {
      void unlistenP.then((un) => un());
    };
  }, [reload]);

  const selected = connections.find((c) => c.id === selectedId) ?? null;

  return (
    <>
      {/* Drag strip across the top of the window. With titleBarStyle:
          Overlay there's no system title bar to grab, so we mark the
          ~22px under the traffic lights as `app-region: drag`. Sits
          above all other content; nothing interactive renders here
          (sidebar content starts at pt-12 = 48px, detail at py-6 = 24px). */}
      <div className="fixed inset-x-0 top-0 z-50 h-[22px] [app-region:drag]" />
      {connections.length === 0 ? (
        <EmptyState
          onAdd={() => setShowAdd(true)}
          onOpenExisting={openExisting}
        />
      ) : (
        <div className="grid h-screen grid-cols-[240px_1fr]">
          <Sidebar
            connections={connections}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onAdd={() => setShowAdd(true)}
            onOpenExisting={openExisting}
          />
          <main className="overflow-y-auto px-8 py-6">
            {selected && (
              <Detail
                connection={selected}
                syncState={syncState.get(selected.id) ?? "idle"}
                onSync={() => void api.syncConnection(selected.id)}
                onReveal={() => void api.revealFolder(selected.folder)}
                onEdit={() => setEditTarget(selected)}
                onRemove={() => setRemoveTarget(selected)}
              />
            )}
          </main>
        </div>
      )}
      {showAdd && (
        <AddConnectionSheet
          onCancel={() => setShowAdd(false)}
          onAdded={async (c) => {
            setShowAdd(false);
            await reload();
            setSelectedId(c.id);
            await api.syncConnection(c.id);
          }}
        />
      )}
      {editTarget && (
        <EditCredentialsSheet
          connection={editTarget}
          onCancel={() => setEditTarget(null)}
          onSaved={async () => {
            setEditTarget(null);
            await reload();
          }}
        />
      )}
      {removeTarget && (
        <RemoveConfirmSheet
          connection={removeTarget}
          onCancel={() => setRemoveTarget(null)}
          onRemoved={async () => {
            setRemoveTarget(null);
            setSelectedId(null);
            await reload();
          }}
        />
      )}
    </>
  );
}
