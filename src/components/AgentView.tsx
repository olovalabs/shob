import { memo, useEffect, useMemo, useRef, useState } from "react"
import {
  ArrowUp,
  ChevronDown,
  GitBranch,
  Plus,
  Sparkles,
  StopCircle,
} from "lucide-react"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"
import { nativeApi } from "@/services/native"
import {
  buildConnectedOpenCodeModelOptions,
  parseOpenCodeModelValue,
  pickOpenCodeModel,
  type OpenCodeModelOption,
} from "@/utils/opencode-models"
import type {
  ElectronOpencodeEventEnvelope,
  ElectronOpencodeEventSubscription,
} from "../electron"
import { SessionTurn } from "@/components/opencode/tools"
import type { AgentMessage, AgentMessagePart } from "@/types"


interface AgentViewProps {
  sessionId: string
  isActive?: boolean
}

const SUGGESTED_PROMPTS = [
  "Explain the structure of this project",
  "Find bugs in the recent changes",
  "Refactor the active file for clarity",
  "Write unit tests for the current module",
] as const

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

type OpenCodePartView = {
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

type LiveAssistantState = {
  content: string
  toolCalls: ToolCallView[]
  parts: OpenCodePartView[]
  error: unknown | null
  createdAt: number
}

type OpenCodeMessageInfo = {
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

type OpenCodeSessionInfo = {
  id?: string
  title?: string
  parentID?: string
  time?: {
    created?: number
    updated?: number
  }
}

type OpenCodeEventPayload = {
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

const getDirectoryParts = (path: string | null | undefined) => {
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

const formatRelativeTime = (ts: number) => {
  const diff = Math.max(0, Date.now() - ts)
  const mins = Math.floor(diff / 60000)
  if (mins < 1) return "just now"
  if (mins < 60) return `${mins}m ago`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  return `${days}d ago`
}

const describeOpenCodeError = (error: unknown) => {
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

const isOpenCodeDefaultTitle = (title: string | null | undefined) =>
  Boolean(title && OPEN_CODE_DEFAULT_TITLE_RE.test(title.trim()))

const isLocalAgentPlaceholderTitle = (title: string | null | undefined) =>
  Boolean(title && LOCAL_AGENT_PLACEHOLDER_TITLE_RE.test(title.trim()))

const shouldAdoptOpenCodeTitle = (nextTitle: string | null | undefined, currentTitle: string | null | undefined) => {
  const next = nextTitle?.trim()
  const current = currentTitle?.trim()
  if (!next || next === current || isOpenCodeDefaultTitle(next)) return false
  return !current || isLocalAgentPlaceholderTitle(current) || isOpenCodeDefaultTitle(current)
}

const getOpenCodeCreateTitle = (sessionName: string | undefined, existingSessionID?: string | null) => {
  if (existingSessionID) return undefined
  return isLocalAgentPlaceholderTitle(sessionName) ? undefined : sessionName
}

const extractTextFromParts = (parts: OpenCodePartView[]) =>
  parts
    .filter((part) => part.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("\n")
    .trim()

const extractToolCallsFromParts = (parts: OpenCodePartView[]): ToolCallView[] =>
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

const sortOpenCodeParts = (parts: OpenCodePartView[]) =>
  [...parts].sort((left, right) => left.id.localeCompare(right.id))

const normalizePartForView = (part: OpenCodePartView | AgentMessagePart): OpenCodePartView => ({
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

const normalizeRawPartsForView = (parts: unknown, fallbackMessageID?: string | null) => {
  if (!Array.isArray(parts)) return []
  return parts
    .filter((part): part is Record<string, unknown> => Boolean(part && typeof part === "object"))
    .map((part, index) => normalizePartForView({
      ...(part as OpenCodePartView),
      id: typeof part.id === "string" ? part.id : `${fallbackMessageID ?? "message"}-part-${index}`,
      messageID: typeof part.messageID === "string" ? part.messageID : fallbackMessageID ?? undefined,
    }))
}

const toolCallToPart = (toolCall: ToolCallView, index: number, messageID: string): OpenCodePartView => ({
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

const buildAssistantParts = (messageID: string, content: string, toolCalls: ToolCallView[]) => {
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

const createOpenCodeID = (type: "message" | "part") => {
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

type AgentMsg = AgentMessage & {
  parts?: AgentMessagePart[] | OpenCodePartView[] | null
}

type TurnMsg = Omit<AgentMsg, "agent" | "error" | "model" | "parts" | "time" | "toolCalls"> & {
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

const getAssistantParts = (msg: AgentMsg) => {
  if (Array.isArray(msg.parts) && msg.parts.length > 0) {
    return msg.parts.map((part) => normalizePartForView(part))
  }
  return buildAssistantParts(msg.id, msg.content ?? "", msg.toolCalls ?? [])
}

const getUserParts = (msg: AgentMsg) =>
  Array.isArray(msg.parts) ? msg.parts.map((part) => normalizePartForView(part)) : []

const toTurnMessage = (msg: AgentMsg): TurnMsg => ({
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

const convertToSessionFormat = (msgs: AgentMsg[]) => {
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


function MessageGroupRenderer({
  messages: msgs,
  isThinking,
  liveAssistant,
}: {
  messages: AgentMsg[]
  isThinking: boolean
  liveAssistant: LiveAssistantState | null
}) {
  const groups = useMemo(() => convertToSessionFormat(msgs), [msgs])

  return (
    <>
      {groups.map((group, index) => {
        const isLast = index === groups.length - 1
        const assistantMessages = group.assistantMessages.map(toTurnMessage)

        if (isLast && isThinking && liveAssistant) {
          const liveID = "live-assistant"
          assistantMessages.push(toTurnMessage({
            id: liveID,
            role: "assistant",
            content: liveAssistant.content,
            createdAt: liveAssistant.createdAt,
            toolCalls: liveAssistant.toolCalls,
            parts: liveAssistant.parts.length > 0
              ? liveAssistant.parts
              : buildAssistantParts(liveID, liveAssistant.content, liveAssistant.toolCalls),
            error: liveAssistant.error
              ? { name: "OpenCodeError", data: { message: describeOpenCodeError(liveAssistant.error) } }
              : null,
          }))
        }

        return (
          <SessionTurn
            key={group.userMessage.id}
            messages={[toTurnMessage(group.userMessage), ...assistantMessages]}
            userMessageIndex={0}
            assistantMessages={assistantMessages}
            working={isThinking && isLast}
            error={isThinking && isLast && liveAssistant?.error ? describeOpenCodeError(liveAssistant.error) : null}
          />
        )
      })}
    </>
  )
}

function AgentViewComponent({ sessionId, isActive = true }: AgentViewProps) {
  const project = useStore((state) => {
    for (const p of state.projects) {
      const s = p.sessions.find((item) => item.id === sessionId)
      if (s) return p
    }
    return null
  })

  const session = useStore((state) => {
    for (const p of state.projects) {
      const s = p.sessions.find((item) => item.id === sessionId)
      if (s) return s
    }
    return null
  })

  const appendAgentMessage = useStore((state) => state.appendAgentMessage)
  const updateSession = useStore((state) => state.updateSession)
  const preferredOpencodeProviderId = useStore((state) => state.preferredOpencodeProviderId)
  const preferredOpencodeModelId = useStore((state) => state.preferredOpencodeModelId)
  const preferredOpencodeVariant = useStore((state) => state.preferredOpencodeVariant)
  const setPreferredOpencodeModel = useStore((state) => state.setPreferredOpencodeModel)
  const setPreferredOpencodeVariant = useStore((state) => state.setPreferredOpencodeVariant)

  const [input, setInput] = useState("")
  const [isThinking, setIsThinking] = useState(false)
  const [composerMode, setComposerMode] = useState<"build" | "plan">("build")
  const [selectedModel, setSelectedModel] = useState("")
  const [modelPower, setModelPower] = useState(preferredOpencodeVariant || "high")
  const [modelOptions, setModelOptions] = useState<OpenCodeModelOption[]>([])
  const [providerStatus, setProviderStatus] = useState<"idle" | "loading" | "ready" | "error">("idle")
  const [attachedFiles, setAttachedFiles] = useState<File[]>([])
  const [liveAssistant, setLiveAssistant] = useState<LiveAssistantState | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const activePromptRef = useRef<string | null>(null)
  const opencodeEventHandlersRef = useRef(new Set<(payload: ElectronOpencodeEventEnvelope) => void>())
  const opencodeEventSubscriptionRef = useRef<ElectronOpencodeEventSubscription | null>(null)

  type SubagentTracker = {
    localSessionId: string
    opencodeSessionId: string
    partsByMessageID: Map<string, Map<string, OpenCodePartView>>
    currentMessageID: string | null
    lastFlushedAt: number
  }
  const subagentTrackersRef = useRef<Map<string, SubagentTracker>>(new Map())

  const messages = useMemo(() => session?.agentMessages ?? [], [session?.agentMessages])
  const projectPathParts = useMemo(() => getDirectoryParts(project?.path), [project?.path])
  const lastUpdatedLabel = useMemo(() => {
    const ts = session?.lastActiveAt ?? session?.createdAt ?? null
    return ts ? formatRelativeTime(ts) : null
  }, [session?.lastActiveAt, session?.createdAt])

  useEffect(() => {
    if (!isActive) return
    textareaRef.current?.focus()
  }, [isActive, sessionId])

  useEffect(() => {
    if (!project) return
    for (const s of project.sessions) {
      if (s.kind === "agent" && s.opencodeSessionId && s.parentSessionId) {
        if (!subagentTrackersRef.current.has(s.opencodeSessionId)) {
          subagentTrackersRef.current.set(s.opencodeSessionId, {
            localSessionId: s.id,
            opencodeSessionId: s.opencodeSessionId,
            partsByMessageID: new Map(),
            currentMessageID: null,
            lastFlushedAt: 0,
          })
        }
      }
    }
  }, [project?.id, project?.sessions.length])

  useEffect(() => {
    const el = scrollRef.current
    if (!el) return
    el.scrollTop = el.scrollHeight
  }, [messages.length, isThinking, liveAssistant?.content, liveAssistant?.toolCalls.length])

  useEffect(() => {
    return () => {
      activePromptRef.current = null
    }
  }, [sessionId])

  useEffect(() => {
    const openSubagentSession = async (opencodeSessionID: string, label?: string, autoLoad = false) => {
      const state = useStore.getState()
      const currentProject = state.projects.find((item) => item.id === project?.id)
      if (!currentProject) return

      const existing = currentProject.sessions.find((item) => item.kind === "agent" && item.opencodeSessionId === opencodeSessionID)
      if (existing) {
        if (!subagentTrackersRef.current.has(opencodeSessionID)) {
          subagentTrackersRef.current.set(opencodeSessionID, {
            localSessionId: existing.id,
            opencodeSessionId: opencodeSessionID,
            partsByMessageID: new Map(),
            currentMessageID: null,
            lastFlushedAt: 0,
          })
        }
        state.setActiveSession(existing.id)
        return
      }

      let title = label?.trim()
      let parentID: string | null = null
      try {
        const info = await nativeApi.invoke("opencode_session_get", {
          directory: currentProject.path,
          sessionID: opencodeSessionID,
        })
        if (info.title && !isOpenCodeDefaultTitle(info.title)) {
          title = info.title.trim()
        }
        parentID = typeof info.parentID === "string" ? info.parentID : null
      } catch (error) {
        console.warn("Failed to load OpenCode subagent session:", error)
      }

      const child = await state.launchAgentSession(currentProject.id)
      await state.updateSession(currentProject.id, child.id, {
        name: title || child.name,
        opencodeSessionId: opencodeSessionID,
        opencodeProviderId: session?.opencodeProviderId ?? state.preferredOpencodeProviderId,
        opencodeModelId: session?.opencodeModelId ?? state.preferredOpencodeModelId,
        opencodeModelVariant: session?.opencodeModelVariant ?? state.preferredOpencodeVariant,
        parentSessionId: parentID ? currentProject.sessions.find((s) => s.opencodeSessionId === parentID)?.id ?? null : null,
      })

      subagentTrackersRef.current.set(opencodeSessionID, {
        localSessionId: child.id,
        opencodeSessionId: opencodeSessionID,
        partsByMessageID: new Map(),
        currentMessageID: null,
        lastFlushedAt: 0,
      })

      if (autoLoad) {
        await state.loadSubagentMessages(currentProject.id, child.id, opencodeSessionID)
      }
    }

    const handleOpenSubagent = (event: Event) => {
      const detail = (event as CustomEvent<{ sessionID?: unknown; title?: unknown }>).detail
      if (typeof detail?.sessionID !== "string" || !detail.sessionID) return
      void openSubagentSession(detail.sessionID, typeof detail.title === "string" ? detail.title : undefined, true)
    }

    window.addEventListener("shob:open-opencode-session", handleOpenSubagent)
    return () => window.removeEventListener("shob:open-opencode-session", handleOpenSubagent)
  }, [
    project?.id,
    session?.opencodeModelId,
    session?.opencodeModelVariant,
    session?.opencodeProviderId,
  ])

  const openSubagentSessionAutoCreate = async (opencodeSessionID: string, projectId: string, parentSessionId: string) => {
    const state = useStore.getState()
    const currentProject = state.projects.find((item) => item.id === projectId)
    if (!currentProject) return

    const existing = currentProject.sessions.find((item) => item.kind === "agent" && item.opencodeSessionId === opencodeSessionID)
    if (existing) {
      if (!subagentTrackersRef.current.has(opencodeSessionID)) {
        subagentTrackersRef.current.set(opencodeSessionID, {
          localSessionId: existing.id,
          opencodeSessionId: opencodeSessionID,
          partsByMessageID: new Map(),
          currentMessageID: null,
          lastFlushedAt: 0,
        })
      }
      return
    }

    let title: string | undefined
    let parentOpenCodeID: string | null = null
    try {
      const info = await nativeApi.invoke("opencode_session_get", {
        directory: currentProject.path,
        sessionID: opencodeSessionID,
      })
      if (info.title && !isOpenCodeDefaultTitle(info.title)) {
        title = info.title.trim()
      }
      parentOpenCodeID = typeof info.parentID === "string" ? info.parentID : null
    } catch (error) {
      console.warn("Failed to load OpenCode subagent session:", error)
    }

    const prevActiveSession = state.activeSessionId
    const child = await state.launchAgentSession(currentProject.id)
    await state.updateSession(currentProject.id, child.id, {
      name: title || child.name,
      opencodeSessionId: opencodeSessionID,
      opencodeProviderId: session?.opencodeProviderId ?? state.preferredOpencodeProviderId,
      opencodeModelId: session?.opencodeModelId ?? state.preferredOpencodeModelId,
      opencodeModelVariant: session?.opencodeModelVariant ?? state.preferredOpencodeVariant,
      parentSessionId: parentOpenCodeID ? currentProject.sessions.find((s) => s.opencodeSessionId === parentOpenCodeID)?.id ?? null : null,
    })
    state.setActiveSession(prevActiveSession)

    subagentTrackersRef.current.set(opencodeSessionID, {
      localSessionId: child.id,
      opencodeSessionId: opencodeSessionID,
      partsByMessageID: new Map(),
      currentMessageID: null,
      lastFlushedAt: 0,
    })

    await state.loadSubagentMessages(currentProject.id, child.id, opencodeSessionID)
  }

  useEffect(() => {
    if (!project?.path) return

    const flushSubagentToStore = (tracker: SubagentTracker) => {
      const state = useStore.getState()
      const proj = state.projects.find((p) => p.id === project.id)
      if (!proj) return

      const allParts: OpenCodePartView[] = []
      for (const partsMap of tracker.partsByMessageID.values()) {
        allParts.push(...partsMap.values())
      }
      const sorted = sortOpenCodeParts(allParts)

      const agentMessages: AgentMessage[] = []
      const messageOrder = new Map<string, number>()
      let orderCounter = 0

      for (const [messageID, partsMap] of tracker.partsByMessageID.entries()) {
        messageOrder.set(messageID, orderCounter++)
        const parts = sortOpenCodeParts([...partsMap.values()])
        const textParts = parts.filter((p) => p.type === "text")
        const toolParts = parts.filter((p) => p.type === "tool")
        const content = textParts.map((p) => p.text ?? "").join("\n").trim()

        agentMessages.push({
          id: messageID,
          role: "assistant",
          content: content || "Subagent completed its task.",
          createdAt: parts[0]?.state?.time?.start ?? Date.now(),
          parts,
          toolCalls: toolParts.map((tp) => ({
            id: tp.id ?? null,
            callID: tp.callID ?? null,
            tool: tp.tool ?? "tool",
            status: tp.state?.status ?? "completed",
            title: tp.state?.title ?? null,
            input: tp.state?.input ?? null,
            output: tp.state?.output ?? null,
            error: tp.state?.error ?? null,
            raw: tp.state?.raw ?? null,
            metadata: tp.state?.metadata ?? null,
            attachments: tp.state?.attachments ?? null,
            startedAt: tp.state?.time?.start ?? null,
            endedAt: tp.state?.time?.end ?? null,
            compactedAt: tp.state?.time?.compacted ?? null,
          })),
        })
      }

      agentMessages.sort((a, b) => (messageOrder.get(a.id) ?? 0) - (messageOrder.get(b.id) ?? 0))

      void state.updateSession(project.id, tracker.localSessionId, {
        agentMessages,
        lastActiveAt: Date.now(),
      })
    }

    const handleSubagentEvent = (payload: ElectronOpencodeEventEnvelope) => {
      if (payload.type !== "event") return
      const event = payload.event as OpenCodeEventPayload
      const properties = event?.properties ?? {}
      const eventSessionID = properties.sessionID
      if (!eventSessionID) return

      const tracker = subagentTrackersRef.current.get(eventSessionID)
      if (!tracker) return

      if (event?.type === "message.part.updated" && properties.part?.messageID) {
        const part = properties.part
        const messageID = part.messageID
        if (!messageID) return
        tracker.currentMessageID = messageID
        const messageParts = tracker.partsByMessageID.get(messageID) ?? new Map<string, OpenCodePartView>()
        messageParts.set(part.id, part)
        tracker.partsByMessageID.set(messageID, messageParts)

        const now = Date.now()
        if (now - tracker.lastFlushedAt > 200) {
          tracker.lastFlushedAt = now
          flushSubagentToStore(tracker)
        }
        return
      }

      if (event?.type === "message.part.delta" && properties.messageID && properties.partID && properties.field) {
        const messageParts = tracker.partsByMessageID.get(properties.messageID) ?? new Map<string, OpenCodePartView>()
        const current = messageParts.get(properties.partID) ?? {
          id: properties.partID,
          messageID: properties.messageID,
          type: properties.field === "text" ? "text" : "unknown",
        }
        const previous = typeof (current as Record<string, unknown>)[properties.field] === "string"
          ? String((current as Record<string, unknown>)[properties.field])
          : ""
        messageParts.set(properties.partID, {
          ...current,
          type: current.type === "unknown" && properties.field === "text" ? "text" : current.type,
          [properties.field]: `${previous}${properties.delta ?? ""}`,
        } as OpenCodePartView)
        tracker.partsByMessageID.set(properties.messageID, messageParts)

        const now = Date.now()
        if (now - tracker.lastFlushedAt > 200) {
          tracker.lastFlushedAt = now
          flushSubagentToStore(tracker)
        }
        return
      }

      if (event?.type === "message.updated") {
        const info = properties.info
        if (!info || info.role !== "assistant") return
        if (info.id) {
          tracker.currentMessageID = info.id
          if (!tracker.partsByMessageID.has(info.id)) {
            tracker.partsByMessageID.set(info.id, new Map())
          }
        }
        flushSubagentToStore(tracker)
        return
      }

      if (event?.type === "session.updated") {
        flushSubagentToStore(tracker)
      }
    }

    opencodeEventHandlersRef.current.add(handleSubagentEvent)

    return () => {
      opencodeEventHandlersRef.current.delete(handleSubagentEvent)
    }
  }, [project?.id, project?.path])

  useEffect(() => {
    setModelPower(preferredOpencodeVariant || "high")
  }, [preferredOpencodeVariant])

  useEffect(() => {
    if (!project?.path) return

    let disposed = false
    let subscription: ElectronOpencodeEventSubscription | null = null
    let unlisten: (() => void) | null = null

    void nativeApi.invoke("opencode_event_subscribe", {
      directory: project.path,
      global: true,
    }).then(async (created) => {
      if (disposed) {
        void nativeApi.invoke("opencode_event_unsubscribe", { id: created.id }).catch(() => undefined)
        return
      }

      subscription = created
      opencodeEventSubscriptionRef.current = created
      const cleanup = await nativeApi.listen<ElectronOpencodeEventEnvelope>(created.channel, ({ payload }) => {
        if (payload.type === "error") {
          console.warn("OpenCode event stream error:", payload.error)
        }
        for (const handler of opencodeEventHandlersRef.current) {
          handler(payload)
        }
      })
      if (disposed) {
        cleanup()
        void nativeApi.invoke("opencode_event_unsubscribe", { id: created.id }).catch(() => undefined)
        return
      }
      unlisten = cleanup
    }).catch((error) => {
      console.warn("Failed to start OpenCode event stream:", error)
    })

    return () => {
      disposed = true
      if (unlisten) unlisten()
      if (opencodeEventSubscriptionRef.current?.id === subscription?.id) {
        opencodeEventSubscriptionRef.current = null
      }
      if (subscription) {
        void nativeApi.invoke("opencode_event_unsubscribe", { id: subscription.id }).catch(() => undefined)
      }
    }
  }, [project?.path])

  useEffect(() => {
    if (!isActive || !project?.path) return

    let cancelled = false
    setProviderStatus("loading")
    nativeApi.invoke("opencode_provider_list", { directory: project.path })
      .then((providers) => {
        if (cancelled) return
        const options = buildConnectedOpenCodeModelOptions(providers)
        setModelOptions(options)
        setSelectedModel((current) => {
          const picked = pickOpenCodeModel({
            options,
            providers,
            currentValue: current,
            preferredProviderID: preferredOpencodeProviderId,
            preferredModelID: preferredOpencodeModelId,
            sessionProviderID: session?.opencodeProviderId,
            sessionModelID: session?.opencodeModelId,
          })
          return picked?.value ?? ""
        })
        setProviderStatus("ready")
      })
      .catch((error) => {
        if (cancelled) return
        console.error("Failed to load OpenCode providers:", error)
        setModelOptions([])
        setSelectedModel("")
        setProviderStatus("error")
      })

    return () => {
      cancelled = true
    }
  }, [
    isActive,
    preferredOpencodeModelId,
    preferredOpencodeProviderId,
    project?.path,
    session?.opencodeModelId,
    session?.opencodeProviderId,
  ])

  const autoGrow = () => {
    const el = textareaRef.current
    if (!el) return
    el.style.height = "0px"
    const next = Math.min(el.scrollHeight, 240)
    el.style.height = `${next}px`
  }

  useEffect(() => {
    autoGrow()
  }, [input])

  const hasSelectedConnectedModel = Boolean(selectedModel && modelOptions.some((option) => option.value === selectedModel))
  const canSubmit =
    (input.trim().length > 0 || attachedFiles.length > 0) &&
    !isThinking &&
    !!project &&
    !!session &&
    hasSelectedConnectedModel

  const handleSubmit = async () => {
    if (!canSubmit || !project || !session) return

    const text = input.trim()
    const attachmentLine =
      attachedFiles.length > 0 ? `\n\nAttached files: ${attachedFiles.map((file) => file.name).join(", ")}` : ""
    const promptText = `${text}${attachmentLine}`.trim()
    setInput("")
    setAttachedFiles([])
    autoGrow()

    await appendAgentMessage(project.id, session.id, {
      role: "user",
      content: promptText,
    })

    const promptRunId = crypto.randomUUID()
    activePromptRef.current = promptRunId
    setIsThinking(true)
    setLiveAssistant({ content: "", toolCalls: [], parts: [], error: null, createdAt: Date.now() })

    let removeOpencodeEventHandler: (() => void) | null = null

    try {
      const model = parseOpenCodeModelValue(selectedModel, modelOptions)
      if (!model.providerID || !model.modelID) {
        throw new Error("Connect and select an OpenCode model before sending a message.")
      }
      setPreferredOpencodeModel(model.providerID, model.modelID)
      setPreferredOpencodeVariant(modelPower)
      const partsByMessageID = new Map<string, Map<string, OpenCodePartView>>()
      const requestMessageID = createOpenCodeID("message")
      let resolvedSessionID = session.opencodeSessionId ?? null
      let assistantMessageID: string | null = null
      let completedFromEvent = false
      let eventError: unknown = null
      let finalContent = ""
      let finalToolCalls: ToolCallView[] = []
      let finalParts: OpenCodePartView[] = []
      let finalError: unknown = null

      const setLiveAssistantSnapshot = (snapshot: Omit<LiveAssistantState, "createdAt">) => {
        setLiveAssistant((current) => ({
          ...snapshot,
          createdAt: current?.createdAt ?? Date.now(),
        }))
      }

      const collectAllParts = (): OpenCodePartView[] => {
        const all: OpenCodePartView[] = []
        for (const partsMap of partsByMessageID.values()) {
          all.push(...partsMap.values())
        }
        return sortOpenCodeParts(all)
      }

      const applyPromptStatusSnapshot = (status: {
        assistantMessageID?: string | null
        parts?: unknown[]
        content?: string
        toolCalls?: ToolCallView[]
        error?: unknown
      }) => {
        if (status.assistantMessageID) assistantMessageID = status.assistantMessageID

        const statusParts = normalizeRawPartsForView(status.parts, assistantMessageID)
        if (statusParts.length > 0 && assistantMessageID) {
          const messageParts = partsByMessageID.get(assistantMessageID) ?? new Map<string, OpenCodePartView>()
          statusParts.forEach((part) => messageParts.set(part.id, part))
          partsByMessageID.set(assistantMessageID, messageParts)
        }

        const mergedParts = collectAllParts()
        const hasSnapshot =
          mergedParts.length > 0 ||
          Boolean(status.content) ||
          Boolean(status.toolCalls?.length) ||
          Boolean(status.error)

        if (!hasSnapshot) return

        finalParts = mergedParts.length > 0 ? mergedParts : finalParts
        finalContent = mergedParts.length > 0 ? extractTextFromParts(mergedParts) : status.content || finalContent
        finalToolCalls = mergedParts.length > 0
          ? extractToolCallsFromParts(mergedParts)
          : status.toolCalls?.length ? status.toolCalls : finalToolCalls
        finalError = status.error ?? finalError

        if (finalParts.length === 0) {
          finalParts = buildAssistantParts(assistantMessageID ?? "assistant", finalContent, finalToolCalls)
        }

        setLiveAssistantSnapshot({
          content: finalContent,
          toolCalls: finalToolCalls,
          parts: finalParts,
          error: finalError,
        })
      }

      const maybeSyncGeneratedTitle = async (opencodeSessionID: string, attempts = 1) => {
        for (let attempt = 0; attempt < attempts; attempt += 1) {
          if (attempt > 0) await new Promise((resolve) => setTimeout(resolve, 450))
          try {
            const info = await nativeApi.invoke("opencode_session_get", {
              directory: project.path,
              sessionID: opencodeSessionID,
            })
            if (shouldAdoptOpenCodeTitle(info.title, session.name) && info.title) {
              await updateSession(project.id, session.id, { name: info.title.trim() })
              return true
            }
          } catch (error) {
            console.warn("Failed to sync OpenCode generated title:", error)
          }
        }
        return false
      }

      const renderFromParts = () => {
        const parts = collectAllParts()
        finalParts = parts
        finalContent = extractTextFromParts(parts)
        finalToolCalls = extractToolCallsFromParts(parts)
        finalError = eventError
        setLiveAssistantSnapshot({
          content: finalContent,
          toolCalls: finalToolCalls,
          parts: finalParts,
          error: finalError,
        })
      }

      const handleOpencodeEvent = (payload: ElectronOpencodeEventEnvelope) => {
        if (activePromptRef.current !== promptRunId) return
        if (payload.type === "error") {
          console.warn("OpenCode event stream error:", payload.error)
          return
        }
        if (payload.type !== "event") return

        const event = payload.event as OpenCodeEventPayload
        const properties = event?.properties ?? {}
        if (properties.sessionID && resolvedSessionID && properties.sessionID !== resolvedSessionID) return

        if (event?.type === "session.updated") {
          const eventSessionID = properties.sessionID ?? properties.info?.id
          if (!eventSessionID || !resolvedSessionID) return
          if (eventSessionID && resolvedSessionID && eventSessionID !== resolvedSessionID) return
          const nextTitle = properties.info?.title
          if (shouldAdoptOpenCodeTitle(nextTitle, session.name) && nextTitle) {
            void updateSession(project.id, session.id, { name: nextTitle.trim() })
          }
          return
        }

        if (event?.type === "session.error") {
          finalError = properties.error ?? "OpenCode session failed."
          eventError = finalError
          completedFromEvent = true
          setLiveAssistantSnapshot({
            content: finalContent,
            toolCalls: finalToolCalls,
            parts: finalParts,
            error: finalError,
          })
          return
        }

        if (event?.type === "message.updated") {
          const info = properties.info
          if (!info || info.role !== "assistant") return
          if (info.parentID !== requestMessageID) return
          resolvedSessionID = properties.sessionID ?? resolvedSessionID
          assistantMessageID = info.id ?? assistantMessageID
          completedFromEvent = typeof info.time?.completed === "number"
          eventError = info.error ?? null
          renderFromParts()
          return
        }

        if (event?.type === "message.part.updated" && properties.part?.messageID) {
          const part = properties.part
          const messageID = part.messageID
          if (!messageID) return
          const messageParts = partsByMessageID.get(messageID) ?? new Map<string, OpenCodePartView>()
          messageParts.set(part.id, part)
          partsByMessageID.set(messageID, messageParts)
          renderFromParts()
          return
        }

        if (event?.type === "message.part.removed" && properties.messageID && properties.partID) {
          return
        }

        if (event?.type === "message.part.delta" && properties.messageID && properties.partID && properties.field) {
          const messageParts = partsByMessageID.get(properties.messageID) ?? new Map<string, OpenCodePartView>()
          const current = messageParts.get(properties.partID) ?? {
            id: properties.partID,
            messageID: properties.messageID,
            type: properties.field === "text" ? "text" : "unknown",
          }
          const previous = typeof (current as Record<string, unknown>)[properties.field] === "string"
            ? String((current as Record<string, unknown>)[properties.field])
            : ""
          messageParts.set(properties.partID, {
            ...current,
            type: current.type === "unknown" && properties.field === "text" ? "text" : current.type,
            [properties.field]: `${previous}${properties.delta ?? ""}`,
          } as OpenCodePartView)
          partsByMessageID.set(properties.messageID, messageParts)
          renderFromParts()
        }
      }

      opencodeEventHandlersRef.current.add(handleOpencodeEvent)
      removeOpencodeEventHandler = () => {
        opencodeEventHandlersRef.current.delete(handleOpencodeEvent)
      }

      const started = await nativeApi.invoke("opencode_session_prompt_async", {
        directory: project.path,
        sessionID: session.opencodeSessionId,
        title: getOpenCodeCreateTitle(session.name, session.opencodeSessionId),
        prompt: promptText,
        parts: [{ id: createOpenCodeID("part"), type: "text", text: promptText }],
        providerID: model.providerID,
        modelID: model.modelID,
        agent: composerMode,
        variant: modelPower,
        messageID: requestMessageID,
      })

      if (activePromptRef.current !== promptRunId) return
      resolvedSessionID = started.sessionID
      if (
        started.sessionID !== session.opencodeSessionId ||
        model.providerID !== session.opencodeProviderId ||
        model.modelID !== session.opencodeModelId ||
        modelPower !== session.opencodeModelVariant
      ) {
        await updateSession(project.id, session.id, {
          opencodeSessionId: started.sessionID,
          opencodeProviderId: model.providerID,
          opencodeModelId: model.modelID,
          opencodeModelVariant: modelPower,
        })
      }
      void maybeSyncGeneratedTitle(started.sessionID, 1)

      let lastPollAt = 0
      let pollFailures = 0

      while (activePromptRef.current === promptRunId) {
        if (completedFromEvent) break

        if (Date.now() - lastPollAt > 500) {
          lastPollAt = Date.now()
          try {
            const status = await nativeApi.invoke("opencode_session_prompt_status", {
              directory: project.path,
              sessionID: started.sessionID,
              requestMessageID: started.requestMessageID,
            })

            pollFailures = 0
            applyPromptStatusSnapshot(status)

            if (status.completed) break
          } catch (error) {
            pollFailures += 1
            console.warn("OpenCode prompt status poll failed:", error)
            if (pollFailures >= 8 && !finalContent && finalToolCalls.length === 0) {
              throw error
            }
          }
        }

        await new Promise((resolve) => setTimeout(resolve, 80))
      }

      if (activePromptRef.current !== promptRunId) return

      try {
        if (completedFromEvent) {
          await new Promise((resolve) => setTimeout(resolve, 120))
        }

        const finalStatus = await nativeApi.invoke("opencode_session_prompt_status", {
          directory: project.path,
          sessionID: resolvedSessionID ?? started.sessionID,
          requestMessageID: started.requestMessageID,
        })
        applyPromptStatusSnapshot(finalStatus)
      } catch (error) {
        console.warn("OpenCode final prompt snapshot failed:", error)
      }

      if (finalParts.length > 0) {
        finalParts = sortOpenCodeParts(finalParts)
        finalContent = extractTextFromParts(finalParts)
        finalToolCalls = extractToolCallsFromParts(finalParts)
      }

      const error = finalError ? `\n\nOpenCode error: ${describeOpenCodeError(finalError)}` : ""
      const assistantMessageIDForParts = assistantMessageID ?? createOpenCodeID("message")
      const assistantParts = finalParts.length > 0
        ? finalParts
        : buildAssistantParts(assistantMessageIDForParts, finalContent, finalToolCalls)
      await appendAgentMessage(project.id, session.id, {
        role: "assistant",
        content: (finalContent || "OpenCode completed the request without returning text.") + error,
        toolCalls: finalToolCalls,
        parts: assistantParts,
        agent: composerMode,
        model: { providerID: model.providerID, modelID: model.modelID },
        error: finalError ? { name: "OpenCodeError", data: { message: describeOpenCodeError(finalError) } } : null,
      })

      for (const tc of finalToolCalls) {
        if (tc.tool === "task") {
          const meta = tc.metadata as Record<string, unknown> | null
          const childSessionId = typeof meta?.sessionId === "string" ? meta.sessionId : typeof meta?.sessionID === "string" ? meta.sessionID : null
          if (childSessionId) {
            void openSubagentSessionAutoCreate(childSessionId, project.id, session.id)
          }
        }
      }

      void maybeSyncGeneratedTitle(started.sessionID, 8)
    } catch (error) {
      if (activePromptRef.current !== promptRunId) return
      const errorText = `OpenCode could not complete that request.\n\n${describeOpenCodeError(error)}`
      await appendAgentMessage(project.id, session.id, {
        role: "assistant",
        content: errorText,
        parts: buildAssistantParts(createOpenCodeID("message"), errorText, []),
        error: { name: "OpenCodeError", data: { message: describeOpenCodeError(error) } },
      })
    } finally {
      if (removeOpencodeEventHandler) removeOpencodeEventHandler()
      if (activePromptRef.current === promptRunId) {
        activePromptRef.current = null
        setLiveAssistant(null)
        setIsThinking(false)
      }
    }
  }

  const handleKeyDown = (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault()
      void handleSubmit()
    }
  }

  const handleSuggestion = (prompt: string) => {
    setInput(prompt)
    requestAnimationFrame(() => {
      textareaRef.current?.focus()
    })
  }

  const handlePickFiles = () => fileInputRef.current?.click()

  const handleFilesSelected = (event: React.ChangeEvent<HTMLInputElement>) => {
    const files = event.target.files
    if (!files || files.length === 0) return
    setAttachedFiles(Array.from(files))
    event.target.value = ""
  }

  return (
    <div
      className="agent-container absolute inset-0 flex h-full w-full flex-col"
      data-active={isActive ? "true" : "false"}
      style={{
        display: isActive ? "flex" : "none",
      }}
    >
      <div
        ref={scrollRef}
        className="thin-scrollbar relative z-[1] flex-1 overflow-y-auto"
      >
        {messages.length === 0 ? (
          <div className="flex min-h-full w-full items-start justify-center px-6 py-14 sm:py-20">
            <div className="flex w-full max-w-[720px] flex-col items-center text-center">
              <div className="mb-5 flex h-12 w-12 items-center justify-center rounded-2xl border border-border/70 bg-card/70">
                <Sparkles className="h-5 w-5 text-foreground/85" strokeWidth={1.7} />
              </div>

              <h1 className="agent-hero-text mb-3 text-[30px] font-medium leading-tight tracking-tight sm:text-[36px]">
                How can I help you today?
              </h1>

              <p className="mb-8 max-w-[520px] text-[13px] leading-[1.6] text-muted-foreground">
                Ask anything about the project. The agent has access to this workspace
                and can read files, run tools, and coordinate sub-agents.
              </p>

              <div className="mb-10 flex flex-col items-center gap-2 text-[12px] text-muted-foreground">
                {project && (
                  <div className="flex max-w-[520px] items-baseline gap-0 break-words text-center">
                    <span className="text-muted-foreground/75">{projectPathParts.parent}</span>
                    <span className="font-medium text-foreground">{projectPathParts.name}</span>
                  </div>
                )}
                <div className="flex items-center gap-1.5 text-muted-foreground/80">
                  <GitBranch className="h-3.5 w-3.5" strokeWidth={1.7} />
                  <span>main</span>
                  {lastUpdatedLabel && (
                    <>
                      <span className="mx-1 text-muted-foreground/40">\u00b7</span>
                      <span>Last activity {lastUpdatedLabel}</span>
                    </>
                  )}
                </div>
              </div>

              <div className="grid w-full max-w-[560px] grid-cols-1 gap-2 sm:grid-cols-2">
                {SUGGESTED_PROMPTS.map((prompt) => (
                  <button
                    key={prompt}
                    type="button"
                    onClick={() => handleSuggestion(prompt)}
                    className="agent-glass rounded-xl px-3.5 py-3 text-left text-[12.5px] leading-[1.5] text-foreground/82 transition-colors hover:text-foreground"
                  >
                    {prompt}
                  </button>
                ))}
              </div>
            </div>
          </div>
        ) : (
          <div className="mx-auto flex w-full max-w-[800px] flex-col gap-2 px-6 py-8 sm:py-10 2xl:max-w-[1000px]">
            <MessageGroupRenderer
              messages={messages}
              isThinking={isThinking}
              liveAssistant={liveAssistant}
            />
          </div>
        )}
      </div>

      <div className="relative z-[1] shrink-0 px-4 pb-5 pt-3 sm:px-6">
        <div className="mx-auto w-full max-w-[800px] 2xl:max-w-[1000px]">
          <div className="agent-composer relative overflow-hidden rounded-[16px] border border-border/70 bg-card/85">
            <textarea
              ref={textareaRef}
              value={input}
              onChange={(event) => setInput(event.target.value)}
              onKeyDown={handleKeyDown}
              placeholder={
                project
                  ? "Ask the agent anything\u2026"
                  : "Open a project folder to start chatting"
              }
              disabled={!project}
              rows={1}
              className="w-full max-h-[200px] min-h-[52px] resize-none overflow-y-auto bg-transparent px-3.5 pb-14 pt-3 text-[14px] leading-[1.5] text-foreground placeholder:text-muted-foreground/70 focus:outline-none disabled:cursor-not-allowed disabled:opacity-60"
            />

            <div
              aria-hidden="true"
              className="pointer-events-none absolute inset-x-0 bottom-[42px] h-7"
              style={{ background: "linear-gradient(to top, var(--card), transparent)" }}
            />

            <div className="absolute inset-x-0 bottom-0 z-[2] flex items-center justify-between gap-2 px-2.5 py-2">
              <input
                ref={fileInputRef}
                type="file"
                multiple
                className="hidden"
                onChange={handleFilesSelected}
              />

              <div className="flex min-w-0 flex-1 items-center gap-1 pointer-events-auto">
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-sm"
                  className="h-6 w-6 rounded-full border border-border/70 bg-card/90 text-foreground shadow-xs hover:bg-accent/70"
                  onClick={handlePickFiles}
                  title="Add files"
                >
                  <Plus className="h-3.5 w-3.5" />
                </Button>

                <div className="inline-flex items-center gap-0.5 rounded-md border border-border/70 bg-card/90 p-0.5 shadow-xs">
                  <button
                    type="button"
                    onClick={() => setComposerMode("build")}
                    className={`rounded px-2 py-1 text-[11px] leading-none transition-colors ${
                      composerMode === "build"
                        ? "bg-accent/85 text-foreground"
                        : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                    }`}
                  >
                    Build
                  </button>
                  <button
                    type="button"
                    onClick={() => setComposerMode("plan")}
                    className={`rounded px-2 py-1 text-[11px] leading-none transition-colors ${
                      composerMode === "plan"
                        ? "bg-accent/85 text-foreground"
                        : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                    }`}
                  >
                    Plan
                  </button>
                </div>

                <div className="relative max-w-[140px]">
                  <select
                    value={selectedModel}
                    onChange={(event) => {
                      const value = event.target.value
                      setSelectedModel(value)
                      const model = parseOpenCodeModelValue(value, modelOptions)
                      if (model.providerID && model.modelID) {
                        setPreferredOpencodeModel(model.providerID, model.modelID)
                      }
                    }}
                    disabled={providerStatus === "loading" || modelOptions.length === 0}
                    className="h-7 w-full appearance-none rounded-md border border-border/70 bg-card/90 pl-2 pr-6 text-[11px] text-foreground shadow-xs outline-none transition-colors hover:bg-accent/55"
                    title={providerStatus === "loading" ? "Loading OpenCode models" : "Connected model"}
                  >
                    {modelOptions.length === 0 ? (
                      <option value="">
                        {providerStatus === "loading" ? "Loading models..." : "Connect a model"}
                      </option>
                    ) : (
                      modelOptions.map((option) => (
                        <option key={option.value} value={option.value}>
                          {option.shortLabel}
                        </option>
                      ))
                    )}
                  </select>
                  <ChevronDown className="pointer-events-none absolute right-1.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                </div>

                <div className="relative max-w-[95px]">
                  <select
                    value={modelPower}
                    onChange={(event) => {
                      setModelPower(event.target.value)
                      setPreferredOpencodeVariant(event.target.value)
                    }}
                    className="h-7 w-full appearance-none rounded-md border border-border/70 bg-card/90 pl-2 pr-6 text-[11px] text-foreground shadow-xs outline-none transition-colors hover:bg-accent/55"
                    title="Model power"
                  >
                    <option value="low">Low</option>
                    <option value="medium">Medium</option>
                    <option value="high">High</option>
                    <option value="xhigh">XHigh</option>
                  </select>
                  <ChevronDown className="pointer-events-none absolute right-1.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                </div>

                {attachedFiles.length > 0 && (
                  <span className="truncate text-[11px] text-muted-foreground">
                    {attachedFiles.length} file{attachedFiles.length > 1 ? "s" : ""} selected
                  </span>
                )}
              </div>

              {isThinking ? (
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  onClick={() => {
                    activePromptRef.current = null
                    setLiveAssistant(null)
                    if (project?.path && session?.opencodeSessionId) {
                      void nativeApi.invoke("opencode_session_abort", {
                        directory: project.path,
                        sessionID: session.opencodeSessionId,
                      }).catch((error) => console.warn("Failed to abort OpenCode session:", error))
                    }
                    setIsThinking(false)
                  }}
                  className="pointer-events-auto h-8 gap-1.5 rounded-full px-3"
                >
                  <StopCircle className="h-3.5 w-3.5" />
                  Stop
                </Button>
              ) : (
                <Button
                  type="button"
                  size="icon-sm"
                  onClick={() => void handleSubmit()}
                  disabled={!canSubmit}
                  className="pointer-events-auto h-8 w-8 rounded-full"
                  title="Send (Enter)"
                >
                  <ArrowUp className="h-4 w-4" strokeWidth={2.2} />
                </Button>
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

export const AgentView = memo(
  AgentViewComponent,
  (prev, next) => prev.sessionId === next.sessionId && prev.isActive === next.isActive,
)
