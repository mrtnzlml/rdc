import { useState } from "react";
import { api } from "../api";
import type { AuthKind, ConnectionSummary } from "../types";

type Props = {
  onCancel: () => void;
  onAdded: (created: ConnectionSummary) => void;
};

export default function AddConnectionSheet({ onCancel, onAdded }: Props) {
  const [name, setName] = useState("");
  const [apiBase, setApiBase] = useState("");
  const [orgId, setOrgId] = useState("");
  const [authKind, setAuthKind] = useState<AuthKind>("password");
  const [token, setToken] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setError(null);
    if (!name.trim() || !apiBase.trim() || !orgId.trim()) {
      setError("Name, API URL, and Org ID are required.");
      return;
    }
    setBusy(true);
    try {
      const created = await api.addConnection({
        name: name.trim(),
        api_base: apiBase.trim(),
        org_id: Number(orgId),
        auth_kind: authKind,
        token: authKind === "token" ? token : null,
        username: authKind === "password" ? username : null,
        password: authKind === "password" ? password : null,
      });
      onAdded(created);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop">
      <div className="modal" role="dialog" aria-label="Add Connection">
        <h3>Add Connection</h3>
        {error && <div className="banner banner-error">{error}</div>}
        <div className="field">
          <label>Name</label>
          <input
            type="text"
            placeholder="Acme Corp — Production"
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoFocus
          />
        </div>
        <div className="field">
          <label>API URL</label>
          <input
            type="url"
            placeholder="https://acme.app.rossum.ai/api/v1"
            value={apiBase}
            onChange={(e) => setApiBase(e.target.value)}
          />
        </div>
        <div className="field">
          <label>Org ID</label>
          <input
            type="number"
            min="1"
            value={orgId}
            onChange={(e) => setOrgId(e.target.value)}
          />
        </div>
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
            <label>Token</label>
            <input
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
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
              <label>Password</label>
              <input
                type="password"
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
            {busy ? "Working…" : "Add & Sync"}
          </button>
        </div>
      </div>
    </div>
  );
}
