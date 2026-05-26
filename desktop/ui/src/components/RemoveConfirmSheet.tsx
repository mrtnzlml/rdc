import { useState } from "react";
import { api } from "../api";
import type { ConnectionSummary } from "../types";

type Props = {
  connection: ConnectionSummary;
  onCancel: () => void;
  onRemoved: () => void;
};

export default function RemoveConfirmSheet({ connection: c, onCancel, onRemoved }: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const confirm = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.removeConnection(c.id);
      onRemoved();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop">
      <div className="modal">
        <h3>Remove "{c.name}"?</h3>
        <p>
          This will delete the local folder (<code>{c.folder}</code>) and remove the stored sign-in.
          Rossum data is not affected.
        </p>
        {error && <div className="banner banner-error">{error}</div>}
        <div className="modal-actions">
          <button className="btn" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button className="btn btn-destructive" onClick={confirm} disabled={busy}>
            {busy ? "Removing…" : "Remove"}
          </button>
        </div>
      </div>
    </div>
  );
}
