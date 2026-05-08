import { memo, useEffect, useMemo, useRef, useState } from "react"
import {
  ArrowUp,
  Bot,
  Brain,
  CheckCircle2,
  ChevronDown,
  Circle,
  Code2,
  FileSearch,
  GitBranch,
  Globe,
  ListTree,
  Loader2,
  PencilLine,
  Plus,
  Search,
  Sparkles,
  SquareTerminal,
  StopCircle,
  Wrench,
} from "lucide-react"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"
import { nativeApi } from "@/services/native"
import type { ElectronOpencodeProviderList } from "../electron"


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

type ModelOption = {
  value: string
  label: string
  providerID: string
  modelID: string
  connected: boolean
}

type ToolCallView = {
  id?: string | null
  callID?: string | null
  tool: string
  status: "pending" | "running" | "completed" | "error" | string
  title?: string | null
  input?: unknown
  output?: string | null
  error?: string | null
  startedAt?: number | null
  endedAt?: number | null
}

const FALLBACK_MODEL_OPTIONS: ModelOption[] = [
  { value: "openai/gpt-5", label: "OpenAI · GPT-5", providerID: "openai", modelID: "gpt-5", connected: false },
  { value: "openai/gpt-5.4", label: "OpenAI · GPT-5.4", providerID: "openai", modelID: "gpt-5.4", connected: false },
  {
    value: "openai/gpt-5.4-mini",
    label: "OpenAI · GPT-5.4-Mini",
    providerID: "openai",
    modelID: "gpt-5.4-mini",
    connected: false,
  },
  {
    value: "openai/gpt-5.3-codex",
    label: "OpenAI · GPT-5.3-Codex",
    providerID: "openai",
    modelID: "gpt-5.3-codex",
    connected: false,
  },
]

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

const buildModelOptions = (providers: ElectronOpencodeProviderList | null): ModelOption[] => {
  if (!providers?.all?.length) return FALLBACK_MODEL_OPTIONS

  const connected = new Set(providers.connected ?? [])
  const options = providers.all.flatMap((provider) =>
    Object.values(provider.models ?? {}).map((model) => ({
      value: `${provider.id}/${model.id}`,
      label: `${provider.name} · ${model.name || model.id}`,
      providerID: provider.id,
      modelID: model.id,
      connected: connected.has(provider.id),
    })),
  )

  return options
    .sort((left, right) => {
      if (left.connected !== right.connected) return left.connected ? -1 : 1
      return left.label.localeCompare(right.label)
    })
    .slice(0, 300)
}

const pickDefaultModel = (providers: ElectronOpencodeProviderList | null, options: ModelOption[]) => {
  for (const [providerID, modelID] of Object.entries(providers?.default ?? {})) {
    const match = options.find((item) => item.providerID === providerID && item.modelID === modelID)
    if (match) return match.value
  }

  return options.find((item) => item.connected)?.value ?? options[0]?.value ?? FALLBACK_MODEL_OPTIONS[0].value
}

const parseModelValue = (value: string, options: ModelOption[]) => {
  const option = options.find((item) => item.value === value)
  if (option) return { providerID: option.providerID, modelID: option.modelID }

  const [providerID, ...modelParts] = value.split("/")
  return {
    providerID: providerID || "openai",
    modelID: modelParts.join("/") || value,
  }
}

const describeOpenCodeError = (error: unknown) => {
  if (error instanceof Error) return error.message
  if (typeof error === "string") return error
  if (error && typeof error === "object") {
    const data = error as any
    return data?.data?.message ?? data?.message ?? JSON.stringify(data)
  }
  return "OpenCode returned an unknown error"
}

const formatToolInput = (value: unknown) => {
  if (value === null || value === undefined) return ""
  if (typeof value === "string") return value
  try {
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
}

const asRecord = (value: unknown): Record<string, unknown> =>
  value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {}

const basename = (value: unknown) => {
  if (typeof value !== "string" || !value) return undefined
  return value.split(/[\\/]/).filter(Boolean).at(-1) ?? value
}

const stringValue = (value: unknown) => (typeof value === "string" && value ? value : undefined)

const getToolInfo = (toolCall: ToolCallView) => {
  const input = asRecord(toolCall.input)
  const subtitle =
    stringValue(input.description) ??
    stringValue(input.query) ??
    stringValue(input.url) ??
    basename(input.filePath) ??
    basename(input.path) ??
    stringValue(input.pattern) ??
    stringValue(input.name) ??
    toolCall.title ??
    undefined

  switch (toolCall.tool) {
    case "read":
      return { Icon: FileSearch, title: "Read", subtitle: basename(input.filePath) ?? subtitle }
    case "list":
      return { Icon: ListTree, title: "List", subtitle: basename(input.path) ?? subtitle }
    case "glob":
      return { Icon: Search, title: "Glob", subtitle: stringValue(input.pattern) ?? subtitle }
    case "grep":
      return { Icon: Search, title: "Grep", subtitle: stringValue(input.pattern) ?? subtitle }
    case "bash":
      return { Icon: SquareTerminal, title: "Shell", subtitle }
    case "edit":
      return { Icon: PencilLine, title: "Edit", subtitle: basename(input.filePath) ?? subtitle }
    case "write":
      return { Icon: PencilLine, title: "Write", subtitle: basename(input.filePath) ?? subtitle }
    case "apply_patch":
      return { Icon: Code2, title: "Patch", subtitle }
    case "webfetch":
      return { Icon: Globe, title: "Web Fetch", subtitle: stringValue(input.url) ?? subtitle }
    case "websearch":
      return { Icon: Globe, title: "Web Search", subtitle: stringValue(input.query) ?? subtitle }
    case "task":
      return { Icon: Bot, title: "Agent", subtitle }
    case "skill":
      return { Icon: Brain, title: stringValue(input.name) ?? "Skill", subtitle }
    default:
      return { Icon: Wrench, title: toolCall.tool, subtitle }
  }
}

const toolStatusLabel = (status: string) => {
  if (status === "completed") return "Completed"
  if (status === "running") return "Running"
  if (status === "pending") return "Pending"
  if (status === "error") return "Error"
  return status
}

const toolArgs = (input: unknown) => {
  const record = asRecord(input)
  const skip = new Set(["description", "query", "url", "filePath", "path", "pattern", "name"])
  return Object.entries(record)
    .filter(([key]) => !skip.has(key))
    .flatMap(([key, value]) => {
      if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
        return [`${key}=${String(value)}`]
      }
      return []
    })
    .slice(0, 3)
}

const formatDuration = (start?: number | null, end?: number | null) => {
  if (typeof start !== "number") return null
  const stop = typeof end === "number" ? end : Date.now()
  const seconds = Math.max(0, (stop - start) / 1000)
  if (seconds < 1) return "<1s"
  if (seconds < 60) return `${Math.round(seconds)}s`
  return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`
}

const ToolCallsList = ({ toolCalls, messageID }: { toolCalls: ToolCallView[]; messageID: string }) => {
  if (!Array.isArray(toolCalls) || toolCalls.length === 0) return null

  return (
    <div className="mb-3 space-y-1.5 border-l border-border/70 pl-3">
      {toolCalls.map((toolCall, index) => {
        const { Icon, title, subtitle } = getToolInfo(toolCall)
        const pending = toolCall.status === "pending" || toolCall.status === "running"
        const failed = toolCall.status === "error"
        const args = toolArgs(toolCall.input)
        const duration = formatDuration(toolCall.startedAt, toolCall.endedAt)
        const input = formatToolInput(toolCall.input)
        const hasDetails = Boolean(input || toolCall.output || toolCall.error)

        return (
          <details
            key={`${messageID}-tool-${toolCall.id ?? toolCall.callID ?? index}`}
            className="group rounded-lg"
            open={pending || failed}
            data-component="tool-part-wrapper"
            data-tool={toolCall.tool}
            data-status={toolCall.status}
          >
            <summary className="flex min-h-8 cursor-pointer list-none items-center gap-2 rounded-lg px-2 py-1.5 text-[12px] text-muted-foreground hover:bg-muted/45 [&::-webkit-details-marker]:hidden">
              <span className="inline-flex size-4 shrink-0 items-center justify-center">
                {pending ? (
                  <Loader2 className="size-3.5 animate-spin text-warning" />
                ) : failed ? (
                  <Circle className="size-3.5 fill-destructive text-destructive" />
                ) : (
                  <Icon className="size-3.5 text-muted-foreground" strokeWidth={1.8} />
                )}
              </span>
              <span className="min-w-0 flex items-center gap-2">
                <span className="font-medium text-foreground/90">{title}</span>
                {subtitle ? <span className="truncate text-muted-foreground">{subtitle}</span> : null}
                {args.map((arg) => (
                  <span key={arg} className="hidden rounded bg-muted/70 px-1.5 py-0.5 text-[11px] text-muted-foreground sm:inline">
                    {arg}
                  </span>
                ))}
              </span>
              <span className="ml-auto flex shrink-0 items-center gap-2 text-[11px] text-muted-foreground">
                {duration ? <span>{duration}</span> : null}
                <span className={failed ? "text-destructive" : pending ? "text-warning" : "text-success"}>
                  {toolStatusLabel(toolCall.status)}
                </span>
                {!pending && !failed ? <CheckCircle2 className="size-3.5 text-success" /> : null}
                {hasDetails ? (
                  <ChevronDown className="size-3.5 transition-transform group-open:rotate-180" />
                ) : null}
              </span>
            </summary>
            {hasDetails ? (
              <div className="space-y-2 pb-2 pl-8 pr-2">
                {input ? (
                  <pre className="max-h-60 overflow-auto rounded-md border border-border/60 bg-background/65 p-2 text-[11px] leading-5 text-muted-foreground">
                    {input}
                  </pre>
                ) : null}
                {toolCall.output ? (
                  <pre className="max-h-72 overflow-auto rounded-md border border-border/60 bg-background/65 p-2 text-[11px] leading-5 text-foreground/85">
                    {toolCall.output}
                  </pre>
                ) : null}
                {toolCall.error ? (
                  <div className="rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1.5 text-[11px] text-destructive">
                    {toolCall.error}
                  </div>
                ) : null}
              </div>
            ) : null}
          </details>
        )
      })}
    </div>
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

  const [input, setInput] = useState("")
  const [isThinking, setIsThinking] = useState(false)
  const [composerMode, setComposerMode] = useState<"build" | "plan">("build")
  const [selectedModel, setSelectedModel] = useState(FALLBACK_MODEL_OPTIONS[0].value)
  const [modelPower, setModelPower] = useState("high")
  const [modelOptions, setModelOptions] = useState<ModelOption[]>(FALLBACK_MODEL_OPTIONS)
  const [providerStatus, setProviderStatus] = useState<"idle" | "loading" | "ready" | "error">("idle")
  const [attachedFiles, setAttachedFiles] = useState<File[]>([])
  const [liveAssistant, setLiveAssistant] = useState<{ content: string; toolCalls: ToolCallView[]; error: unknown | null } | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const activePromptRef = useRef<string | null>(null)

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
  }, [messages.length, isThinking])

  useEffect(() => {
    return () => {
      activePromptRef.current = null
    }
  }, [sessionId])

  useEffect(() => {
    if (!isActive || !project?.path) return

    let cancelled = false
    setProviderStatus("loading")
    nativeApi.invoke("opencode_provider_list", { directory: project.path })
      .then((providers) => {
        if (cancelled) return
        const options = buildModelOptions(providers)
        setModelOptions(options)
        setSelectedModel((current) =>
          options.some((option) => option.value === current)
            ? current
            : pickDefaultModel(providers, options),
        )
        setProviderStatus("ready")
      })
      .catch((error) => {
        if (cancelled) return
        console.error("Failed to load OpenCode providers:", error)
        setModelOptions(FALLBACK_MODEL_OPTIONS)
        setProviderStatus("error")
      })

    return () => {
      cancelled = true
    }
  }, [isActive, project?.path])

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

  const canSubmit = (input.trim().length > 0 || attachedFiles.length > 0) && !isThinking && !!project && !!session

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

    try {
      const model = parseModelValue(selectedModel, modelOptions)
      const started = await nativeApi.invoke("opencode_session_prompt_async", {
        directory: project.path,
        sessionID: session.opencodeSessionId,
        title: session.name,
        prompt: promptText,
        providerID: model.providerID,
        modelID: model.modelID,
        agent: composerMode,
        variant: modelPower,
      })

      if (activePromptRef.current !== promptRunId) return
      if (started.sessionID !== session.opencodeSessionId) {
        await updateSession(project.id, session.id, {
          opencodeSessionId: started.sessionID,
          opencodeProviderId: model.providerID,
          opencodeModelId: model.modelID,
          opencodeModelVariant: modelPower,
        })
      }

      let finalContent = ""
      let finalToolCalls: ToolCallView[] = []
      let finalError: unknown = null
      const deadline = Date.now() + 1000 * 60 * 8

      while (activePromptRef.current === promptRunId && Date.now() < deadline) {
        const status = await nativeApi.invoke("opencode_session_prompt_status", {
          directory: project.path,
          sessionID: started.sessionID,
          requestMessageID: started.requestMessageID,
        })

        finalContent = status.content || ""
        finalToolCalls = status.toolCalls ?? []
        finalError = status.error ?? null
        setLiveAssistant({
          content: finalContent,
          toolCalls: finalToolCalls,
          error: finalError,
        })

        if (status.completed) break
        await new Promise((resolve) => setTimeout(resolve, 140))
      }

      if (Date.now() >= deadline && activePromptRef.current === promptRunId) {
        finalError = finalError ?? "Timed out waiting for OpenCode stream completion."
      }

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
          <div className="mx-auto flex w-full max-w-[800px] flex-col gap-5 px-6 py-8 sm:py-10 2xl:max-w-[1000px]">
            {messages.map((message) => {
              const isUser = message.role === "user"
              return (
                <div
                  key={message.id}
                  className={`agent-message-bubble flex w-full ${
                    isUser ? "justify-end" : "justify-start"
                  }`}
                >
                  <div
                    className={`whitespace-pre-wrap text-[13.5px] leading-[1.65] ${
                      isUser
                        ? "max-w-[82%] rounded-2xl border border-border/70 bg-card/80 px-4 py-2.5 text-foreground backdrop-blur"
                        : "w-full text-foreground/92"
                    }`}
                  >
                    {!isUser && (
                      <div className="mb-2 flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.18em] text-muted-foreground">
                        <span className="inline-flex h-5 w-5 items-center justify-center rounded-md border border-border/70 bg-background/60">
                          <Sparkles className="h-3 w-3" strokeWidth={1.8} />
                        </span>
                        Agent
                      </div>
                    )}
                    {!isUser && Array.isArray(message.toolCalls) && message.toolCalls.length > 0 ? (
                      <ToolCallsList toolCalls={message.toolCalls} messageID={message.id} />
                    ) : null}
                    {message.content}
                  </div>
                </div>
              )
            })}

            {isThinking && (
              <div className="agent-message-bubble flex w-full justify-start">
                <div className="w-full">
                  <div className="mb-2 flex items-center gap-2 text-[12.5px] text-muted-foreground">
                    <span className="inline-flex h-5 w-5 items-center justify-center rounded-md border border-border/70 bg-background/60">
                          <Sparkles className="h-3 w-3 text-foreground/70" strokeWidth={1.8} />
                    </span>
                    <span className="inline-flex items-center gap-1.5">
                      <Loader2 className="size-3.5 animate-spin" />
                      Thinking
                    </span>
                  </div>
                  {liveAssistant && liveAssistant.toolCalls.length > 0 ? (
                    <ToolCallsList toolCalls={liveAssistant.toolCalls} messageID={`live-${sessionId}`} />
                  ) : null}
                  {liveAssistant?.content ? (
                    <div className="whitespace-pre-wrap text-[13.5px] leading-[1.65] text-foreground/92">{liveAssistant.content}</div>
                  ) : null}
                </div>
              </div>
            )}
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
                    onChange={(event) => setSelectedModel(event.target.value)}
                    className="h-7 w-full appearance-none rounded-md border border-border/70 bg-card/90 pl-2 pr-6 text-[11px] text-foreground shadow-xs outline-none transition-colors hover:bg-accent/55"
                    title={providerStatus === "loading" ? "Loading OpenCode models" : "Model"}
                  >
                    {modelOptions.map((option) => (
                      <option key={option.value} value={option.value}>
                        {option.connected ? "" : "Not connected - "}{option.label}
                      </option>
                    ))}
                  </select>
                  <ChevronDown className="pointer-events-none absolute right-1.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                </div>

                <div className="relative max-w-[95px]">
                  <select
                    value={modelPower}
                    onChange={(event) => setModelPower(event.target.value)}
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
