import type { HTMLAttributes, ReactNode } from "react";

export interface EmptyStateProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
  icon?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  compact?: boolean;
}

export function EmptyState({
  icon,
  title,
  description,
  action,
  compact = false,
  className = "",
  ...props
}: EmptyStateProps) {
  return (
    <div
      {...props}
      className={`empty-state ${compact ? "empty-state-compact" : ""} ${className}`.trim()}
    >
      {icon && <span className="empty-state-icon">{icon}</span>}
      <div className="empty-state-copy">
        <h2>{title}</h2>
        {description && <p>{description}</p>}
      </div>
      {action && <div className="empty-state-action">{action}</div>}
    </div>
  );
}
