import { useMemo } from "react"
import type { OpenCodePartView, ToolCallView, TodoItem } from "./types"
import type { AgentMessage } from "@/types"

const getTodosFromPart = (p: OpenCodePartView): TodoItem[] => {
  type RawTodo = { id: string; content: string; status: string }
  const fromInput = (p.state?.input as { todos?: RawTodo[] } | null)?.todos
  const fromMeta  = (p.state?.metadata as { todos?: RawTodo[] } | null)?.todos
  const raw = fromInput ?? fromMeta ?? []
  return raw.map((t) => ({ id: t.id, content: t.content, status: t.status as TodoItem["status"] }))
}

const extractTodosFromParts = (parts: OpenCodePartView[]): TodoItem[] | null => {
  const todoParts = parts.filter((p) => p.type === "tool" && p.tool === "todowrite")
  if (todoParts.length === 0) return null
  const last = todoParts[todoParts.length - 1]
  return getTodosFromPart(last)
}

const extractTodosFromToolCalls = (toolCalls: ToolCallView[]): TodoItem[] | null => {
  const todoCalls = toolCalls.filter((tc) => tc.tool === "todowrite")
  if (todoCalls.length === 0) return null
  const last = todoCalls[todoCalls.length - 1]
  type RawTodo = { id: string; content: string; status: string }
  const fromInput = (last.input as { todos?: RawTodo[] } | null)?.todos
  const fromMeta  = (last.metadata as { todos?: RawTodo[] } | null)?.todos
  const raw = fromInput ?? fromMeta ?? []
  return raw.map((t) => ({ id: t.id, content: t.content, status: t.status as TodoItem["status"] }))
}

interface UseTodosParams {
  isThinking: boolean
  liveAssistant: {
    parts?: OpenCodePartView[]
    toolCalls?: ToolCallView[]
  } | null
  messages: AgentMessage[]
}

export const useTodos = ({ isThinking, liveAssistant, messages }: UseTodosParams): TodoItem[] => {
  return useMemo(() => {
    if (isThinking) {
      if (liveAssistant?.parts?.length) {
        const todos = extractTodosFromParts(liveAssistant.parts)
        if (todos !== null) return todos
      }
      if (liveAssistant?.toolCalls?.length) {
        const todos = extractTodosFromToolCalls(liveAssistant.toolCalls)
        if (todos !== null) return todos
      }
    }

    for (let i = messages.length - 1; i >= 0; i--) {
      const msg = messages[i]
      if (msg.role !== "assistant") continue
      if (msg.parts?.length) {
        const todos = extractTodosFromParts(msg.parts as OpenCodePartView[])
        if (todos !== null) return todos
      }
      if (msg.toolCalls?.length) {
        const todos = extractTodosFromToolCalls(msg.toolCalls)
        if (todos !== null) return todos
      }
    }

    return []
  }, [isThinking, liveAssistant?.parts, liveAssistant?.toolCalls, messages])
}
