import { ToolErrorCard } from "./tool-error-card"
import { FallbackTool } from "./fallback-tool"
import { ToolRegistry } from "./tool-registry"
import type { ToolCallView } from "@/components/AgentView"
import {
  MessageResponse,
} from "@/components/ai-elements/message"


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

  return (
    <div data-component="text-part">
      <MessageResponse isAnimating={working}>{text}</MessageResponse>
    </div>
  )
}

function ReasoningPartDisplay({ part, working }: MessagePartProps) {
  const text = part.text ?? ""
  if (!text.trim()) return null

  return (
    <div className="my-2 text-sm text-muted-foreground">
      {text}
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
