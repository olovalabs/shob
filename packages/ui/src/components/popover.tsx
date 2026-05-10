import { Popover as PopoverPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"
import { useI18n } from "../context/i18n"
import { IconButton } from "./icon-button"

export interface PopoverProps extends React.PropsWithChildren {
  trigger?: React.ReactNode
  triggerAs?: React.ElementType
  triggerProps?: Record<string, unknown>
  title?: React.ReactNode
  description?: React.ReactNode
  portal?: boolean
  open?: boolean
  defaultOpen?: boolean
  onOpenChange?: (open: boolean) => void
  modal?: boolean
  class?: string
  style?: React.CSSProperties
}

function Popover({ trigger, triggerAs, triggerProps, title, description, portal = true, open, defaultOpen, onOpenChange, modal = false, class: className, style, children }: PopoverProps) {
  const i18n = useI18n()
  const TriggerComp = triggerAs ?? "div"

  const content = (
    <PopoverPrimitive.Content
      data-component="popover-content"
      className={cn(className)}
      style={style}
    >
      {title && (
        <div data-slot="popover-header">
          <PopoverPrimitive.Title data-slot="popover-title">{title}</PopoverPrimitive.Title>
          <PopoverPrimitive.Close asChild>
            <IconButton icon="close" variant="ghost" aria-label={i18n.t("ui.common.close")} />
          </PopoverPrimitive.Close>
        </div>
      )}
      {description && (
        <PopoverPrimitive.Description data-slot="popover-description">{description}</PopoverPrimitive.Description>
      )}
      <div data-slot="popover-body">{children}</div>
    </PopoverPrimitive.Content>
  )

  return (
    <PopoverPrimitive.Root open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange} modal={modal}>
      <PopoverPrimitive.Trigger asChild data-slot="popover-trigger" {...(triggerProps as Record<string, unknown>)}>
        <TriggerComp>{trigger}</TriggerComp>
      </PopoverPrimitive.Trigger>
      {portal ? <PopoverPrimitive.Portal>{content}</PopoverPrimitive.Portal> : content}
    </PopoverPrimitive.Root>
  )
}

export { Popover }
