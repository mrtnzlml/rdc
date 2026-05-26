import { useEffect } from "react";
import type { ReactNode } from "react";

type Props = {
  title: string;
  children: ReactNode;
  onClose?: () => void;
  width?: string;
  role?: string;
  ariaLabel?: string;
};

export default function Modal({
  title,
  children,
  onClose,
  width = "w-[460px]",
  role = "dialog",
  ariaLabel,
}: Props) {
  // Esc closes the sheet — standard macOS behavior.
  useEffect(() => {
    if (!onClose) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <div className="fixed inset-0 z-10 flex animate-[fade-in_120ms_ease-out] items-center justify-center bg-black/30 p-5 backdrop-blur-sm">
      <div
        role={role}
        aria-label={ariaLabel ?? title}
        className={`max-h-[calc(100vh-40px)] ${width} max-w-full animate-[sheet-in_180ms_cubic-bezier(0.2,0.7,0.2,1)] overflow-y-auto rounded-3xl border border-border-subtle bg-bg-elev p-7 shadow-sheet`}
      >
        <h3 className="m-0 mb-5 text-[17px] font-semibold tracking-tight">
          {title}
        </h3>
        {children}
      </div>
    </div>
  );
}
