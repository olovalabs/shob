import { useMemo } from "react"
import { Part } from "./message-part"
import { ContextToolGroup } from "./context-tool-group"
import { TextShimmer } from "./text-shimmer"

const CONTEXT_GROUP_TOOLS = new Set(["read", "glob", "grep", "list"])

interface PartData {
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

interface AssistantMessageProps {
  parts: PartData[]
  messageId: string
  working?: boolean
  showReasoningSummaries?: boolean
}

function isContextGroupTool(part: PartData): boolean {
  return part.type === "tool" && !!part.tool && CONTEXT_GROUP_TOOLS.has(part.tool)
}

function renderable(part: PartData, showReasoning: boolean): boolean {
  if (part.type === "tool") {
    if (part.tool === "todowrite") return false
    if (part.tool === "question" && part.state?.status && ["pending", "running"].includes(part.state.status)) return false
    return true
  }
  if (part.type === "text") return !!part.text?.trim()
  if (part.type === "reasoning") return showReasoning && !!part.text?.trim()
  return false
}

interface PartRef {
  messageID: string
  partID: string
}

type PartGroup =
  | { key: string; type: "part"; ref: PartRef }
  | { key: string; type: "context"; refs: PartRef[] }

function groupParts(parts: { partID: string; part: PartData }[]): PartGroup[] {
  const result: PartGroup[] = []
  let start = -1

  const flush = (end: number) => {
    if (start < 0) return
    const slice = parts.slice(start, end + 1)
    result.push({
      key: `context:${slice[0]?.partID ?? ""}`,
      type: "context",
      refs: slice.map((item) => ({ messageID: "", partID: item.partID })),
    })
    start = -1
  }

  parts.forEach((item, index) => {
    if (isContextGroupTool(item.part)) {
      if (start < 0) start = index
      return
    }
    flush(index - 1)
    result.push({
      key: `part:${item.partID}`,
      type: "part",
      ref: { messageID: "", partID: item.partID },
    })
  })

  flush(parts.length - 1)
  return result
}

export function AssistantMessageDisplay({
  parts,
  messageId,
  working,
  showReasoningSummaries = true,
}: AssistantMessageProps) {
  const filtered = useMemo(
    () => parts.filter((p) => renderable(p, showReasoningSummaries)),
    [parts, showReasoningSummaries],
  )

  const grouped = useMemo(
    () =>
      groupParts(filtered.map((p) => ({ partID: p.id, part: p }))),
    [filtered],
  )

  const contextGroupParts = useMemo(() => {
    return filtered
      .filter(isContextGroupTool)
      .map((p) => ({
        tool: p.tool ?? "tool",
        status: p.state?.status ?? "pending",
        input: p.state?.input as Record<string, unknown> | undefined,
      }))
  }, [filtered])

  const showThinking = working && filtered.length === 0

  return (
    <div data-component="assistant-message">
      {contextGroupParts.length > 0 && (
        <ContextToolGroup parts={contextGroupParts} busy={working} />
      )}
      {grouped
        .filter((g) => g.type === "part")
        .map((g) => {
          if (g.type !== "part") return null
          const part = filtered.find((p) => p.id === g.ref.partID)
          if (!part) return null
          return (
            <Part
              key={part.id}
              part={part}
              message={{ id: messageId, role: "assistant" }}
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
