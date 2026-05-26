import type { ButtonHTMLAttributes, ReactNode } from "react";

type Variant = "secondary" | "primary" | "destructive" | "link";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant;
  children: ReactNode;
};

const base =
  "cursor-pointer font-sans text-[13px] transition-all duration-150 disabled:cursor-not-allowed disabled:opacity-50 active:scale-[0.98]";

const variants: Record<Variant, string> = {
  secondary:
    "rounded-lg border border-border-subtle bg-bg-elev px-3.5 py-1.5 text-fg shadow-button hover:bg-row-hover",
  primary:
    "rounded-full border border-accent/0 bg-accent px-4 py-1.5 font-medium text-accent-fg shadow-button hover:brightness-105",
  destructive:
    "rounded-lg border border-border-subtle bg-bg-elev px-3.5 py-1.5 text-error shadow-button hover:bg-error/10",
  link: "border-none bg-transparent p-0 text-accent hover:underline",
};

export default function Button({
  variant = "secondary",
  className = "",
  children,
  ...rest
}: Props) {
  return (
    <button {...rest} className={`${base} ${variants[variant]} ${className}`}>
      {children}
    </button>
  );
}
