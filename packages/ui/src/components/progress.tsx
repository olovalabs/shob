import { Progress as ProgressPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export interface ProgressProps extends React.PropsWithChildren<React.ComponentProps<typeof ProgressPrimitive.Root>> {
  hideLabel?: boolean
  showValueLabel?: boolean
}

function Progress({ className, children, hideLabel, showValueLabel, value, ...props }: ProgressProps) {
  return (
    <ProgressPrimitive.Root data-component="progress" className={cn(className)} value={value} {...props}>
      {(children || showValueLabel) && (
        <div data-slot="progress-header">
          {children && (
            <ProgressPrimitive.Label
              data-slot="progress-label"
              className={cn(hideLabel && "sr-only")}
            >
              {children}
            </ProgressPrimitive.Label>
          )}
          {showValueLabel && <ProgressPrimitive.ValueLabel data-slot="progress-value-label" />}
        </div>
      )}
      <ProgressPrimitive.Indicator data-slot="progress-fill" />
    </ProgressPrimitive.Root>
  )
}

export { Progress }
