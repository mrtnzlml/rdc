import { useCallback, useEffect, useState } from "react";
import { listen, Event } from "@tauri-apps/api/event";
import { api, listenSyncProgress, pickFolder } from "./api";
import { setWindowTitle, startWindowDrag, toggleWindowMaximize } from "./window";
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

  // Suppress the WebView's default right-click menu globally. Our
  // sidebar rows handle their own contextmenu event with
  // `e.preventDefault()` before bubbling, so the custom menu still
  // opens; this listener just kills the WebView default everywhere
  // else (including dev-mode "Reload / Inspect Element").
  useEffect(() => {
    // Use globalThis.Event so we don't accidentally resolve to Tauri's
    // generic `Event<T>` (imported above).
    const handler = (e: globalThis.Event) => e.preventDefault();
    document.addEventListener("contextmenu", handler);
    return () => document.removeEventListener("contextmenu", handler);
  }, []);

  // Listen for menu-bar events from the Rust side (Cmd+N / Cmd+O).
  useEffect(() => {
    const unlistenAdd = listen("menu:new-connection", () => setShowAdd(true));
    const unlistenOpen = listen<void>("menu:open-existing", (_: Event<void>) => {
      void openExisting();
    });
    return () => {
      void unlistenAdd.then((un) => un());
      void unlistenOpen.then((un) => un());
    };
  }, [openExisting]);

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

  // Update the macOS window title to show the selected Connection.
  // Visible in Cmd-Tab, the Dock context menu, and Mission Control.
  useEffect(() => {
    setWindowTitle(selected ? `Rossum Local · ${selected.name}` : "Rossum Local");
  }, [selected]);

  return (
    <>
      {/* Drag strip across the entire window header zone (48px tall).
          With titleBarStyle: Overlay there's no system title bar to
          grab, so we initiate a window drag manually from this strip's
          onMouseDown (and toggle maximize on double-click — standard
          macOS title-bar behavior). The sidebar's pt-12 and the detail
          pane's pt-12 keep interactive content out from under it. */}
      <div
        onMouseDown={startWindowDrag}
        onDoubleClick={toggleWindowMaximize}
        className="fixed inset-x-0 top-0 z-50 h-12 select-none"
      />
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
            onSyncConnection={(id) => void api.syncConnection(id)}
            onRevealConnection={(folder) => void api.revealFolder(folder)}
            onEditConnection={(c) => setEditTarget(c)}
            onRemoveConnection={(c) => setRemoveTarget(c)}
          />
          <main className="overflow-y-auto bg-bg px-8 pb-6 pt-12">
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
