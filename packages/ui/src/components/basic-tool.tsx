import { useEffect, type JSX } from "react"
import { animate, type AnimationPlaybackControls } from "motion"
import { useI18n } from "../context/i18n"
import { useState } from "react"
import { Collapsible } from "./collapsible"
import type { IconProps } from "./icon"
import { TextShimmer } from "./text-shimmer"

export type TriggerTitle = {
  title: string
  titleClass?: string
  subtitle?: string
  subtitleClass?: string
  args?: string[]
  argsClass?: string
  action?: JSX.Element
}

const isTriggerTitle = (val: any): val is TriggerTitle => {
  return (
    typeof val === "object" && val !== null && "title" in val && (typeof Node === "undefined" || !(val instanceof Node))
  )
}

export interface BasicToolProps {
  icon: IconProps["name"]
  trigger: TriggerTitle | JSX.Element
  children?: JSX.Element
  status?: string
  hideDetails?: boolean
  defaultOpen?: boolean
  forceOpen?: boolean
  defer?: boolean
  locked?: boolean
  animated?: boolean
  onSubtitleClick?: () => void
  onTriggerClick?: JSX.EventHandlerUnion<HTMLElement, MouseEvent>
  triggerHref?: string
  clickable?: boolean
}

const SPRING = { type: "spring" as const, visualDuration: 0.35, bounce: 0 }

export function BasicTool(props: BasicToolProps) {
  const [state, setState] = useState({
    open: props.defaultOpen ?? false,
    ready: props.defaultOpen ?? false,
  })
  const pending = props.status === "pending" || props.status === "running"

  let frame: number | undefined

  const cancel = () => {
    if (frame === undefined) return
    cancelAnimationFrame(frame)
    frame = undefined
  }

  useEffect(() => {
    if (props.forceOpen) setState(prev => ({ ...prev, open: true }))
  }, [props.forceOpen])

  useEffect(() => {
    if (!props.defer) return
    if (!state.open) {
      cancel()
      setState(prev => ({ ...prev, ready: false }))
      return
    }

    cancel()
    frame = requestAnimationFrame(() => {
      frame = undefined
      if (!state.open) return
      setState(prev => ({ ...prev, ready: true }))
    })

    return () => cancel()
  }, [state.open, props.defer])

  let contentRef: HTMLDivElement | undefined
  let heightAnim: AnimationPlaybackControls | undefined
  const initialOpen = state.open

  useEffect(() => {
    if (!props.animated || !contentRef) return
    heightAnim?.stop()
    if (state.open) {
      contentRef.style.overflow = "hidden"
      heightAnim = animate(contentRef, { height: "auto" }, SPRING)
      void heightAnim.finished.then(() => {
        if (!contentRef || !state.open) return
        contentRef.style.overflow = "visible"
        contentRef.style.height = "auto"
      })
    } else {
      contentRef.style.overflow = "hidden"
      heightAnim = animate(contentRef, { height: "0px" }, SPRING)
    }
    return () => heightAnim?.stop()
  }, [state.open, props.animated])

  const handleOpenChange = (value: boolean) => {
    if (pending) return
    if (props.locked && !value) return
    setState(prev => ({ ...prev, open: value }))
  }

  const triggerContent = (() => {
    if (isTriggerTitle(props.trigger)) {
      const title = props.trigger
      return (
        <div data-slot="basic-tool-tool-info-structured">
          <div data-slot="basic-tool-tool-info-main">
            <span data-slot="basic-tool-tool-title" className={title.titleClass}>
              <TextShimmer text={title.title} active={pending} />
            </span>
            {!pending && title.subtitle && (
              <span
                data-slot="basic-tool-tool-subtitle"
                className={`${title.subtitleClass ?? ""}${props.onSubtitleClick ? " clickable" : ""}`}
                onClick={(e) => {
                  if (props.onSubtitleClick) {
                    e.stopPropagation()
                    props.onSubtitleClick()
                  }
                }}
              >
                {title.subtitle}
              </span>
            )}
            {!pending && title.args?.length && title.args.map((arg) => (
              <span data-slot="basic-tool-tool-arg" className={title.argsClass} key={arg}>
                {arg}
              </span>
            ))}
          </div>
          {!pending && title.action && (
            <span data-slot="basic-tool-tool-action">{title.action}</span>
          )}
        </div>
      )
    }
    return props.trigger as JSX.Element
  })()

  const trigger = () => (
    <div
      data-component="tool-trigger"
      data-clickable={props.clickable ? "true" : undefined}
      data-hide-details={props.hideDetails ? "true" : undefined}
    >
      <div data-slot="basic-tool-tool-trigger-content">
        <div data-slot="basic-tool-tool-info">
          {triggerContent}
        </div>
      </div>
      {props.children && !props.hideDetails && !props.locked && !pending && (
        <Collapsible.Arrow />
      )}
    </div>
  )

  return (
    <Collapsible open={state.open} onOpenChange={handleOpenChange} className="tool-collapsible">
      {props.triggerHref ? (
        <Collapsible.Trigger
          as="a"
          href={props.triggerHref}
          data-hide-details={props.hideDetails ? "true" : undefined}
          onClick={props.onTriggerClick}
        >
          {trigger()}
        </Collapsible.Trigger>
      ) : (
        <Collapsible.Trigger
          data-hide-details={props.hideDetails ? "true" : undefined}
          onClick={props.onTriggerClick}
        >
          {trigger()}
        </Collapsible.Trigger>
      )}
      {props.animated && props.children && !props.hideDetails && (
        <div
          ref={(el) => { contentRef = el }}
          data-slot="collapsible-content"
          data-animated
          style={{
            height: initialOpen ? "auto" : "0px",
            overflow: initialOpen ? "visible" : "hidden",
          }}
        >
          {props.children}
        </div>
      )}
      {!props.animated && props.children && !props.hideDetails && (
        <Collapsible.Content>
          {(!props.defer || state.ready) && props.children}
        </Collapsible.Content>
      )}
    </Collapsible>
  )
}

function label(input: Record<string, unknown> | undefined) {
  const keys = ["description", "query", "url", "filePath", "path", "pattern", "name"]
  return keys.map((key) => input?.[key]).find((value): value is string => typeof value === "string" && value.length > 0)
}

function args(input: Record<string, unknown> | undefined) {
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
  const i18n = useI18n()

  return (
    <BasicTool
      icon="mcp"
      status={props.status}
      trigger={{
        title: i18n.t("ui.basicTool.called", { tool: props.tool }),
        subtitle: label(props.input),
        args: args(props.input),
      }}
      hideDetails={props.hideDetails}
    />
  )
}
