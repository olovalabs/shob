import { useMemo } from "react"
import { Part } from "./message-part"
import { ContextToolGroup } from "./context-tool-group"
import { TextShimmer } from "./text-shimmer"

const CONTEXT_GROUP_TOOLS = new Set(["read", "glob", "grep", "list"])
const HIDDEN_TOOLS = new Set(["todowrite"])

interface PartData {
  id: string
  type: string
  text?: string
  tool?: string
  callID?: string
  messageID?: string
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

type PartGroup =
  | { key: string; type: "part"; part: PartData }
  | { key: string; type: "context"; parts: PartData[] }

function groupParts(parts: PartData[]): PartGroup[] {
  const result: PartGroup[] = []
  let start = -1

  const flush = (end: number) => {
    if (start < 0) return
    const first = parts[start]
    const last = parts[end]
    if (!first || !last) { start = -1; return }
    result.push({
      key: `context:${first.id}`,
      type: "context",
      parts: parts.slice(start, end + 1),
    })
    start = -1
  }

  parts.forEach((part, index) => {
    if (part.type === "tool" && CONTEXT_GROUP_TOOLS.has(part.tool ?? "")) {
      if (start < 0) start = index
      return
    }
    flush(index - 1)
    result.push({
      key: `part:${part.messageID ?? ""}:${part.id}`,
      type: "part",
      part,
    })
  })

  flush(parts.length - 1)
  return result
}

function isRenderable(part: PartData, showReasoning: boolean): boolean {
  if (part.type === "tool") {
    if (HIDDEN_TOOLS.has(part.tool ?? "")) return false
    if (part.tool === "question" && (part.state?.status === "pending" || part.state?.status === "running")) return false
    return true
  }
  if (part.type === "text") return !!part.text?.trim()
  if (part.type === "reasoning") return showReasoning && !!part.text?.trim()
  return false
}

interface AssistantMessageProps {
  parts: PartData[]
  messageId: string
  working?: boolean
  showReasoningSummaries?: boolean
}

export function AssistantMessageDisplay({
  parts,
  messageId,
  working,
  showReasoningSummaries = true,
}: AssistantMessageProps) {
  const filtered = useMemo(
    () => parts.filter((p) => isRenderable(p, showReasoningSummaries)),
    [parts, showReasoningSummaries],
  )

  const grouped = useMemo(
    () => groupParts(filtered),
    [filtered],
  )

  const lastKey = useMemo(() => grouped.at(-1)?.key, [grouped])

  const showThinking = working && grouped.length === 0

  return (
    <div data-component="assistant-message">
      {grouped.map((item) => {
        if (item.type === "context") {
          return (
            <ContextToolGroup
              key={item.key}
              parts={item.parts}
              busy={working && item.key === lastKey}
            />
          )
        }

        return (
          <Part
            key={item.key}
            part={item.part}
            message={{ id: messageId, role: "assistant" }}
            working={working && item.key === lastKey}
          />
        )
      })}

      {showThinking && (
        <div data-slot="session-turn-thinking">
          <TextShimmer text="Thinking" />
        </div>
      )}
    </div>
  )
}
