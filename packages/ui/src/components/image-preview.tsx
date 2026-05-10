import { Dialog as DialogPrimitive } from "radix-ui"
import { useI18n } from "../context/i18n"
import { IconButton } from "./icon-button"

export interface ImagePreviewProps {
  src: string
  alt?: string
  open?: boolean
  onOpenChange?: (open: boolean) => void
}

function ImagePreview({ src, alt, open, onOpenChange }: ImagePreviewProps) {
  const i18n = useI18n()
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay />
        <DialogPrimitive.Content data-component="image-preview" data-slot="image-preview-content">
          <div data-slot="image-preview-header">
            <DialogPrimitive.Close asChild>
              <IconButton icon="close" variant="ghost" aria-label={i18n.t("ui.common.close")} />
            </DialogPrimitive.Close>
          </div>
          <div data-slot="image-preview-body">
            <img src={src} alt={alt ?? i18n.t("ui.imagePreview.alt")} data-slot="image-preview-image" />
          </div>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  )
}

export { ImagePreview }
