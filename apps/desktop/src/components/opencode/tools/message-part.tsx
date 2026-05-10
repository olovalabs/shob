import { ToolErrorCard } from "./tool-error-card"
import { Markdown } from "./markdown"
import { FallbackTool } from "./fallback-tool"
import { ToolRegistry } from "./tool-registry"
import type { ToolCallView } from "@/components/AgentView"

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

function TextPartDisplay({ part, message, showCopy, working }: MessagePartProps) {
  const text = (part.text ?? "").trim()
  if (!text) return null

  const interrupted = message?.role === "assistant" && message?.error?.name === "MessageAbortedError"
  let meta = ""
  if (message?.role === "assistant") {
    const items: string[] = []
    if (message.agent) items.push(message.agent[0].toUpperCase() + message.agent.slice(1))
    if (message.model?.modelID) items.push(message.model.modelID)
    if (interrupted) items.push("Interrupted")
    meta = items.join(" · ")
  }

  return (
    <div data-component="text-part">
      <div data-slot="text-part-body">
        <Markdown text={text} streaming={working} />
      </div>
      {showCopy && (
        <div data-slot="text-part-copy-wrapper" data-interrupted={interrupted ? "" : undefined}>
          {meta && <span data-slot="text-part-meta">{meta}</span>}
        </div>
      )}
    </div>
  )
}

function ReasoningPartDisplay({ part }: MessagePartProps) {
  const text = (part.text ?? "").trim()
  if (!text) return null

  return (
    <div data-component="reasoning-part" className="my-3 border-l-2 border-border/40 ml-1.5 pl-4">
      <details className="group">
        <summary className="flex cursor-pointer list-none items-center gap-2 py-1 text-[12px] font-medium text-muted-foreground transition-colors hover:text-foreground [&::-webkit-details-marker]:hidden">
          <svg className="h-3.5 w-3.5 shrink-0 transition-transform group-open:rotate-90" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
            <path strokeLinecap="round" strokeLinejoin="round" d="M9 5l7 7-7 7" />
          </svg>
          <span>Thought</span>
        </summary>
        <div className="whitespace-pre-wrap py-2 pr-2 text-[13px] leading-relaxed text-muted-foreground opacity-90 font-mono">
          {text}
        </div>
      </details>
    </div>
  )
}

function ToolPartDisplay({ part, defaultOpen, hideDetails }: MessagePartProps) {
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

  // Create a ToolCallView from the part data to use with the specific tool components
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
        <RegisteredTool
          tool={toolCall.tool}
          status={toolCall.status}
          input={input}
          output={toolCall.output ?? undefined}
          metadata={partMetadata}
          defaultOpen={defaultOpen}
          hideDetails={hideDetails}
        />
      ) : (
        <FallbackTool toolCall={toolCall} />
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
