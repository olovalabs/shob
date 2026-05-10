import { Checkbox as CheckboxPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export interface CheckboxProps extends React.PropsWithChildren<React.ComponentProps<typeof CheckboxPrimitive.Root>> {
  hideLabel?: boolean
  description?: string
  icon?: React.ReactNode
}

function Checkbox({ className, children, hideLabel, description, icon, ...props }: CheckboxProps) {
  return (
    <div data-component="checkbox" className={cn("flex items-start gap-2", className)}>
      <CheckboxPrimitive.Root data-slot="checkbox-checkbox-control" {...props}>
        <CheckboxPrimitive.Indicator data-slot="checkbox-checkbox-indicator">
          {icon || (
            <svg viewBox="0 0 12 12" fill="none" width="10" height="10" xmlns="http://www.w3.org/2000/svg">
              <path d="M3 7.17905L5.02703 8.85135L9 3.5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="square" />
            </svg>
          )}
        </CheckboxPrimitive.Indicator>
      </CheckboxPrimitive.Root>
      <div data-slot="checkbox-checkbox-content">
        {children && (
          <label data-slot="checkbox-checkbox-label" className={cn(hideLabel && "sr-only")}>
            {children}
          </label>
        )}
        {description && <p data-slot="checkbox-checkbox-description">{description}</p>}
      </div>
    </div>
  )
}

export { Checkbox }
