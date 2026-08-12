import type { HTMLAttributes, ReactNode } from "react";

export type InlineNoticeTone = "info" | "success" | "warning" | "danger";

export interface InlineNoticeProps extends Omit<
  HTMLAttributes<HTMLDivElement>,
  "title"
> {
  tone?: InlineNoticeTone;
  icon?: ReactNode;
  title?: ReactNode;
  action?: ReactNode;
  compact?: boolean;
}

export function InlineNotice({
  tone = "info",
  icon,
  title,
  action,
  compact = false,
  className = "",
  children,
  role,
  ...props
}: InlineNoticeProps) {
  return (
    <div
      {...props}
      className={`inline-notice inline-notice-${tone} ${compact ? "inline-notice-compact" : ""} ${className}`.trim()}
      role={role ?? (tone === "danger" ? "alert" : "status")}
    >
      {icon && <span className="inline-notice-icon">{icon}</span>}
      <div className="inline-notice-copy">
        {title && <strong>{title}</strong>}
        <div>{children}</div>
      </div>
      {action && <div className="inline-notice-action">{action}</div>}
    </div>
  );
}
