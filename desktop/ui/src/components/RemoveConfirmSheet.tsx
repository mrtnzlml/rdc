import { useState } from "react";
import { api } from "../api";
import type { ConnectionSummary } from "../types";
import Button from "./Button";
import Modal from "./Modal";

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
    <Modal title={`Remove "${c.name}"?`}>
      <p className="mb-4 text-[13px] text-fg-muted">
        This will delete the local folder (<code className="rounded bg-bg-sidebar px-1 py-0.5 text-xs">{c.folder}</code>) and remove the stored sign-in. Rossum data is not affected.
      </p>
      {error && (
        <div className="mb-4 rounded-xl border border-error/30 bg-error/10 px-4 py-3 text-error">
          {error}
        </div>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onCancel} disabled={busy}>
          Cancel
        </Button>
        <Button variant="destructive" onClick={confirm} disabled={busy}>
          {busy ? "Removing…" : "Remove"}
        </Button>
      </div>
    </Modal>
  );
}
