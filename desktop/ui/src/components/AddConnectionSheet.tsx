import { useState } from "react";
import { api } from "../api";
import type { AuthKind, ConnectionSummary } from "../types";
import Button from "./Button";
import Modal from "./Modal";
import Field from "./Field";

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
    <Modal title="Add Connection" ariaLabel="Add Connection">
      {error && (
        <div className="mb-4 rounded-xl border border-error/30 bg-error/10 px-4 py-3 text-error">
          {error}
        </div>
      )}
      <Field label="Name">
        <input
          type="text"
          placeholder="Acme Corp — Production"
          value={name}
          onChange={(e) => setName(e.target.value)}
          autoFocus
        />
      </Field>
      <Field label="API URL">
        <input
          type="url"
          placeholder="https://acme.app.rossum.ai/api/v1"
          value={apiBase}
          onChange={(e) => setApiBase(e.target.value)}
        />
      </Field>
      <Field label="Org ID">
        <input
          type="number"
          min="1"
          value={orgId}
          onChange={(e) => setOrgId(e.target.value)}
        />
      </Field>
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
        <Field label="Token">
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
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
          <Field label="Password">
            <input
              type="password"
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
          {busy ? "Working…" : "Add & Sync"}
        </Button>
      </div>
    </Modal>
  );
}
