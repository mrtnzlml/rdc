import { invoke } from "@tauri-apps/api/core";
import { listen, Event } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type {
  AddConnectionInput,
  ConnectionSummary,
  EditCredentialsInput,
  SyncProgressEvent,
} from "./types";

export const api = {
  listConnections: () => invoke<ConnectionSummary[]>("list_connections"),
  addConnection: (input: AddConnectionInput) =>
    invoke<ConnectionSummary>("add_connection", { input }),
  openExistingProject: (path: string) =>
    invoke<ConnectionSummary>("open_existing_project", { path }),
  syncConnection: (connectionId: string) =>
    invoke<void>("sync_connection", { connectionId }),
  editCredentials: (input: EditCredentialsInput) =>
    invoke<void>("edit_credentials", { input }),
  removeConnection: (connectionId: string) =>
    invoke<void>("remove_connection", { connectionId }),
  revealFolder: (path: string) => invoke<void>("reveal_folder", { path }),
};

/// Show a folder-picker dialog. Returns the absolute path the user
/// chose, or null if they cancelled.
export async function pickFolder(title: string): Promise<string | null> {
  const result = await openDialog({
    directory: true,
    multiple: false,
    title,
  });
  // Tauri's open returns null | string | string[]; with multiple=false
  // and directory=true it's null | string.
  return typeof result === "string" ? result : null;
}

export function listenSyncProgress(
  handler: (e: SyncProgressEvent) => void,
): Promise<() => void> {
  return listen<SyncProgressEvent>("sync-progress", (event: Event<SyncProgressEvent>) =>
    handler(event.payload),
  );
}
