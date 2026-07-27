import { cloneElement, useId, type ReactElement, type ReactNode } from "react";

export interface FormFieldProps {
  label: string;
  hint?: ReactNode;
  error?: ReactNode;
  required?: boolean;
  children: ReactElement<{
    id?: string;
    "aria-describedby"?: string;
    "aria-invalid"?: boolean;
  }>;
  className?: string;
}

export function FormField({
  label,
  hint,
  error,
  required = false,
  children,
  className = "",
}: FormFieldProps) {
  const generatedId = useId();
  const controlId = children.props.id ?? `field-${generatedId}`;
  const descriptionId = hint || error ? `${controlId}-description` : undefined;

  return (
    <label className={`form-field ${className}`.trim()} htmlFor={controlId}>
      <span className="form-field-label">
        {label}
        {required && <span aria-hidden="true">*</span>}
      </span>
      <span className="form-field-control">
        {cloneElement(children, {
          id: controlId,
          "aria-describedby": descriptionId,
          "aria-invalid": error ? true : undefined,
        })}
      </span>
      {(hint || error) && (
        <span
          className={`form-field-message ${error ? "is-error" : ""}`}
          id={descriptionId}
        >
          {error ?? hint}
        </span>
      )}
    </label>
  );
}
