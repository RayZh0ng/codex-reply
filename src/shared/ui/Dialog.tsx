import {
  useEffect,
  useId,
  useRef,
  useState,
  type DialogHTMLAttributes,
  type ReactNode,
} from "react";

import { SelectPortalContainerProvider } from "./Select";

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
  busy?: boolean;
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
  busy = false,
  className = "",
  ...props
}: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const [portalContainer, setPortalContainer] = useState<HTMLDivElement | null>(null);
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
      aria-busy={busy || undefined}
      className={`dialog dialog-${size} ${className}`.trim()}
      onCancel={(event) => {
        event.preventDefault();
        if (!busy) onClose?.();
      }}
      onClick={(event) => {
        if (!busy && event.target === event.currentTarget) onClose?.();
      }}
      ref={ref}
    >
      <SelectPortalContainerProvider container={portalContainer}>
        <section className="dialog-panel">
          <header>
            <h2 id={titleId}>{title}</h2>
            {description && <p>{description}</p>}
          </header>
          {children && <div className="dialog-content">{children}</div>}
          {footer && <footer>{footer}</footer>}
        </section>
        <div className="dialog-portal-root" ref={setPortalContainer} />
      </SelectPortalContainerProvider>
    </dialog>
  );
}
