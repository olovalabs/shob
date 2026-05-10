import { useMemo } from "react"
import { Part } from "./message-part"
import { ContextToolGroup } from "./context-tool-group"
import { TextShimmer } from "./text-shimmer"
import {
  Reasoning,
  ReasoningTrigger,
  ReasoningContent,
} from "@/components/ai-elements/reasoning"
import { MessageResponse } from "@/components/ai-elements/message"

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

/**
 * Build an interleaved sequence that preserves the original part order but:
 * - coalesces consecutive reasoning parts into one block
 * - keeps tool parts and text parts in their original positions
 */
type InterleavedItem =
  | { type: "reasoning"; text: string; id: string; isLast: boolean }
  | { type: "part"; part: PartData; id: string }

function buildInterleaved(filtered: PartData[]): InterleavedItem[] {
  const coalesced: InterleavedItem[] = []

  for (const p of filtered) {
    if (p.type === "reasoning") {
      const last = coalesced[coalesced.length - 1]
      if (last && last.type === "reasoning") {
        last.text = last.text + "\n" + (p.text ?? "")
      } else {
        coalesced.push({ type: "reasoning", text: p.text ?? "", id: p.id, isLast: false })
      }
    } else {
      coalesced.push({ type: "part", part: p, id: p.id })
    }
  }

  // Mark whether a reasoning block is the last item (for "streaming" state)
  for (let i = 0; i < coalesced.length; i++) {
    const item = coalesced[i]
    if (item.type === "reasoning") {
      // isLast only matters so reasoning knows if something comes after it;
      // the Reasoning component handles auto-collapse itself via `isStreaming`
    }
  }

  return coalesced
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

  // Build the interleaved flow preserving original part order
  const interleaved = useMemo(() => buildInterleaved(filtered), [filtered])

  // Figure out if the last item in the stream is a reasoning block
  // so we know if reasoning is still "streaming" (nothing after it yet)
  const lastItemIsReasoning = interleaved.length > 0 && interleaved[interleaved.length - 1].type === "reasoning"

  return (
    <div data-component="assistant-message" className="w-full space-y-2 py-2 px-4 md:px-5">
      {/* Context tool group (reading files, grep, etc.) */}
      {contextGroupParts.length > 0 && (
        <ContextToolGroup parts={contextGroupParts} busy={working} />
      )}

      {/* Interleaved stream: reasoning → tool calls → text, in original order */}
      {interleaved.map((item, idx) => {
        if (item.type === "reasoning") {
          // Reasoning is "streaming" only if it's the last item AND we're still working
          const isStreamingBlock = working && idx === interleaved.length - 1
          return (
            <Reasoning
              key={item.id}
              isStreaming={isStreamingBlock}
              className="not-prose"
            >
              <ReasoningTrigger />
              <ReasoningContent>{item.text}</ReasoningContent>
            </Reasoning>
          )
        }

        // Tool or text part — rendered via Part which uses MessageResponse for text
        const part = filtered.find((p) => p.id === item.part.id)
        if (!part) return null

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
