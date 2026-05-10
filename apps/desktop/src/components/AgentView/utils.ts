import type { AgentMessagePart } from "@/types"
import type {
  OpenCodePartView,
  ToolCallView,
  AgentMsg,
  TurnMsg,
} from "./types"

export const getDirectoryParts = (path: string | null | undefined) => {
  if (!path) return { parent: "", name: "" }
  const normalized = path.replace(/[\\/]+$/, "")
  const parts = normalized.split(/[\\/]/)
  const name = parts[parts.length - 1] ?? normalized
  const parent = parts.slice(0, -1).join("/")
  return {
    parent: parent ? `${parent}/` : "",
    name: name || normalized,
  }
}

export const formatRelativeTime = (ts: number) => {
  const diff = Math.max(0, Date.now() - ts)
  const mins = Math.floor(diff / 60000)
  if (mins < 1) return "just now"
  if (mins < 60) return `${mins}m ago`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  return `${days}d ago`
}

export const describeOpenCodeError = (error: unknown) => {
  if (error instanceof Error) return error.message
  if (typeof error === "string") return error
  if (error && typeof error === "object") {
    const data = error as { data?: { message?: unknown }; message?: unknown }
    const nested = typeof data.data?.message === "string"
      ? data.data.message
      : typeof data.message === "string"
        ? data.message
        : undefined
    return nested ?? JSON.stringify(data)
  }
  return "OpenCode returned an unknown error"
}

const OPEN_CODE_DEFAULT_TITLE_RE =
  /^(New session|Child session) - \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/
const LOCAL_AGENT_PLACEHOLDER_TITLE_RE = /^Agent \d+$/

export const isOpenCodeDefaultTitle = (title: string | null | undefined) =>
  Boolean(title && OPEN_CODE_DEFAULT_TITLE_RE.test(title.trim()))

export const isLocalAgentPlaceholderTitle = (title: string | null | undefined) =>
  Boolean(title && LOCAL_AGENT_PLACEHOLDER_TITLE_RE.test(title.trim()))

export const shouldAdoptOpenCodeTitle = (nextTitle: string | null | undefined, currentTitle: string | null | undefined) => {
  const next = nextTitle?.trim()
  const current = currentTitle?.trim()
  if (!next || next === current || isOpenCodeDefaultTitle(next)) return false
  return !current || isLocalAgentPlaceholderTitle(current) || isOpenCodeDefaultTitle(current)
}

export const getOpenCodeCreateTitle = (sessionName: string | undefined, existingSessionID?: string | null) => {
  if (existingSessionID) return undefined
  return isLocalAgentPlaceholderTitle(sessionName) ? undefined : sessionName
}

export const extractTextFromParts = (parts: OpenCodePartView[]) =>
  parts
    .filter((part) => part.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("\n")
    .trim()

export const extractToolCallsFromParts = (parts: OpenCodePartView[]): ToolCallView[] =>
  parts
    .filter((part) => part.type === "tool")
    .map((part) => ({
      id: part.id ?? null,
      callID: part.callID ?? null,
      tool: part.tool ?? "tool",
      status: part.state?.status ?? "pending",
      title: typeof part.state?.title === "string" ? part.state.title : null,
      input: part.state?.input ?? null,
      output: typeof part.state?.output === "string" ? part.state.output : null,
      error: typeof part.state?.error === "string" ? part.state.error : null,
      raw: typeof part.state?.raw === "string" ? part.state.raw : null,
      metadata: part.state?.metadata ?? null,
      attachments: Array.isArray(part.state?.attachments) ? part.state.attachments : null,
      startedAt: typeof part.state?.time?.start === "number" ? part.state.time.start : null,
      endedAt: typeof part.state?.time?.end === "number" ? part.state.time.end : null,
      compactedAt: typeof part.state?.time?.compacted === "number" ? part.state.time.compacted : null,
    }))

export const sortOpenCodeParts = (parts: OpenCodePartView[]) =>
  [...parts].sort((left, right) => left.id.localeCompare(right.id))

export const normalizePartForView = (part: OpenCodePartView | AgentMessagePart): OpenCodePartView => ({
  id: part.id,
  messageID: part.messageID,
  type: part.type,
  text: part.text,
  tool: part.tool,
  callID: part.callID,
  state: part.state
    ? {
        status: part.state.status,
        title: part.state.title,
        input: part.state.input,
        output: part.state.output,
        error: part.state.error,
        raw: part.state.raw,
        metadata: part.state.metadata,
        attachments: part.state.attachments,
        time: part.state.time,
      }
    : undefined,
})

export const normalizeRawPartsForView = (parts: unknown, fallbackMessageID?: string | null) => {
  if (!Array.isArray(parts)) return []
  return parts
    .filter((part): part is Record<string, unknown> => Boolean(part && typeof part === "object"))
    .map((part, index) => normalizePartForView({
      ...(part as OpenCodePartView),
      id: typeof part.id === "string" ? part.id : `${fallbackMessageID ?? "message"}-part-${index}`,
      messageID: typeof part.messageID === "string" ? part.messageID : fallbackMessageID ?? undefined,
    }))
}

export const toolCallToPart = (toolCall: ToolCallView, index: number, messageID: string): OpenCodePartView => ({
  id: toolCall.id ?? toolCall.callID ?? `${messageID}-tool-${index}`,
  messageID,
  type: "tool",
  tool: toolCall.tool,
  callID: toolCall.callID ?? undefined,
  state: {
    status: toolCall.status,
    title: toolCall.title ?? undefined,
    input: toolCall.input ?? undefined,
    output: toolCall.output ?? undefined,
    error: toolCall.error ?? undefined,
    raw: toolCall.raw ?? undefined,
    metadata: toolCall.metadata ?? undefined,
    attachments: toolCall.attachments ?? undefined,
    time: {
      start: toolCall.startedAt ?? undefined,
      end: toolCall.endedAt ?? undefined,
      compacted: toolCall.compactedAt ?? undefined,
    },
  },
})

export const buildAssistantParts = (messageID: string, content: string, toolCalls: ToolCallView[]) => {
  const parts = toolCalls.map((toolCall, index) => toolCallToPart(toolCall, index, messageID))
  if (content.trim()) {
    parts.push({
      id: `${messageID}-text`,
      messageID,
      type: "text",
      text: content,
    })
  }
  return parts
}

const OPEN_CODE_ID_LENGTH = 26
let lastOpenCodeIDTimestamp = 0
let openCodeIDCounter = 0

const randomBase62 = (length: number) => {
  const chars = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
  const bytes = new Uint8Array(length)
  crypto.getRandomValues(bytes)
  return Array.from(bytes, (byte) => chars[byte % chars.length]).join("")
}

export const createOpenCodeID = (type: "message" | "part") => {
  const prefix = type === "message" ? "msg" : "prt"
  const timestamp = Date.now()
  if (timestamp !== lastOpenCodeIDTimestamp) {
    lastOpenCodeIDTimestamp = timestamp
    openCodeIDCounter = 0
  }

  openCodeIDCounter += 1
  const value = BigInt(timestamp) * BigInt(0x1000) + BigInt(openCodeIDCounter)
  const bytes = new Uint8Array(6)
  for (let i = 0; i < 6; i += 1) {
    bytes[i] = Number((value >> BigInt(40 - 8 * i)) & BigInt(0xff))
  }
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")
  return `${prefix}_${hex}${randomBase62(OPEN_CODE_ID_LENGTH - 12)}`
}

export const getAssistantParts = (msg: AgentMsg) => {
  if (Array.isArray(msg.parts) && msg.parts.length > 0) {
    return msg.parts.map((part: AgentMessagePart | OpenCodePartView) => normalizePartForView(part))
  }
  return buildAssistantParts(msg.id, msg.content ?? "", msg.toolCalls ?? [])
}

export const getUserParts = (msg: AgentMsg) =>
  Array.isArray(msg.parts) ? msg.parts.map((part: AgentMessagePart | OpenCodePartView) => normalizePartForView(part)) : []

export const toTurnMessage = (msg: AgentMsg): TurnMsg => ({
  ...msg,
  content: msg.content ?? "",
  toolCalls: msg.toolCalls ?? undefined,
  agent: msg.agent ?? undefined,
  model: msg.model ?? undefined,
  error: msg.error ?? undefined,
  parts: msg.role === "assistant" ? getAssistantParts(msg) : getUserParts(msg),
  time: msg.time ?? {
    created: msg.createdAt,
    completed: msg.role === "assistant" ? msg.createdAt : undefined,
  },
})

export const convertToSessionFormat = (msgs: AgentMsg[]) => {
  const groups: Array<{ userIndex: number; userMessage: AgentMsg; assistantMessages: AgentMsg[] }> = []
  let cu: { index: number; message: AgentMsg } | null = null
  const assistants: AgentMsg[] = []

  for (let i = 0; i < msgs.length; i++) {
    const msg = msgs[i]
    if (msg?.role === "user") {
      if (cu && assistants.length > 0) {
        groups.push({
          userIndex: cu.index,
          userMessage: cu.message,
          assistantMessages: [...assistants],
        })
        assistants.length = 0
      }
      cu = { index: i, message: msg }
    } else if (msg?.role === "assistant" && cu) {
      assistants.push(msg)
    }
  }

  if (cu) {
    groups.push({
      userIndex: cu.index,
      userMessage: cu.message,
      assistantMessages: [...assistants],
    })
  }

  return groups
}
