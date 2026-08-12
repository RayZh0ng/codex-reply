import type { ButtonHTMLAttributes, ReactNode } from "react";

export type ButtonVariant = "primary" | "secondary" | "quiet" | "danger" | "icon";
export type ButtonSize = "sm" | "md";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  leadingIcon?: ReactNode;
  loading?: boolean;
  loadingLabel?: string;
}

export function Button({
  variant = "secondary",
  size = "md",
  leadingIcon,
  loading = false,
  loadingLabel = "正在处理",
  className = "",
  children,
  type = "button",
  disabled,
  ...props
}: ButtonProps) {
  return (
    <button
      {...props}
      aria-busy={loading || undefined}
      className={`button button-${variant} button-${size} ${className}`.trim()}
      disabled={disabled || loading}
      type={type}
    >
      {loading ? <span aria-hidden="true" className="button-spinner" /> : leadingIcon}
      <span className="button-label">{loading ? loadingLabel : children}</span>
    </button>
  );
}
