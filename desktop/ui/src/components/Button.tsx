import type { ButtonHTMLAttributes, ReactNode } from "react";

type Variant = "secondary" | "primary" | "destructive" | "link";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant;
  icon?: ReactNode;
  children: ReactNode;
};

// Approximates macOS NSButton sizing: ~32px tall for regular buttons,
// ~24px for link buttons.
const base =
  "inline-flex cursor-pointer items-center justify-center gap-1.5 font-sans text-[13px] transition-all duration-150 disabled:cursor-not-allowed disabled:opacity-50 active:scale-[0.98]";

const variants: Record<Variant, string> = {
  secondary:
    "h-8 rounded-lg border border-border-subtle bg-bg-elev px-3.5 text-fg shadow-button hover:bg-row-hover",
  primary:
    "h-8 rounded-full bg-accent px-4 font-medium text-accent-fg shadow-button hover:brightness-105",
  destructive:
    "h-8 rounded-lg border border-border-subtle bg-bg-elev px-3.5 text-error shadow-button hover:bg-error/10",
  link: "border-none bg-transparent p-0 text-accent hover:underline",
};

export default function Button({
  variant = "secondary",
  icon,
  className = "",
  children,
  ...rest
}: Props) {
  return (
    <button {...rest} className={`${base} ${variants[variant]} ${className}`}>
      {icon}
      {children}
    </button>
  );
}
