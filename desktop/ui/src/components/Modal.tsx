import type { ReactNode } from "react";

type Props = {
  title: string;
  children: ReactNode;
  width?: string;
  role?: string;
  ariaLabel?: string;
};

export default function Modal({
  title,
  children,
  width = "w-[460px]",
  role = "dialog",
  ariaLabel,
}: Props) {
  return (
    <div className="fixed inset-0 z-10 flex items-center justify-center bg-black/30 p-5 backdrop-blur-sm">
      <div
        role={role}
        aria-label={ariaLabel ?? title}
        className={`max-h-[calc(100vh-40px)] ${width} max-w-full overflow-y-auto rounded-3xl border border-border-subtle bg-bg-elev p-7 shadow-sheet`}
      >
        <h3 className="m-0 mb-5 text-[17px] font-semibold tracking-tight">
          {title}
        </h3>
        {children}
      </div>
    </div>
  );
}
