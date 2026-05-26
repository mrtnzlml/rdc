type Props = { onAdd: () => void };

export default function EmptyState({ onAdd }: Props) {
  return (
    <div className="empty">
      <h1>Sync a Rossum organization</h1>
      <p>Pull your Rossum config locally so Claude can read it.</p>
      <button className="btn btn-primary" onClick={onAdd}>
        Add Connection
      </button>
    </div>
  );
}
