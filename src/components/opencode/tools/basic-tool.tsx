import { useEffect, useRef, useState } from "react"
import { animate } from "motion"
import { ChevronDown } from "lucide-react"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { cn } from "@/lib/utils"
import { TextShimmer } from "./text-shimmer"

export type TriggerTitle = {
  title: string
  titleClass?: string
  subtitle?: string
  subtitleClass?: string
  args?: string[]
  argsClass?: string
  action?: React.ReactNode
}

const isTriggerTitle = (val: unknown): val is TriggerTitle => {
  return typeof val === "object" && val !== null && "title" in val
}

export interface BasicToolProps {
  icon?: string
  trigger: TriggerTitle | React.ReactNode
  children?: React.ReactNode
  status?: string
  hideDetails?: boolean
  defaultOpen?: boolean
  forceOpen?: boolean
  defer?: boolean
  locked?: boolean
  animated?: boolean
  onSubtitleClick?: () => void
  onTriggerClick?: React.MouseEventHandler
  triggerHref?: string
  clickable?: boolean
}

const SPRING = { type: "spring" as const, visualDuration: 0.35, bounce: 0 }

export function BasicTool(props: BasicToolProps) {
  const [open, setOpen] = useState(props.defaultOpen ?? false)
  const [ready, setReady] = useState(props.defaultOpen ?? false)
  const pending = () => props.status === "pending" || props.status === "running"

  const contentRef = useRef<HTMLDivElement>(null)
  const initialOpen = useRef(open)
  const frameRef = useRef<number | undefined>(undefined)
  const heightAnimRef = useRef<ReturnType<typeof animate> | undefined>(undefined)

  useEffect(() => {
    if (props.forceOpen) setOpen(true)
  }, [props.forceOpen])

  useEffect(() => {
    if (!props.defer) return
    if (!open) {
      if (frameRef.current) cancelAnimationFrame(frameRef.current)
      setReady(false)
      return
    }
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = undefined
      if (!open) return
      setReady(true)
    })
    return () => {
      if (frameRef.current) cancelAnimationFrame(frameRef.current)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  useEffect(() => {
    if (!props.animated || !contentRef.current) return
    heightAnimRef.current?.stop()
    if (open) {
      contentRef.current.style.overflow = "hidden"
      heightAnimRef.current = animate(contentRef.current, { height: "auto" }, SPRING)
      heightAnimRef.current.finished.then(() => {
        if (!contentRef.current || !open) return
        contentRef.current.style.overflow = "visible"
        contentRef.current.style.height = "auto"
      })
    } else {
      contentRef.current.style.overflow = "hidden"
      heightAnimRef.current = animate(contentRef.current, { height: "0px" }, SPRING)
    }
    return () => {
      heightAnimRef.current?.stop()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const handleOpenChange = (value: boolean) => {
    if (pending()) return
    if (props.locked && !value) return
    setOpen(value)
  }

  const renderTrigger = () => (
    <div
      data-component="tool-trigger"
      data-clickable={props.clickable ? "true" : undefined}
      data-hide-details={props.hideDetails ? "true" : undefined}
    >
      <div data-slot="basic-tool-tool-trigger-content">
        <div data-slot="basic-tool-tool-info">
          {isTriggerTitle(props.trigger) && props.trigger ? (
            (() => {
              const title = props.trigger as TriggerTitle
              return (
                <div data-slot="basic-tool-tool-info-structured">
                  <div data-slot="basic-tool-tool-info-main">
                    <span
                      data-slot="basic-tool-tool-title"
                      className={cn(title.titleClass)}
                    >
                      <TextShimmer text={title.title} active={pending()} />
                    </span>
                    {!pending() && title.subtitle && (
                      <span
                        data-slot="basic-tool-tool-subtitle"
                        className={cn(title.subtitleClass, props.clickable && "clickable")}
                        onClick={(e) => {
                          e.stopPropagation()
                          props.onSubtitleClick?.()
                        }}
                      >
                        {title.subtitle}
                      </span>
                    )}
                    {!pending() && title.args?.length && (
                      title.args.map((arg, i) => (
                        <span key={i} data-slot="basic-tool-tool-arg" className={title.argsClass}>
                          {arg}
                        </span>
                      ))
                    )}
                  </div>
                  {!pending() && title.action && (
                    <span data-slot="basic-tool-tool-action">{title.action}</span>
                  )}
                </div>
              )
            })()
          ) : (
            props.trigger
          )}
        </div>
      </div>
      {props.children && !props.hideDetails && !props.locked && !pending() && (
        <span data-slot="collapsible-arrow">
          <ChevronDown className="h-4 w-4" data-slot="collapsible-arrow-icon" />
        </span>
      )}
    </div>
  )

  const triggerElement = props.triggerHref ? (
    <a href={props.triggerHref} onClick={props.onTriggerClick}>
      {renderTrigger()}
    </a>
  ) : (
    renderTrigger()
  )

  return (
    <Collapsible open={open} onOpenChange={handleOpenChange} className="tool-collapsible">
      <CollapsibleTrigger
        data-hide-details={props.hideDetails ? "true" : undefined}
        onClick={props.onTriggerClick}
      >
        {triggerElement}
      </CollapsibleTrigger>
      {props.animated && props.children && !props.hideDetails && (
        <div
          ref={contentRef}
          data-slot="collapsible-content"
          data-animated
          style={{
            height: initialOpen.current ? "auto" : "0px",
            overflow: initialOpen.current ? "visible" : "hidden" as const,
          }}
        >
          {props.children}
        </div>
      )}
      {!props.animated && props.children && !props.hideDetails && (
        <CollapsibleContent>
          {(!props.defer || ready) && props.children}
        </CollapsibleContent>
      )}
    </Collapsible>
  )
}

function label(input: Record<string, unknown> | undefined): string | undefined {
  const keys = ["description", "query", "url", "filePath", "path", "pattern", "name"]
  return keys.map((key) => input?.[key]).find((value): value is string => typeof value === "string" && value.length > 0)
}

function args(input: Record<string, unknown> | undefined): string[] {
  if (!input) return []
  const skip = new Set(["description", "query", "url", "filePath", "path", "pattern", "name"])
  return Object.entries(input)
    .filter(([key]) => !skip.has(key))
    .flatMap(([key, value]) => {
      if (typeof value === "string") return [`${key}=${value}`]
      if (typeof value === "number") return [`${key}=${value}`]
      if (typeof value === "boolean") return [`${key}=${value}`]
      return []
    })
    .slice(0, 3)
}

export function GenericTool(props: {
  tool: string
  status?: string
  hideDetails?: boolean
  input?: Record<string, unknown>
}) {
  return (
    <BasicTool
      icon="mcp"
      status={props.status}
      trigger={{
        title: `${props.tool} called`,
        subtitle: label(props.input),
        args: args(props.input),
      }}
      hideDetails={props.hideDetails}
    />
  )
}
