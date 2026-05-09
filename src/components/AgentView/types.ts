import type { AgentMessage, AgentMessagePart } from "@/types"

export interface AgentViewProps {
  sessionId: string
  isActive?: boolean
}

export type ToolCallView = {
  id?: string | null
  callID?: string | null
  tool: string
  status: "pending" | "running" | "completed" | "error" | string
  title?: string | null
  input?: unknown
  output?: string | null
  error?: string | null
  raw?: string | null
  metadata?: Record<string, unknown> | null
  attachments?: unknown[] | null
  startedAt?: number | null
  endedAt?: number | null
  compactedAt?: number | null
}

export type OpenCodePartView = {
  id: string
  messageID?: string
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
    raw?: string
    metadata?: Record<string, unknown>
    attachments?: unknown[]
    time?: {
      start?: number
      end?: number
      compacted?: number
    }
  }
}

export type LiveAssistantState = {
  content: string
  toolCalls: ToolCallView[]
  parts: OpenCodePartView[]
  error: unknown | null
  createdAt: number
}

export type OpenCodeMessageInfo = {
  id?: string
  role?: string
  parentID?: string
  title?: string
  error?: unknown
  time?: {
    created?: number
    completed?: number
  }
}

export type OpenCodeSessionInfo = {
  id?: string
  title?: string
  parentID?: string
  time?: {
    created?: number
    updated?: number
  }
}

export type OpenCodeEventPayload = {
  type?: string
  properties?: {
    sessionID?: string
    messageID?: string
    partID?: string
    field?: string
    delta?: string
    part?: OpenCodePartView
    info?: OpenCodeMessageInfo & OpenCodeSessionInfo
    status?: { type?: string; message?: string; attempt?: number; next?: number }
    error?: unknown
  }
}

export type SubagentTracker = {
  localSessionId: string
  opencodeSessionId: string
  partsByMessageID: Map<string, Map<string, OpenCodePartView>>
  currentMessageID: string | null
  lastFlushedAt: number
}

export type AgentMsg = AgentMessage & {
  parts?: AgentMessagePart[] | OpenCodePartView[] | null
}

export type TurnMsg = Omit<AgentMsg, "agent" | "error" | "model" | "parts" | "time" | "toolCalls"> & {
  content: string
  parts: OpenCodePartView[]
  toolCalls?: ToolCallView[]
  agent?: string
  model?: {
    providerID?: string
    modelID?: string
  }
  time?: {
    created?: number
    completed?: number
  }
  error?: {
    name?: string
    data?: { message?: string }
  }
}

export type TodoItem = {
  id: string
  content: string
  status: "pending" | "in_progress" | "completed" | "cancelled"
}
