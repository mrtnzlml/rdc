import type { ReactNode } from "react";

type Props = { label: string; children: ReactNode };

export default function Field({ label, children }: Props) {
  return (
    <div className="mb-3 flex flex-col gap-1">
      <label className="text-xs text-fg-muted">{label}</label>
      {children}
    </div>
  );
}
