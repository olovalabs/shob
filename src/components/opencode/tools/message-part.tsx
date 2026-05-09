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

function TextPartDisplay({ part, message, showCopy }: MessagePartProps) {
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
        <Markdown text={text} />
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
    <div data-component="reasoning-part">
      <div className="whitespace-pre-wrap text-[13px] leading-relaxed">{text}</div>
    </div>
  )
}

function ToolPartDisplay({ part, defaultOpen, hideDetails }: MessagePartProps) {
  if (!part.tool) return null
  if (part.tool === "todowrite") return null
  if (part.tool === "question" && part.state?.status && ["pending", "running"].includes(part.state.status)) {
    return null
  }

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
