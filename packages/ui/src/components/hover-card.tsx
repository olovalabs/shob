import { HoverCard as HoverCardPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export interface HoverCardProps
  extends React.PropsWithChildren,
    Omit<React.ComponentProps<typeof HoverCardPrimitive.Root>, "children"> {
  trigger: React.ReactNode
  mount?: HTMLElement
}

function HoverCard({ trigger, mount, className, children, ...props }: HoverCardProps) {
  return (
    <HoverCardPrimitive.Root {...props}>
      <HoverCardPrimitive.Trigger asChild>
        <div data-slot="hover-card-trigger" tabIndex={-1}>
          {trigger}
        </div>
      </HoverCardPrimitive.Trigger>
      <HoverCardPrimitive.Portal mount={mount}>
        <HoverCardPrimitive.Content
          data-component="hover-card-content"
          className={cn(className)}
        >
          <div data-slot="hover-card-body">{children}</div>
        </HoverCardPrimitive.Content>
      </HoverCardPrimitive.Portal>
    </HoverCardPrimitive.Root>
  )
}

export { HoverCard }
