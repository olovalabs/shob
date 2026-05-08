import { useEffect, useMemo, useRef, useState } from "react"
import { ArrowUp, GitBranch, Sparkles, StopCircle } from "lucide-react"
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

export function AgentView({ sessionId, isActive = true }: AgentViewProps) {
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
  const textareaRef = useRef<HTMLTextAreaElement>(null)
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

  const canSubmit = input.trim().length > 0 && !isThinking && !!project && !!session

  const handleSubmit = async () => {
    if (!canSubmit || !project || !session) return

    const text = input.trim()
    setInput("")
    autoGrow()

    await appendAgentMessage(project.id, session.id, {
      role: "user",
      content: text,
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

  return (
    <div
      className="agent-container absolute inset-0 flex h-full w-full flex-col"
      data-active={isActive ? "true" : "false"}
      style={{
        visibility: isActive ? "visible" : "hidden",
        pointerEvents: isActive ? "auto" : "none",
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
          <div className="agent-composer flex flex-col gap-2 p-2.5">
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
              className="w-full resize-none bg-transparent px-2.5 pt-2 text-[14px] leading-[1.5] text-foreground placeholder:text-muted-foreground/70 focus:outline-none disabled:cursor-not-allowed disabled:opacity-60"
            />

            <div className="flex items-center justify-between gap-2 px-1">
              <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground/80">
                <kbd className="rounded border border-border/70 bg-background/70 px-1.5 py-[1px] font-mono text-[10px] text-muted-foreground">
                  Enter
                </kbd>
                <span>to send</span>
                <span className="mx-1 text-muted-foreground/30">\u00b7</span>
                <kbd className="rounded border border-border/70 bg-background/70 px-1.5 py-[1px] font-mono text-[10px] text-muted-foreground">
                  Shift+Enter
                </kbd>
                <span>for a new line</span>
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
                  className="h-8 gap-1.5 rounded-full px-3"
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
                  className="h-8 w-8 rounded-full"
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
