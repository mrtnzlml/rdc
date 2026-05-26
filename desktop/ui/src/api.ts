import { invoke } from "@tauri-apps/api/core";
import { listen, Event } from "@tauri-apps/api/event";
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
  syncConnection: (connectionId: string) =>
    invoke<void>("sync_connection", { connectionId }),
  editCredentials: (input: EditCredentialsInput) =>
    invoke<void>("edit_credentials", { input }),
  removeConnection: (connectionId: string) =>
    invoke<void>("remove_connection", { connectionId }),
  revealFolder: (path: string) => invoke<void>("reveal_folder", { path }),
};

export function listenSyncProgress(
  handler: (e: SyncProgressEvent) => void,
): Promise<() => void> {
  return listen<SyncProgressEvent>("sync-progress", (event: Event<SyncProgressEvent>) =>
    handler(event.payload),
  );
}
