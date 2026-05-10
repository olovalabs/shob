import { Switch as SwitchPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export interface SwitchProps extends React.PropsWithChildren<React.ComponentProps<typeof SwitchPrimitive.Root>> {
  hideLabel?: boolean
  description?: string
}

function Switch({ className, children, hideLabel, description, ...props }: SwitchProps) {
  return (
    <SwitchPrimitive.Root data-component="switch" className={cn(className)} {...props}>
      <SwitchPrimitive.Input data-slot="switch-input" />
      {children && (
        <SwitchPrimitive.Label data-slot="switch-label" className={cn(hideLabel && "sr-only")}>
          {children}
        </SwitchPrimitive.Label>
      )}
      <SwitchPrimitive.Thumb data-slot="switch-thumb" />
    </SwitchPrimitive.Root>
  )
}

export { Switch }
