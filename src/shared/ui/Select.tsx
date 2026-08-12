import * as RadixSelect from "@radix-ui/react-select";
import { CaretDown } from "@phosphor-icons/react/CaretDown";
import { CaretUp } from "@phosphor-icons/react/CaretUp";
import { Check } from "@phosphor-icons/react/Check";
import { createContext, useContext, type ReactNode } from "react";

const SelectPortalContainerContext = createContext<HTMLElement | null>(null);

export function SelectPortalContainerProvider({
  children,
  container,
}: {
  children: ReactNode;
  container: HTMLElement | null;
}) {
  return (
    <SelectPortalContainerContext.Provider value={container}>
      {children}
    </SelectPortalContainerContext.Provider>
  );
}

export interface SelectOption {
  value: string;
  label: ReactNode;
  description?: ReactNode;
  disabled?: boolean;
  keywords?: string;
}

export interface SelectProps {
  id?: string;
  ariaLabel: string;
  "aria-describedby"?: string;
  "aria-invalid"?: boolean;
  disabled?: boolean;
  invalid?: boolean;
  onValueChange: (value: string) => void;
  options: SelectOption[];
  placeholder?: string;
  portalContainer?: HTMLElement | null;
  size?: "sm" | "md";
  value: string;
}

export function Select({
  id,
  ariaLabel,
  "aria-describedby": ariaDescribedBy,
  "aria-invalid": ariaInvalid,
  disabled = false,
  invalid = false,
  onValueChange,
  options,
  placeholder = "请选择",
  portalContainer,
  size = "md",
  value,
}: SelectProps) {
  const dialogPortalContainer = useContext(SelectPortalContainerContext);
  const current = options.find((option) => option.value === value);
  return (
    <RadixSelect.Root disabled={disabled} onValueChange={onValueChange} value={value}>
      <RadixSelect.Trigger
        aria-describedby={ariaDescribedBy}
        aria-invalid={ariaInvalid ?? (invalid || undefined)}
        aria-label={ariaLabel}
        className={`select-trigger select-${size}`}
        id={id}
      >
        <RadixSelect.Value placeholder={placeholder}>
          {current?.label}
        </RadixSelect.Value>
        <RadixSelect.Icon className="select-caret">
          <CaretDown size={18} weight="bold" />
        </RadixSelect.Icon>
      </RadixSelect.Trigger>
      <RadixSelect.Portal
        container={
          portalContainer === undefined ? dialogPortalContainer : portalContainer
        }
      >
        <RadixSelect.Content
          className="select-content"
          collisionPadding={12}
          position="popper"
          sideOffset={6}
        >
          <RadixSelect.ScrollUpButton className="select-scroll-button">
            <CaretUp size={15} weight="bold" />
          </RadixSelect.ScrollUpButton>
          <RadixSelect.Viewport className="select-viewport">
            {options.map((option) => (
              <RadixSelect.Item
                className="select-item"
                disabled={option.disabled}
                key={option.value}
                textValue={
                  typeof option.label === "string"
                    ? `${option.label} ${option.keywords ?? ""}`.trim()
                    : option.keywords
                }
                value={option.value}
              >
                <span className="select-item-copy">
                  <RadixSelect.ItemText>{option.label}</RadixSelect.ItemText>
                  {option.description && (
                    <span className="select-item-description">
                      {option.description}
                    </span>
                  )}
                </span>
                <RadixSelect.ItemIndicator className="select-indicator">
                  <Check size={16} weight="bold" />
                </RadixSelect.ItemIndicator>
              </RadixSelect.Item>
            ))}
          </RadixSelect.Viewport>
          <RadixSelect.ScrollDownButton className="select-scroll-button">
            <CaretDown size={15} weight="bold" />
          </RadixSelect.ScrollDownButton>
        </RadixSelect.Content>
      </RadixSelect.Portal>
    </RadixSelect.Root>
  );
}
