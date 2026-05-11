import { ToolErrorCard } from "./tool-error-card"
import { FallbackTool } from "./fallback-tool"
import { ToolRegistry } from "./tool-registry"
import type { ToolCallView } from "@/components/AgentView"
import { Markdown } from "@/components/shob/tools/markdown"
import { useCallback, useEffect, useRef, useState } from "react"

const TEXT_RENDER_PACE_MS = 30
const TEXT_RENDER_SNAP = /[\s.,!?;:)\]]/

function step(size: number) {
  if (size <= 12) return 2
  if (size <= 48) return 4
  if (size <= 96) return 8
  return Math.min(24, Math.ceil(size / 8))
}

function next(text: string, start: number) {
  const end = Math.min(text.length, start + step(text.length - start))
  const max = Math.min(text.length, end + 8)
  for (let i = end; i < max; i++) {
    if (TEXT_RENDER_SNAP.test(text[i] ?? "")) return i + 1
  }
  return end
}

function usePacedValue(getValue: () => string, live?: boolean) {
  const [value, setValue] = useState(getValue)
  const shownRef = useRef(getValue())
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  const frameRef = useRef<number | undefined>(undefined)
  const sourceRef = useRef(getValue())
  const liveRef = useRef(live)
  liveRef.current = live

  const clear = useCallback(() => {
    if (timeoutRef.current !== undefined) {
      clearTimeout(timeoutRef.current)
      timeoutRef.current = undefined
    }
    if (frameRef.current !== undefined) {
      cancelAnimationFrame(frameRef.current)
      frameRef.current = undefined
    }
  }, [])

  const sync = useCallback((text: string) => {
    if (shownRef.current === text) return
    shownRef.current = text
    setValue(text)
  }, [])

  const run = useCallback(() => {
    timeoutRef.current = undefined
    const text = getValue()
    if (!liveRef.current) { sync(text); return }
    if (!text.startsWith(shownRef.current) || text.length <= shownRef.current.length) { sync(text); return }
    const end = next(text, shownRef.current.length)
    sync(text.slice(0, end))
    if (end < text.length) {
      timeoutRef.current = setTimeout(run, TEXT_RENDER_PACE_MS)
    }
  }, [getValue, sync])

  const start = useCallback(() => {
    const text = getValue()
    if (!liveRef.current) { clear(); sync(text); return }
    if (!text.startsWith(shownRef.current) || text.length < shownRef.current.length) { clear(); sync(text); return }
    if (text.length <= shownRef.current.length) return
    if (timeoutRef.current !== undefined) return
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = undefined
      timeoutRef.current = setTimeout(run, TEXT_RENDER_PACE_MS)
    })
  }, [getValue, sync, run, clear])

  useEffect(() => {
    const current = getValue()
    if (current === sourceRef.current && !liveRef.current) return
    sourceRef.current = current
    start()
  })

  useEffect(() => () => clear(), [clear])
  return value
}


export interface MessagePartProps {
  part: {
    id: string
    type: string
    text?: string
    tool?: string
    callID?: string
    state?: {
      status?: string
      title?: string
      input?: unknown
      output?: string
      error?: string
      metadata?: Record<string, unknown>
      attachments?: unknown[]
      time?: {
        start?: number
        end?: number
        compacted?: number
      }
    }
  }
  message?: {
    id: string
    role: string
    agent?: string
    model?: { providerID?: string; modelID?: string }
    time?: { created?: number; completed?: number }
    error?: { name?: string }
  }
  hideDetails?: boolean
  defaultOpen?: boolean
  turnDurationMs?: number
  showCopy?: boolean
  working?: boolean
}

export function Part(props: MessagePartProps) {
  const { part } = props

  switch (part.type) {
    case "text":
      return <TextPartDisplay {...props} />
    case "reasoning":
      return <ReasoningPartDisplay {...props} />
    case "tool":
      return <ToolPartDisplay {...props} />
    case "compaction":
      return <CompactionPartDisplay />
    default:
      return null
  }
}

function TextPartDisplay({ part, working }: MessagePartProps) {
  const text = part.text ?? ""
  if (!text.trim()) return null

  const pacedText = usePacedValue(
    () => part.text ?? "",
    working ?? false,
  )

  return (
    <div data-component="text-part">
      <Markdown text={pacedText} cacheKey={part.id} streaming={working} />
    </div>
  )
}

function ReasoningPartDisplay({ part }: MessagePartProps) {
  const text = part.text ?? ""
  if (!text.trim()) return null

  return (
    <div data-component="reasoning-part">
      <div data-component="markdown">{text}</div>
    </div>
  )
}

function ToolPartDisplay({ part, defaultOpen, hideDetails, working }: MessagePartProps) {
  if (!part.tool) return null

  const input = (part.state?.input ?? {}) as Record<string, unknown>
  const partMetadata = (part.state?.metadata ?? {}) as Record<string, unknown>

  if (part.state?.status === "error" && part.state.error) {
    return (
      <div data-component="tool-part-wrapper">
        <ToolErrorCard
          tool={part.tool}
          error={part.state.error}
          defaultOpen={defaultOpen}
        />
      </div>
    )
  }

  const toolCall: ToolCallView = {
    id: part.id,
    callID: part.callID,
    tool: part.tool,
    status: part.state?.status ?? "completed",
    title: part.state?.title,
    input: input,
    output: part.state?.output,
    error: part.state?.error,
    metadata: partMetadata,
    startedAt: part.state?.time?.start,
    endedAt: part.state?.time?.end,
  }
  const RegisteredTool = ToolRegistry.render(part.tool)

  return (
    <div data-component="tool-part-wrapper">
      {RegisteredTool ? (
        // eslint-disable-next-line react-hooks/static-components
        <RegisteredTool
          tool={toolCall.tool}
          status={toolCall.status}
          input={input}
          output={toolCall.output ?? undefined}
          metadata={partMetadata}
          defaultOpen={defaultOpen ?? false}
          hideDetails={hideDetails}
        />
      ) : (
        <FallbackTool toolCall={toolCall} defaultOpen={defaultOpen} />
      )}
    </div>
  )
}

function CompactionPartDisplay() {
  return (
    <div data-component="compaction-part">
      <div data-slot="compaction-part-divider">
        <span data-slot="compaction-part-line" />
        <span data-slot="compaction-part-label">Earlier messages compacted</span>
        <span data-slot="compaction-part-line" />
      </div>
    </div>
  )
}
