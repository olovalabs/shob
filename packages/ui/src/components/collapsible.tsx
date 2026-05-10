import { Collapsible as CollapsiblePrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"
import { Icon } from "./icon"

export interface CollapsibleProps extends React.ComponentProps<typeof CollapsiblePrimitive.Root> {
  variant?: "normal" | "ghost"
}

function CollapsibleRoot({ className, variant = "normal", ...props }: CollapsibleProps) {
  return (
    <CollapsiblePrimitive.Root
      data-component="collapsible"
      data-variant={variant}
      className={cn(className)}
      {...props}
    />
  )
}

function CollapsibleTrigger({ className, ...props }: React.ComponentProps<typeof CollapsiblePrimitive.CollapsibleTrigger>) {
  return <CollapsiblePrimitive.CollapsibleTrigger data-slot="collapsible-trigger" className={cn(className)} {...props} />
}

function CollapsibleContent({ className, ...props }: React.ComponentProps<typeof CollapsiblePrimitive.CollapsibleContent>) {
  return <CollapsiblePrimitive.CollapsibleContent data-slot="collapsible-content" className={cn(className)} {...props} />
}

function CollapsibleArrow(props?: React.ComponentProps<"div">) {
  return (
    <div data-slot="collapsible-arrow" {...(props || {})}>
      <span data-slot="collapsible-arrow-icon">
        <Icon name="chevron-down" size="small" />
      </span>
    </div>
  )
}

export const Collapsible = Object.assign(CollapsibleRoot, {
  Arrow: CollapsibleArrow,
  Trigger: CollapsibleTrigger,
  Content: CollapsibleContent,
})
