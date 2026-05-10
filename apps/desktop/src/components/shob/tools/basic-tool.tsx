import { useState } from "react"
import {
  BrainIcon,
  ChevronDown,
  CircleHelpIcon,
  Code2Icon,
  FileTextIcon,
  GlobeIcon,
  ListIcon,
  SearchIcon,
  TerminalIcon,
  WrenchIcon,
} from "lucide-react"
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

const getIcon = (iconName?: string) => {
  const className = "h-4 w-4"
  switch (iconName) {
    case "console":
      return <TerminalIcon className={className} />
    case "brain":
      return <BrainIcon className={className} />
    case "bubble-5":
      return <CircleHelpIcon className={className} />
    case "bullet-list":
      return <ListIcon className={className} />
    case "code-lines":
    case "write":
      return <Code2Icon className={className} />
    case "glasses":
    case "read":
    case "file":
      return <FileTextIcon className={className} />
    case "magnifying-glass-menu":
    case "task":
      return <SearchIcon className={className} />
    case "window-cursor":
      return <GlobeIcon className={className} />
    case undefined:
      return null
    default:
      return <WrenchIcon className={className} />
  }
}

export function BasicTool(props: BasicToolProps) {
  const [open, setOpen] = useState(props.defaultOpen ?? false)
  const pending = props.status === "pending" || props.status === "running"
  const resolvedOpen = props.forceOpen || open

  const handleOpenChange = (value: boolean) => {
    if (pending) return
    if (props.forceOpen && !value) return
    if (props.locked && !value) return
    setOpen(value)
  }

  const renderTrigger = () => {
    const icon = getIcon(props.icon)

    return (
      <div
        data-component="tool-trigger"
        data-clickable={props.clickable ? "true" : undefined}
        data-hide-details={props.hideDetails ? "true" : undefined}
      >
        <div data-slot="basic-tool-tool-trigger-content">
          {icon && (
            <span data-slot="basic-tool-tool-indicator" aria-hidden="true">
              {icon}
            </span>
          )}
          <div data-slot="basic-tool-tool-info">
            {renderTriggerContent()}
          </div>
        </div>
        {props.children && !props.hideDetails && !props.locked && !pending && (
          <span data-slot="collapsible-arrow">
            <ChevronDown className="h-4 w-4" data-slot="collapsible-arrow-icon" />
          </span>
        )}
      </div>
    )
  }

  const renderTriggerContent = () => {
    if (isTriggerTitle(props.trigger)) {
      const title = props.trigger as TriggerTitle
      return (
        <div data-slot="basic-tool-tool-info-structured">
          <div data-slot="basic-tool-tool-info-main">
            <span data-slot="basic-tool-tool-title" className={cn(title.titleClass)}>
              <TextShimmer text={title.title} active={pending} />
            </span>
            {!pending && title.subtitle && (
              <span
                data-slot="basic-tool-tool-subtitle"
                className={cn(title.subtitleClass, props.onSubtitleClick && "clickable")}
                onClick={(e) => {
                  e.stopPropagation()
                  props.onSubtitleClick?.()
                }}
              >
                {title.subtitle}
              </span>
            )}
            {!pending && title.args?.length
              ? title.args.map((arg, i) => (
                  <span key={i} data-slot="basic-tool-tool-arg" className={title.argsClass}>
                    {arg}
                  </span>
                ))
              : null}
          </div>
          {!pending && title.action && (
            <span data-slot="basic-tool-tool-action">{title.action}</span>
          )}
        </div>
      )
    }
    return props.trigger
  }

  const triggerElement = props.triggerHref ? (
    <a href={props.triggerHref} onClick={props.onTriggerClick}>
      {renderTrigger()}
    </a>
  ) : (
    renderTrigger()
  )

  return (
    <Collapsible
      className="tool-collapsible"
      data-open={resolvedOpen ? "true" : undefined}
      open={resolvedOpen}
      onOpenChange={handleOpenChange}
    >
      <CollapsibleTrigger
        data-hide-details={props.hideDetails ? "true" : undefined}
        onClick={props.onTriggerClick}
      >
        {triggerElement}
      </CollapsibleTrigger>
      {props.children && !props.hideDetails && !pending && (
        <CollapsibleContent>
          {(!props.defer || resolvedOpen) && props.children}
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
