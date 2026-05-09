import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react"
import {
  ArrowUp,
  ChevronDown,
  GitBranch,
  Loader2,
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
import { ToolPart, SessionTurn } from "@/components/opencode/tools"
import "@/components/opencode/tools/tool-renderers"


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
  messageID: string
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

type OpenCodeMessageInfo = {
  id?: string
  role?: string
  parentID?: string
  error?: unknown
  time?: {
    created?: number
    completed?: number
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
    info?: OpenCodeMessageInfo
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



const ToolCallsList = ({ toolCalls, messageID }: { toolCalls: ToolCallView[]; messageID: string }) => {
  if (!Array.isArray(toolCalls) || toolCalls.length === 0) return null

  return (
    <div className="mb-3 space-y-0.5 border-l border-border/70 pl-1" data-component="tool-call-timeline">
      {toolCalls.map((toolCall, index) => (
        <div
          key={`${messageID}-tool-${toolCall.id ?? toolCall.callID ?? index}`}
          data-component="tool-part-wrapper"
          data-tool={toolCall.tool}
          data-status={toolCall.status}
        >
          <ToolPart toolCall={toolCall} />
        </div>
      ))}
    </div>
  )
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AgentMsg = any

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

const convertToolCallsToParts = (toolCalls: ToolCallView[] | null | undefined) => {
  if (!toolCalls || toolCalls.length === 0) return undefined
  return toolCalls.map((tc) => ({
    id: tc.callID ?? tc.id ?? tc.tool,
    type: "tool" as const,
    tool: tc.tool,
    callID: tc.callID ?? tc.id ?? undefined,
    state: {
      status: tc.status,
      title: tc.title ?? undefined,
      input: tc.input as Record<string, unknown> | undefined,
      output: tc.output ?? undefined,
      error: tc.error ?? undefined,
      metadata: tc.metadata as Record<string, unknown> | undefined,
      time: {
        start: tc.startedAt ?? undefined,
        end: tc.endedAt ?? undefined,
        compacted: tc.compactedAt ?? undefined,
      },
    },
  }))
}

function MessageGroupRenderer({
  messages: msgs,
  isThinking,
  liveAssistant,
  sessionId,
}: {
  messages: AgentMsg[]
  isThinking: boolean
  liveAssistant: { content: string; toolCalls: ToolCallView[]; error: unknown } | null
  sessionId: string
}) {
  const groups = useMemo(() => convertToSessionFormat(msgs), [msgs])

  const convertMessageForSessionTurn = useCallback((msg: AgentMsg) => ({
    id: msg.id,
    role: msg.role,
    content: msg.content ?? undefined,
    parts: convertToolCallsToParts(msg.toolCalls) as Array<{
      id: string
      type: string
      text?: string
      tool?: string
      state?: {
        status?: string
        input?: unknown
        output?: string
        error?: string
        metadata?: Record<string, unknown>
      }
    }> | undefined,
    summary: (msg as AgentMsg).summary,
    agent: (msg as AgentMsg).agent ?? undefined,
    model: (msg as AgentMsg).model ?? undefined,
    time: (msg as AgentMsg).time ?? undefined,
    error: (msg as AgentMsg).error ?? undefined,
  }), [])

  return (
    <>
      {groups.map((group) => {
        const convertedUser = convertMessageForSessionTurn(group.userMessage)
        const convertedAssistants = group.assistantMessages.map(convertMessageForSessionTurn)
        return (
          <div key={group.userMessage.id} className="w-full">
            <SessionTurn
              messages={[convertedUser]}
              userMessageIndex={0}
              assistantMessages={convertedAssistants}
            />
          </div>
        )
      })}
      {isThinking && liveAssistant && (
        <div className="w-full">
          <div data-component="tool-part-wrapper" className="my-2">
            <div className="flex items-center gap-2 text-[13px] text-muted-foreground mb-1">
              <Loader2 className="size-3.5 animate-spin" />
              <span>Thinking</span>
            </div>
            {liveAssistant.toolCalls.length > 0 && (
              <ToolCallsList toolCalls={liveAssistant.toolCalls} messageID={`live-${sessionId}`} />
            )}
            {liveAssistant.content && (
              <div className="whitespace-pre-wrap text-[14px] leading-relaxed text-foreground/92">
                {liveAssistant.content}
              </div>
            )}
          </div>
        </div>
      )}
      {isThinking && !liveAssistant && (
        <div className="w-full">
          <div className="flex items-center gap-2 text-[13px] text-muted-foreground py-4">
            <Loader2 className="size-3.5 animate-spin" />
            <span>Thinking</span>
          </div>
        </div>
      )}
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
  const [liveAssistant, setLiveAssistant] = useState<{ content: string; toolCalls: ToolCallView[]; error: unknown | null } | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const activePromptRef = useRef<string | null>(null)
  const opencodeEventHandlersRef = useRef(new Set<(payload: ElectronOpencodeEventEnvelope) => void>())
  const opencodeEventSubscriptionRef = useRef<ElectronOpencodeEventSubscription | null>(null)

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
    setLiveAssistant({ content: "", toolCalls: [], error: null })

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
      let finalError: unknown = null

      const renderFromParts = () => {
        if (!assistantMessageID) return
        const parts = sortOpenCodeParts([...(partsByMessageID.get(assistantMessageID)?.values() ?? [])])
        finalContent = extractTextFromParts(parts)
        finalToolCalls = extractToolCallsFromParts(parts)
        finalError = eventError
        setLiveAssistant({
          content: finalContent,
          toolCalls: finalToolCalls,
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

        if (event?.type === "session.error") {
          finalError = properties.error ?? "OpenCode session failed."
          eventError = finalError
          completedFromEvent = true
          setLiveAssistant({
            content: finalContent,
            toolCalls: finalToolCalls,
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
          const messageParts = partsByMessageID.get(part.messageID) ?? new Map<string, OpenCodePartView>()
          messageParts.set(part.id, part)
          partsByMessageID.set(part.messageID, messageParts)
          if (part.messageID === assistantMessageID) renderFromParts()
          return
        }

        if (event?.type === "message.part.removed" && properties.messageID && properties.partID) {
          const messageParts = partsByMessageID.get(properties.messageID)
          messageParts?.delete(properties.partID)
          if (properties.messageID === assistantMessageID) renderFromParts()
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
          if (properties.messageID === assistantMessageID) renderFromParts()
        }
      }

      opencodeEventHandlersRef.current.add(handleOpencodeEvent)
      removeOpencodeEventHandler = () => {
        opencodeEventHandlersRef.current.delete(handleOpencodeEvent)
      }

      const started = await nativeApi.invoke("opencode_session_prompt_async", {
        directory: project.path,
        sessionID: session.opencodeSessionId,
        title: session.name,
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
            if (status.assistantMessageID) assistantMessageID = status.assistantMessageID
            if (status.content || status.toolCalls?.length || status.error) {
              finalContent = status.content || finalContent
              finalToolCalls = status.toolCalls?.length ? status.toolCalls : finalToolCalls
              finalError = status.error ?? finalError
              setLiveAssistant({
                content: finalContent,
                toolCalls: finalToolCalls,
                error: finalError,
              })
            }

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

      const error = finalError ? `\n\nOpenCode error: ${describeOpenCodeError(finalError)}` : ""
      await appendAgentMessage(project.id, session.id, {
        role: "assistant",
        content: (finalContent || "OpenCode completed the request without returning text.") + error,
        toolCalls: finalToolCalls,
      })
    } catch (error) {
      if (activePromptRef.current !== promptRunId) return
      await appendAgentMessage(project.id, session.id, {
        role: "assistant",
        content: `OpenCode could not complete that request.\n\n${describeOpenCodeError(error)}`,
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
              sessionId={sessionId}
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
