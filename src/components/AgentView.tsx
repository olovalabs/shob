import { memo, useEffect, useMemo, useRef, useState } from "react"
import { ArrowUp, ChevronDown, GitBranch, Plus, Sparkles, StopCircle } from "lucide-react"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"


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

  const [input, setInput] = useState("")
  const [isThinking, setIsThinking] = useState(false)
  const [composerMode, setComposerMode] = useState<"build" | "plan">("build")
  const [selectedModel, setSelectedModel] = useState("gpt-5")
  const [modelPower, setModelPower] = useState("high")
  const [attachedFiles, setAttachedFiles] = useState<File[]>([])
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const thinkingTimerRef = useRef<number | null>(null)

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
      if (thinkingTimerRef.current !== null) {
        window.clearTimeout(thinkingTimerRef.current)
        thinkingTimerRef.current = null
      }
    }
  }, [sessionId])

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
    const metaLine = `\n\nMode: ${composerMode} | Model: ${selectedModel} | Power: ${modelPower}`
    setInput("")
    setAttachedFiles([])
    autoGrow()

    await appendAgentMessage(project.id, session.id, {
      role: "user",
      content: `${text}${attachmentLine}${metaLine}`.trim(),
    })

    setIsThinking(true)
    thinkingTimerRef.current = window.setTimeout(() => {
      thinkingTimerRef.current = null
      void appendAgentMessage(project.id, session.id, {
        role: "assistant",
        content:
          "The agent runtime isn't wired up yet \u2014 this is a UI-only placeholder. Connect an LLM backend here to have the agent respond to your messages in this session.",
      }).finally(() => {
        setIsThinking(false)
      })
    }, 900)
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
      <div className="agent-bg" aria-hidden />
      <div className="agent-blob agent-blob-1" aria-hidden />
      <div className="agent-blob agent-blob-2" aria-hidden />

      <div
        ref={scrollRef}
        className="thin-scrollbar relative z-[1] flex-1 overflow-y-auto"
      >
        {messages.length === 0 ? (
          <div className="flex min-h-full w-full items-start justify-center px-6 py-14 sm:py-20">
            <div className="agent-fade-up flex w-full max-w-[720px] flex-col items-center text-center">
              <div className="mb-5 flex h-12 w-12 items-center justify-center rounded-2xl border border-border/70 bg-card/70 backdrop-blur">
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
          <div className="mx-auto flex w-full max-w-[780px] flex-col gap-5 px-6 py-8 sm:py-10">
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
                    {message.content}
                  </div>
                </div>
              )
            })}

            {isThinking && (
              <div className="agent-message-bubble flex w-full justify-start">
                <div className="flex items-center gap-2 text-[12.5px] text-muted-foreground">
                  <span className="inline-flex h-5 w-5 items-center justify-center rounded-md border border-border/70 bg-background/60">
                    <Sparkles className="h-3 w-3 animate-pulse text-foreground/70" strokeWidth={1.8} />
                  </span>
                  <span className="inline-flex items-center gap-1">
                    Thinking
                    <span className="inline-flex gap-0.5">
                      <span className="h-1 w-1 animate-bounce rounded-full bg-muted-foreground [animation-delay:-0.3s]" />
                      <span className="h-1 w-1 animate-bounce rounded-full bg-muted-foreground [animation-delay:-0.15s]" />
                      <span className="h-1 w-1 animate-bounce rounded-full bg-muted-foreground" />
                    </span>
                  </span>
                </div>
              </div>
            )}
          </div>
        )}
      </div>

      <div className="relative z-[1] shrink-0 px-4 pb-5 pt-3 sm:px-6">
        <div className="agent-fade-up mx-auto w-full max-w-[780px]">
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
                    title="Model"
                  >
                    <option value="gpt-5">GPT-5</option>
                    <option value="gpt-5.4">GPT-5.4</option>
                    <option value="gpt-5.4-mini">GPT-5.4-Mini</option>
                    <option value="gpt-5.3-codex">GPT-5.3-Codex</option>
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
                    if (thinkingTimerRef.current !== null) {
                      window.clearTimeout(thinkingTimerRef.current)
                      thinkingTimerRef.current = null
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
