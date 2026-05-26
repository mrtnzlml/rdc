import { useState } from "react";
import { api } from "../api";
import type { AuthKind, ConnectionSummary } from "../types";
import Button from "./Button";
import Modal from "./Modal";
import Field from "./Field";

type Props = {
  connection: ConnectionSummary;
  onCancel: () => void;
  onSaved: () => void;
};

export default function EditCredentialsSheet({ connection: c, onCancel, onSaved }: Props) {
  const [authKind, setAuthKind] = useState<AuthKind>(c.auth_kind);
  const [token, setToken] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.editCredentials({
        connection_id: c.id,
        auth_kind: authKind,
        token: authKind === "token" ? token : null,
        username: authKind === "password" ? username : null,
        password: authKind === "password" ? password : null,
      });
      onSaved();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <Modal title={`Edit credentials for ${c.name}`}>
      {error && (
        <div className="mb-4 rounded-xl border border-error/30 bg-error/10 px-4 py-3 text-error">
          {error}
        </div>
      )}
      <Field label="Sign in with">
        <select
          value={authKind}
          onChange={(e) => setAuthKind(e.target.value as AuthKind)}
        >
          <option value="password">Email + password</option>
          <option value="token">API token</option>
        </select>
      </Field>
      {authKind === "token" ? (
        <Field label="New token">
          <input
            type="password"
            placeholder="Enter new token"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoFocus
          />
        </Field>
      ) : (
        <>
          <Field label="Email">
            <input
              type="email"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
            />
          </Field>
          <Field label="New password">
            <input
              type="password"
              placeholder="Enter new password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </Field>
        </>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onCancel} disabled={busy}>
          Cancel
        </Button>
        <Button variant="primary" onClick={submit} disabled={busy}>
          {busy ? "Saving…" : "Save"}
        </Button>
      </div>
    </Modal>
  );
}
