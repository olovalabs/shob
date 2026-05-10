import { Tooltip as TooltipPrimitive } from "radix-ui"
import { useState, useEffect, useRef } from "react"
import { cn } from "../lib/utils"

export interface TooltipProps {
  value: React.ReactNode
  class?: string
  contentClass?: string
  contentStyle?: React.CSSProperties
  inactive?: boolean
  forceOpen?: boolean
  placement?: "top" | "bottom" | "left" | "right"
  gutter?: number
  ignoreSafeArea?: boolean
  skipDelayDuration?: number
  children?: React.ReactNode
}

export interface TooltipKeybindProps extends Omit<TooltipProps, "value"> {
  title: string
  keybind: string
}

function TooltipKeybind({ title, keybind, ...props }: TooltipKeybindProps) {
  return (
    <Tooltip
      {...props}
      value={
        <div data-slot="tooltip-keybind">
          <span>{title}</span>
          <span data-slot="tooltip-keybind-key">{keybind}</span>
        </div>
      }
    />
  )
}

function Tooltip({ value, children, class: className, contentClass, contentStyle, inactive, forceOpen, placement, gutter = 4, ignoreSafeArea = true, skipDelayDuration }: TooltipProps) {
  const [open, setOpen] = useState(false)
  const [block, setBlock] = useState(false)
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const el = ref.current
    if (!el) return

    const sync = () => {
      const expanded = !!el.querySelector('[aria-expanded="true"], [data-expanded]')
      if (expanded) {
        setBlock(true)
        setOpen(false)
      } else {
        setBlock(false)
      }
    }

    sync()
    const obs = new MutationObserver(sync)
    obs.observe(el, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeFilter: ["aria-expanded", "data-expanded"],
    })
    return () => obs.disconnect()
  }, [])

  if (inactive) return <>{children}</>

  return (
    <TooltipPrimitive.Root
      open={forceOpen || (open && !block)}
      onOpenChange={(next) => {
        if (next && block) return
        setOpen(next)
      }}
      skipDelayDuration={skipDelayDuration}
    >
      <TooltipPrimitive.Trigger asChild>
        <div ref={ref} data-component="tooltip-trigger" className={cn(className)}>
          {children}
        </div>
      </TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal>
        <TooltipPrimitive.Content
          data-component="tooltip"
          data-placement={placement}
          data-force-open={forceOpen}
          side={placement}
          sideOffset={gutter}
          className={cn(contentClass)}
          style={contentStyle}
          onPointerDownOutside={(e) => {
            if (ref.current === e.target || (e.target instanceof Node && ref.current?.contains(e.target))) {
              e.preventDefault()
            }
          }}
        >
          {value}
        </TooltipPrimitive.Content>
      </TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  )
}

export { Tooltip, TooltipKeybind }
