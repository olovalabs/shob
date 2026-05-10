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
      return nonReasoningParts.map((p) => ({ type: "part" as const, part: p, id: p.id }))
    }

    const allSorted = [...filtered].sort((a, b) => a.id.localeCompare(b.id))
    const coalesced: Array<{ type: "part" | "reasoning", part?: PartData, text?: string, id: string }> = []
    
    for (const p of allSorted) {
      if (p.type === "reasoning") {
        const last = coalesced[coalesced.length - 1]
        if (last && last.type === "reasoning") {
          last.text = last.text + "\n" + (p.text ?? "")
        } else {
          coalesced.push({ type: "reasoning", text: p.text ?? "", id: p.id })
        }
      } else {
        coalesced.push({ type: "part", part: p, id: p.id })
      }
    }
    return coalesced
  }, [filtered, reasoningParts, nonReasoningParts])

  return (
    <div data-component="assistant-message" className="space-y-3">
      {contextGroupParts.length > 0 && (
        <ContextToolGroup parts={contextGroupParts} busy={working} />
      )}
      {interleaved.map((item) => {
        if (item.type === "reasoning") {
          return (
            <InlineReasoningBlock
              key={item.id}
              text={item.text ?? ""}
              isStreaming={Boolean(working)}
            />
          )
        }
        if (!item.part) return null
        const part = nonReasoningParts.find((p) => p.id === item.part!.id)
        if (!part) return null
        const group = grouped.find((g) => g.type === "part" && g.ref.partID === part.id)
        if (!group) return null
        return (
          <Part
            key={part.id}
            part={part}
            message={{ id: messageId, role: "assistant" }}
            working={working}
          />
        )
      })}
      {showThinking && (
        <div data-slot="session-turn-thinking" className="mt-2">
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
    <div data-component="reasoning-part" className="my-3 border-l-2 border-border/40 ml-1.5 pl-4">
      <details open={open} className="group">
        <summary 
          onClick={(e) => { e.preventDefault(); setOpen((o) => !o) }} 
          className="flex cursor-pointer list-none items-center gap-2 py-1 text-[12px] font-medium text-muted-foreground transition-colors hover:text-foreground [&::-webkit-details-marker]:hidden"
        >
          <svg className="h-3.5 w-3.5 shrink-0 transition-transform group-open:rotate-90" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth={2}>
            <path strokeLinecap="round" strokeLinejoin="round" d="M9 5l7 7-7 7" />
          </svg>
          <span>{isStreaming ? "Thinking..." : "Thought"}</span>
        </summary>
        <div className="whitespace-pre-wrap py-2 pr-2 text-[13px] leading-relaxed text-muted-foreground opacity-90 font-mono">
          {text}
        </div>
      </details>
    </div>
  )
}
