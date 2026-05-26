import type { ReactNode } from "react";

type Props = {
  icon: ReactNode;
  onClick?: () => void;
  title?: string;
  variant?: "default" | "destructive";
  disabled?: boolean;
};

// Toolbar-style icon-only button. Subtle by default; gains a tinted
// background on hover, like macOS toolbar buttons.
export default function IconButton({
  icon,
  onClick,
  title,
  variant = "default",
  disabled,
}: Props) {
  const classes =
    variant === "destructive"
      ? "text-error hover:bg-error/10"
      : "text-fg-muted hover:bg-row-hover hover:text-fg";
  return (
    <button
      onClick={onClick}
      title={title}
      aria-label={title}
      disabled={disabled}
      className={`inline-flex h-8 w-8 cursor-pointer items-center justify-center rounded-lg transition-all duration-150 disabled:cursor-not-allowed disabled:opacity-40 active:scale-95 ${classes}`}
    >
      {icon}
    </button>
  );
}
