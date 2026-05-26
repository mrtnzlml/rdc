export type AuthKind = "token" | "password";

export interface ConnectionSummary {
  id: string;
  name: string;
  api_base: string;
  org_id: number;
  folder: string;
  auth_kind: AuthKind;
  last_sync_unix: number | null;
  last_status: "never" | "ok" | "error";
  last_status_message: string | null;
  file_count: number;
}

export interface SyncProgressEvent {
  connection_id: string;
  phase: "started" | "done" | "error";
  message: string | null;
  file_count: number | null;
}

export type SyncState = "idle" | "running" | "error";

export interface AddConnectionInput {
  name: string;
  api_base: string;
  org_id: number;
  auth_kind: AuthKind;
  token: string | null;
  username: string | null;
  password: string | null;
}

export interface EditCredentialsInput {
  connection_id: string;
  auth_kind: AuthKind;
  token: string | null;
  username: string | null;
  password: string | null;
}
