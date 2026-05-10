import { Dialog as DialogPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"
import { useI18n } from "../context/i18n"
import { IconButton } from "./icon-button"

export interface DialogProps extends React.PropsWithChildren {
  title?: React.ReactNode
  description?: React.ReactNode
  action?: React.ReactNode
  size?: "normal" | "large" | "x-large"
  fit?: boolean
  transition?: boolean
  open?: boolean
  onOpenChange?: (open: boolean) => void
}

function Dialog({ title, description, action, size = "normal", fit, transition, open, onOpenChange, className, children }: DialogProps) {
  const i18n = useI18n()
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay data-slot="dialog-overlay" />
        <DialogPrimitive.Content
          data-component="dialog"
          data-fit={fit ? true : undefined}
          data-size={size}
          data-transition={transition ? true : undefined}
          data-slot="dialog-content"
          className={cn(className)}
          onOpenAutoFocus={(e) => {
            const target = e.currentTarget as HTMLElement | null
            const autofocusEl = target?.querySelector("[autofocus]") as HTMLElement | null
            if (autofocusEl) {
              e.preventDefault()
              autofocusEl.focus()
            }
          }}
        >
          <div data-slot="dialog-container">
            {(title || action) && (
              <div data-slot="dialog-header">
                {title && <DialogPrimitive.Title data-slot="dialog-title">{title}</DialogPrimitive.Title>}
                {action || (
                  <DialogPrimitive.Close asChild>
                    <IconButton icon="close" variant="ghost" aria-label={i18n.t("ui.common.close")} />
                  </DialogPrimitive.Close>
                )}
              </div>
            )}
            {description && (
              <DialogPrimitive.Description data-slot="dialog-description">
                {description}
              </DialogPrimitive.Description>
            )}
            <div data-slot="dialog-body">{children}</div>
          </div>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  )
}

export { Dialog }
