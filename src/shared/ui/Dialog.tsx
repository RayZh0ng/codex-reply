import {
  useEffect,
  useId,
  useRef,
  type DialogHTMLAttributes,
  type ReactNode,
} from "react";

export interface DialogProps extends Omit<
  DialogHTMLAttributes<HTMLDialogElement>,
  "open"
> {
  open: boolean;
  title: string;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  onClose?: () => void;
  size?: DialogSize;
}

export type DialogSize = "sm" | "md" | "lg";

export function Dialog({
  open,
  title,
  description,
  children,
  footer,
  onClose,
  size = "sm",
  className = "",
  ...props
}: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = `dialog-${useId()}`;

  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) {
      if (typeof dialog.showModal === "function") dialog.showModal();
      else dialog.setAttribute("open", "");
    } else if (!open && dialog.open) {
      if (typeof dialog.close === "function") dialog.close();
      else dialog.removeAttribute("open");
    }
  }, [open]);

  if (!open) return null;

  return (
    <dialog
      {...props}
      aria-labelledby={titleId}
      className={`dialog dialog-${size} ${className}`.trim()}
      onCancel={(event) => {
        event.preventDefault();
        onClose?.();
      }}
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose?.();
      }}
      ref={ref}
    >
      <section className="dialog-panel">
        <header>
          <h2 id={titleId}>{title}</h2>
          {description && <p>{description}</p>}
        </header>
        {children && <div className="dialog-content">{children}</div>}
        {footer && <footer>{footer}</footer>}
      </section>
    </dialog>
  );
}
