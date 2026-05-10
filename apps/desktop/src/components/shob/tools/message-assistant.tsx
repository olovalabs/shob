import { useMemo } from "react"
import { Part } from "./message-part"
import { ContextToolGroup } from "./context-tool-group"
import { TextShimmer } from "./text-shimmer"
import {
  Reasoning,
  ReasoningTrigger,
  ReasoningContent,
} from "@/components/ai-elements/reasoning"
import { Message } from "@/components/ai-elements/message"

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

function renderable(part: PartData, showReasoning: boolean): boolean {
  if (part.type === "tool") return true
  if (part.type === "text") return !!part.text?.trim()
  if (part.type === "reasoning") return showReasoning && !!part.text?.trim()
  return false
}

const SIMPLE_TOOLS = new Set(["read", "glob", "grep", "list"])

function isSimpleTool(part: PartData): boolean {
  return part.type === "tool" && !!part.tool && SIMPLE_TOOLS.has(part.tool)
}

type InterleavedItem =
  | { type: "reasoning"; id: string; text: string }
  | { type: "context"; id: string; parts: PartData[] }
  | { type: "part"; id: string; part: PartData }

function buildInterleaved(parts: PartData[]): InterleavedItem[] {
  const items: InterleavedItem[] = []

  for (const part of parts) {
    const last = items[items.length - 1]

    if (part.type === "reasoning") {
      const text = part.text ?? ""
      if (last?.type === "reasoning") {
        last.text = `${last.text}\n${text}`
      } else {
        items.push({ type: "reasoning", id: part.id, text })
      }
      continue
    }

    if (isSimpleTool(part)) {
      if (last?.type === "context") {
        last.parts.push(part)
      } else {
        items.push({ type: "context", id: part.id, parts: [part] })
      }
      continue
    }

    items.push({ type: "part", id: part.id, part })
  }

  return items
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

  const interleaved = useMemo(() => buildInterleaved(filtered), [filtered])
  const showThinking = working && interleaved.length === 0

  return (
    <Message
      from="assistant"
      data-component="assistant-message"
      data-streaming={working ? "true" : undefined}
      className="w-full max-w-full py-2 px-4 md:px-5"
    >
      {interleaved.map((item, idx) => {
        if (item.type === "reasoning") {
          return (
            <Reasoning
              key={item.id}
              isStreaming={Boolean(working && idx === interleaved.length - 1)}
              className="not-prose my-1"
            >
              <ReasoningTrigger />
              <ReasoningContent>{item.text}</ReasoningContent>
            </Reasoning>
          )
        }

        if (item.type === "context") {
          return (
            <ContextToolGroup
              key={item.id}
              parts={item.parts}
              busy={working && idx === interleaved.length - 1}
            />
          )
        }

        return (
          <Part
            key={item.id}
            part={item.part}
            message={{ id: messageId, role: "assistant" }}
            working={working && idx === interleaved.length - 1}
          />
        )
      })}

      {showThinking && (
        <div data-slot="session-turn-thinking">
          <TextShimmer text="Thinking" />
        </div>
      )}
    </Message>
  )
}
