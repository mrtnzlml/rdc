import { useEffect, useRef } from "react";
import type { ReactNode } from "react";

export type ContextMenuItem =
  | { label: string; icon?: ReactNode; onClick: () => void; destructive?: boolean }
  | { separator: true };

type Props = {
  x: number;
  y: number;
  items: ContextMenuItem[];
  onClose: () => void;
};

export default function ContextMenu({ x, y, items, onClose }: Props) {
  const ref = useRef<HTMLDivElement>(null);

  // Dismiss on outside click or Esc — matches macOS context-menu behavior.
  useEffect(() => {
    const onPointer = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    // Defer attachment to next frame so the click that opened the menu
    // doesn't immediately dismiss it.
    const t = setTimeout(() => {
      window.addEventListener("mousedown", onPointer);
      window.addEventListener("contextmenu", onPointer);
      window.addEventListener("keydown", onKey);
    }, 0);
    return () => {
      clearTimeout(t);
      window.removeEventListener("mousedown", onPointer);
      window.removeEventListener("contextmenu", onPointer);
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  return (
    <div
      ref={ref}
      style={{ top: y, left: x }}
      className="fixed z-[100] min-w-[200px] animate-[fade-in_80ms_ease-out] overflow-hidden rounded-xl border border-border-subtle bg-bg-elev/95 py-1 shadow-sheet backdrop-blur-md"
    >
      {items.map((it, i) =>
        "separator" in it ? (
          <div key={i} className="my-1 border-t border-border-subtle" />
        ) : (
          <button
            key={i}
            onClick={() => {
              it.onClick();
              onClose();
            }}
            className={`flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-[13px] transition-colors hover:bg-accent hover:text-accent-fg ${
              it.destructive ? "text-error" : "text-fg"
            }`}
          >
            {it.icon && <span className="inline-flex h-4 w-4 items-center justify-center">{it.icon}</span>}
            {it.label}
          </button>
        ),
      )}
    </div>
  );
}
