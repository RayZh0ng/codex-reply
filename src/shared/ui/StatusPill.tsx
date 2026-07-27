import type { HTMLAttributes, ReactNode } from "react";

export type StatusTone =
  "neutral" | "success" | "warning" | "danger" | "running" | "disabled";

export interface StatusPillProps extends HTMLAttributes<HTMLSpanElement> {
  tone?: StatusTone;
  compact?: boolean;
  showDot?: boolean;
  icon?: ReactNode;
}

export function StatusPill({
  tone = "neutral",
  compact = false,
  showDot = true,
  icon,
  className = "",
  children,
  ...props
}: StatusPillProps) {
  return (
    <span
      {...props}
      className={`status-pill ${tone} ${compact ? "compact" : ""} ${className}`.trim()}
    >
      {icon ?? (showDot ? <i aria-hidden="true" /> : null)}
      {children}
    </span>
  );
}
