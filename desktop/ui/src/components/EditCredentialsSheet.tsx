import { useState } from "react";
import { api } from "../api";
import type { AuthKind, ConnectionSummary } from "../types";

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
    <div className="modal-backdrop">
      <div className="modal">
        <h3>Edit credentials for {c.name}</h3>
        {error && <div className="banner banner-error">{error}</div>}
        <div className="field">
          <label>Sign in with</label>
          <select
            value={authKind}
            onChange={(e) => setAuthKind(e.target.value as AuthKind)}
          >
            <option value="password">Email + password</option>
            <option value="token">API token</option>
          </select>
        </div>
        {authKind === "token" ? (
          <div className="field">
            <label>New token</label>
            <input
              type="password"
              placeholder="Enter new token"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              autoFocus
            />
          </div>
        ) : (
          <>
            <div className="field">
              <label>Email</label>
              <input
                type="email"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
              />
            </div>
            <div className="field">
              <label>New password</label>
              <input
                type="password"
                placeholder="Enter new password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>
          </>
        )}
        <div className="modal-actions">
          <button className="btn" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button className="btn btn-primary" onClick={submit} disabled={busy}>
            {busy ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}
