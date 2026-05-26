import Button from "./Button";

type Props = {
  onAdd: () => void;
  onOpenExisting: () => void;
};

export default function EmptyState({ onAdd, onOpenExisting }: Props) {
  return (
    <div className="flex h-screen flex-col items-center justify-center gap-4 px-8 text-center">
      <h1 className="m-0 text-[28px] font-semibold tracking-tight">
        Sync a Rossum organization
      </h1>
      <p className="m-0 mb-3 max-w-[400px] text-[15px] text-fg-muted">
        Pull your Rossum config locally so Claude can read it.
      </p>
      <div className="flex flex-col items-center gap-2">
        <Button variant="primary" onClick={onAdd}>
          Add Connection
        </Button>
        <Button variant="link" onClick={onOpenExisting}>
          Open existing rdc project…
        </Button>
      </div>
    </div>
  );
}
