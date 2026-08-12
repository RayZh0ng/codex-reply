import type { HTMLAttributes, ReactNode } from "react";

export interface PageHeaderProps extends Omit<HTMLAttributes<HTMLElement>, "title"> {
  eyebrow?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  meta?: ReactNode;
  actions?: ReactNode;
}

export function PageHeader({
  eyebrow,
  title,
  description,
  meta,
  actions,
  className = "",
  ...props
}: PageHeaderProps) {
  return (
    <header
      {...props}
      className={`page-header ${className}`.trim()}
      data-animate="heading"
    >
      <div className="page-header-copy">
        {eyebrow && <p className="section-kicker">{eyebrow}</p>}
        <h1>{title}</h1>
        {description && <div className="page-header-description">{description}</div>}
        {meta && <div className="page-header-meta">{meta}</div>}
      </div>
      {actions && <div className="page-header-actions">{actions}</div>}
    </header>
  );
}
