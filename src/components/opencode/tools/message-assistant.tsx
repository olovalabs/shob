import { useEffect, useMemo, useState } from "react"
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
  if (part.type === "tool") return true
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

  const nonReasoningParts = useMemo(
    () => filtered.filter((p) => p.type !== "reasoning"),
    [filtered],
  )

  const grouped = useMemo(
    () =>
      groupParts(nonReasoningParts.map((p) => ({ partID: p.id, part: p }))),
    [nonReasoningParts],
  )

  const contextGroupParts = useMemo(() => {
    return nonReasoningParts
      .filter(isContextGroupTool)
      .map((p) => ({
        tool: p.tool ?? "tool",
        status: p.state?.status ?? "pending",
        input: p.state?.input as Record<string, unknown> | undefined,
      }))
  }, [nonReasoningParts])

  const showThinking = working && filtered.length === 0

  const reasoningParts = useMemo(
    () => filtered.filter((p) => p.type === "reasoning"),
    [filtered],
  )

  const interleaved = useMemo(() => {
    if (reasoningParts.length === 0) {
      return nonReasoningParts.map((p) => ({ type: "part" as const, part: p }))
    }

    const allSorted = [...filtered].sort((a, b) => a.id.localeCompare(b.id))
    return allSorted.map((p) => {
      if (p.type === "reasoning") {
        return { type: "reasoning" as const, part: p }
      }
      return { type: "part" as const, part: p }
    })
  }, [filtered, reasoningParts, nonReasoningParts])

  return (
    <div data-component="assistant-message">
      {contextGroupParts.length > 0 && (
        <ContextToolGroup parts={contextGroupParts} busy={working} />
      )}
      {interleaved.map((item) => {
        if (item.type === "reasoning") {
          return (
            <InlineReasoningBlock
              key={item.part.id}
              text={item.part.text ?? ""}
              isStreaming={Boolean(working)}
            />
          )
        }
        const part = nonReasoningParts.find((p) => p.id === item.part.id)
        if (!part) return null
        const group = grouped.find((g) => g.type === "part" && g.ref.partID === part.id)
        if (!group) return null
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

function InlineReasoningBlock({ text, isStreaming }: { text: string; isStreaming: boolean }) {
  const [open, setOpen] = useState(true)

  useEffect(() => {
    if (!isStreaming) {
      setOpen(false)
    }
  }, [isStreaming])

  return (
    <div data-component="reasoning-part" className="mb-1">
      <details open={open} className="group">
        <summary onClick={() => setOpen((o) => !o)} className="flex cursor-pointer list-none items-center gap-2 py-1 text-[12px] text-muted-foreground transition-colors hover:text-foreground [&::-webkit-details-marker]:hidden">
          <svg className="h-3 w-3 shrink-0 transition-transform group-open:rotate-90" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
            <path strokeLinecap="round" strokeLinejoin="round" d="M9 5l7 7-7 7" />
          </svg>
          <span>Thought</span>
        </summary>
        <div className="whitespace-pre-wrap px-1 py-2 text-[12px] leading-relaxed text-muted-foreground">{text}</div>
      </details>
    </div>
  )
}
