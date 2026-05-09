import { useMemo, useState, type MouseEventHandler } from "react"
import { Check, Copy, ExternalLink } from "lucide-react"
import { ToolRegistry, type ToolProps } from "./tool-registry"
import { BasicTool } from "./basic-tool"
import { TextShimmer } from "./text-shimmer"
import { DiffChanges } from "./diff-changes"
import { Spinner } from "./spinner"
import { getFilename } from "@/lib/utils"

function getDirectory(filePath: string): string {
  const idx = Math.max(filePath.lastIndexOf("/"), filePath.lastIndexOf("\\"))
  return idx >= 0 ? filePath.slice(0, idx) : ""
}

function stripAnsi(str: string): string {
  return str.replace(/\x1b\[[0-9;]*m/g, "")
}

// ── Shell Submessage (bash tool description animation) ──
function ShellSubmessage({ text, animate }: { text: string; animate?: boolean }) {
  const [width, setWidth] = useState<string | undefined>(animate ? "0px" : undefined)
  const [blur, setBlur] = useState(animate)

  useMemo(() => {
    if (!animate) return
    requestAnimationFrame(() => {
      setWidth(undefined)
      setBlur(false)
    })
  }, [animate])

  return (
    <span data-component="shell-submessage">
      <span
        data-slot="shell-submessage-width"
        style={{ width, overflow: "hidden", whiteSpace: "nowrap", transition: "width 0.25s cubic-bezier(0.16,1,0.3,1)" }}
      >
        <span data-slot="basic-tool-tool-subtitle">
          <span
            data-slot="shell-submessage-value"
            style={{
              opacity: blur ? 0 : 1,
              filter: blur ? "blur(2px)" : undefined,
              transition: "opacity 0.32s ease, filter 0.32s ease",
            }}
          >
            {text}
          </span>
        </span>
      </span>
    </span>
  )
}

// ── READ ──
ToolRegistry.register({
  name: "read",
  render(props: ToolProps) {
    const input = props.input as { filePath?: string; offset?: number; limit?: number } | undefined
    const args: string[] = []
    if (input?.offset) args.push("offset=" + input.offset)
    if (input?.limit) args.push("limit=" + input.limit)
    const metadata = props.metadata as { loaded?: string[] } | undefined
    const loaded = props.status === "completed" && Array.isArray(metadata?.loaded) ? metadata!.loaded!.filter((p): p is string => typeof p === "string") : []
    return (
      <>
        <BasicTool
          icon="glasses"
          status={props.status}
          defaultOpen={props.defaultOpen}
          hideDetails={props.hideDetails}
          trigger={{
            title: "Read",
            subtitle: input?.filePath ? getFilename(input.filePath) : "",
            args,
          }}
        />
        {loaded.length > 0 && loaded.map((filepath) => (
          <div key={filepath} data-component="tool-loaded-file">
            <span>Loaded {filepath}</span>
          </div>
        ))}
      </>
    )
  },
})

// ── LIST ──
ToolRegistry.register({
  name: "list",
  render(props: ToolProps) {
    return (
      <BasicTool
        icon="bullet-list"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        trigger={{
          title: "List",
          subtitle: getDirectory((props.input as any)?.path || "/"),
        }}
      >
        {props.output && (
          <div data-component="tool-output" data-scrollable>
            <pre className="text-[13px] whitespace-pre-wrap">{props.output}</pre>
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── GLOB ──
ToolRegistry.register({
  name: "glob",
  render(props: ToolProps) {
    return (
      <BasicTool
        icon="magnifying-glass-menu"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        trigger={{
          title: "Glob",
          subtitle: getDirectory((props.input as any)?.path || "/"),
          args: (props.input as any)?.pattern ? ["pattern=" + (props.input as any).pattern] : [],
        }}
      >
        {props.output && (
          <div data-component="tool-output" data-scrollable>
            <pre className="text-[13px] whitespace-pre-wrap">{props.output}</pre>
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── GREP ──
ToolRegistry.register({
  name: "grep",
  render(props: ToolProps) {
    const args: string[] = []
    if ((props.input as any)?.pattern) args.push("pattern=" + (props.input as any).pattern)
    if ((props.input as any)?.include) args.push("include=" + (props.input as any).include)
    return (
      <BasicTool
        icon="magnifying-glass-menu"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        trigger={{
          title: "Grep",
          subtitle: getDirectory((props.input as any)?.path || "/"),
          args,
        }}
      >
        {props.output && (
          <div data-component="tool-output" data-scrollable>
            <pre className="text-[13px] whitespace-pre-wrap">{props.output}</pre>
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── WEBFETCH ──
ToolRegistry.register({
  name: "webfetch",
  render(props: ToolProps) {
    const pending = props.status === "pending" || props.status === "running"
    const url = typeof (props.input as any)?.url === "string" ? (props.input as any).url : ""
    return (
      <BasicTool
        icon="window-cursor"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails
        trigger={
          <div data-slot="basic-tool-tool-info-structured">
            <div data-slot="basic-tool-tool-info-main">
              <span data-slot="basic-tool-tool-title">
                <TextShimmer text="Web Fetch" active={pending} />
              </span>
              {!pending && url && (
                <a
                  data-slot="basic-tool-tool-subtitle"
                  href={url}
                  target="_blank"
                  rel="noopener noreferrer"
                  onClick={(e) => e.stopPropagation()}
                >
                  {url}
                </a>
              )}
            </div>
            {!pending && url && (
              <div data-component="tool-action">
                <ExternalLink className="h-3.5 w-3.5" />
              </div>
            )}
          </div>
        }
      />
    )
  },
})

// ── WEBSEARCH ──
ToolRegistry.register({
  name: "websearch",
  render(props: ToolProps) {
    const query = typeof (props.input as any)?.query === "string" ? (props.input as any).query : ""
    const provider = (props.metadata as any)?.provider
    const title = provider === "parallel" ? "Parallel Web Search" : provider === "exa" ? "Exa Web Search" : "Web Search"
    return (
      <BasicTool
        icon="window-cursor"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        trigger={{
          title,
          subtitle: query,
        }}
      />
    )
  },
})

// ── TASK ──
ToolRegistry.register({
  name: "task",
  render(props: ToolProps) {
    const agent = typeof (props.input as any)?.subagent_type === "string" ? (props.input as any).subagent_type : undefined
    const title = agent ? agent[0].toUpperCase() + agent.slice(1) : "Agent"
    const description = typeof (props.input as any)?.description === "string" ? (props.input as any).description : undefined
    const sessionID = typeof props.metadata?.sessionId === "string"
      ? props.metadata.sessionId
      : typeof props.metadata?.sessionID === "string"
        ? props.metadata.sessionID
        : undefined
    const subtitle = description || sessionID
    const tone = typeof props.metadata?.color === "string" ? props.metadata.color : undefined
    const running = props.status === "pending" || props.status === "running"
    const clickable = Boolean(sessionID)
    const openSubagent: MouseEventHandler = (event) => {
      if (!sessionID) return
      event.preventDefault()
      event.stopPropagation()
      window.dispatchEvent(new CustomEvent("shob:open-opencode-session", {
        detail: { sessionID, title },
      }))
    }

    const trigger = (
      <div data-component="task-tool-card">
        <div data-slot="basic-tool-tool-info-structured">
          <div data-slot="basic-tool-tool-info-main">
            {running && (
              <span data-component="task-tool-spinner" style={{ color: tone ?? "var(--icon-interactive-base)" }}>
                <Spinner />
              </span>
            )}
            <span data-component="task-tool-title" style={{ color: tone ?? "var(--text-strong)" }}>
              {title}
            </span>
            {subtitle && (
              <span data-slot="basic-tool-tool-subtitle">{subtitle}</span>
            )}
          </div>
        </div>
        {clickable && (
          <div data-component="task-tool-action" title={sessionID}>
            <ExternalLink className="h-3.5 w-3.5" />
          </div>
        )}
      </div>
    )

    return (
      <BasicTool
        icon="task"
        status={props.status}
        trigger={trigger}
        hideDetails
        clickable={clickable}
        onTriggerClick={clickable ? openSubagent : undefined}
      />
    )
  },
})

// ── BASH ──
ToolRegistry.register({
  name: "bash",
  render(props: ToolProps) {
    const pending = props.status === "pending" || props.status === "running"
    const sawPending = pending
    const command = (props.input as any)?.command ?? (props.metadata as any)?.command ?? ""
    const output = stripAnsi(props.output || (props.metadata as any)?.output || "").replace(/\r\n?/g, "\n")
    const description = (props.input as any)?.description
    const text = `$ ${command}${output ? "\n\n" + output : ""}`
    const [copied, setCopied] = useState(false)

    const handleCopy = async () => {
      if (!text) return
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    }

    return (
      <BasicTool
        icon="console"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        trigger={
          <div data-slot="basic-tool-tool-info-structured">
            <div data-slot="basic-tool-tool-info-main">
              <span data-slot="basic-tool-tool-title">
                <TextShimmer text="Shell" active={pending} />
              </span>
              {!pending && description && (
                <ShellSubmessage text={description} animate={sawPending} />
              )}
            </div>
          </div>
        }
      >
        {text && (
          <div data-component="bash-output">
            <div data-slot="bash-copy">
              <button
                onClick={(e) => { e.stopPropagation(); handleCopy() }}
                title={copied ? "Copied" : "Copy"}
                className="text-muted-foreground hover:text-foreground"
              >
                {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
              </button>
            </div>
            <div data-slot="bash-scroll" data-scrollable>
              <pre data-slot="bash-pre">
                <code>{text}</code>
              </pre>
            </div>
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── EDIT ──
ToolRegistry.register({
  name: "edit",
  render(props: ToolProps) {
    const input = props.input as { filePath?: string; oldString?: string; newString?: string; content?: string } | undefined
    const metadata = props.metadata as { filediff?: { file: string; before: string; after: string; additions?: number; deletions?: number }; error?: string; diagnostics?: any } | undefined
    const filename = input?.filePath ? getFilename(input.filePath) : ""
    const pending = props.status === "pending" || props.status === "running"
    const diff = metadata?.filediff
    const error = metadata?.error

    return (
      <BasicTool
        icon="code-lines"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        defer
        trigger={
          <div data-component="edit-trigger">
            <div data-slot="message-part-title-area">
              <div data-slot="message-part-title">
                <span data-slot="message-part-title-text">
                  <TextShimmer text="Edit" active={pending} />
                </span>
                {!pending && filename && (
                  <span data-slot="message-part-title-filename">{filename}</span>
                )}
              </div>
              {!pending && input?.filePath?.includes("/") && (
                <div data-slot="message-part-path">
                  <span data-slot="message-part-directory">{getDirectory(input.filePath)}</span>
                </div>
              )}
            </div>
            <div data-slot="message-part-actions">
              {!pending && diff && (
                <DiffChanges changes={{ additions: diff.additions ?? 0, deletions: diff.deletions ?? 0 }} />
              )}
            </div>
          </div>
        }
      >
        {error && (
          <div className="text-[11px] text-destructive px-2 pb-1">{error}</div>
        )}
        {diff && (
          <div data-component="edit-content" className="p-2">
            <pre className="thin-scrollbar overflow-auto whitespace-pre-wrap text-[11px] leading-5 rounded-md border border-border/60 bg-background/55 p-2 font-mono">
              {diff.before !== diff.after ? (
                <>
                  <span className="text-destructive">{diff.before}</span>
                  <span className="mx-1">→</span>
                  <span className="text-success">{diff.after}</span>
                </>
              ) : (
                diff.before
              )}
            </pre>
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── WRITE ──
ToolRegistry.register({
  name: "write",
  render(props: ToolProps) {
    const input = props.input as { filePath?: string; content?: string } | undefined
    const metadata = props.metadata as { error?: string; diagnostics?: any } | undefined
    const filename = input?.filePath ? getFilename(input.filePath) : ""
    const isPending = props.status === "pending" || props.status === "running"

    return (
      <BasicTool
        icon="code-lines"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        defer
        trigger={
          <div data-component="write-trigger">
            <div data-slot="message-part-title-area">
              <div data-slot="message-part-title">
                <span data-slot="message-part-title-text">
                  <TextShimmer text="Write" active={isPending} />
                </span>
                {!isPending && filename && (
                  <span data-slot="message-part-title-filename">{filename}</span>
                )}
              </div>
              {!isPending && input?.filePath?.includes("/") && (
                <div data-slot="message-part-path">
                  <span data-slot="message-part-directory">{getDirectory(input.filePath)}</span>
                </div>
              )}
            </div>
            <div data-slot="message-part-actions" />
          </div>
        }
      >
        {metadata?.error && (
          <div className="text-[11px] text-destructive px-2 pb-1">{metadata.error as string}</div>
        )}
        {input?.content && (
          <div className="p-2">
            <pre className="thin-scrollbar overflow-auto whitespace-pre-wrap text-[11px] leading-5 rounded-md border border-border/60 bg-background/55 p-2">
              {input.content}
            </pre>
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── APPLY_PATCH ──
ToolRegistry.register({
  name: "apply_patch",
  render(props: ToolProps) {
    const metadata = props.metadata as { files?: Array<{ filePath: string; type: "add" | "delete" | "modify"; additions?: number; deletions?: number }> } | undefined
    const files = metadata?.files ?? []
    const subtitle = files.length > 0 ? `${files.length} ${files.length === 1 ? "file" : "files"}` : ""

    return (
      <BasicTool
        icon="code-lines"
        status={props.status}
        defaultOpen={props.defaultOpen}
        hideDetails={props.hideDetails}
        trigger={{
          title: "Patch",
          subtitle,
        }}
      >
        {files.length > 0 && (
          <div className="px-2 pb-2 space-y-1">
            {files.map((f) => (
              <div key={f.filePath} className="flex items-center gap-2 text-[12px] text-muted-foreground">
                <span className="flex-1 truncate">{getFilename(f.filePath)}</span>
                {f.type === "add" && <span className="text-green-500 text-[10px] uppercase">Created</span>}
                {f.type === "delete" && <span className="text-red-500 text-[10px] uppercase">Deleted</span>}
                {f.type === "modify" && f.additions !== undefined && f.deletions !== undefined && (
                  <DiffChanges changes={{ additions: f.additions, deletions: f.deletions }} />
                )}
              </div>
            ))}
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── TODOWRITE ── (hidden from chat — todos are shown in the bottom TodoDock only)
ToolRegistry.register({
  name: "todowrite",
  render(_props: ToolProps) {
    return null
  },
})

// ── QUESTION ──
ToolRegistry.register({
  name: "question",
  render(props: ToolProps) {
    const questions = Array.isArray((props.input as any)?.questions) ? (props.input as any).questions as Array<{ question: string }> : []
    const answers = Array.isArray((props.metadata as any)?.answers) ? (props.metadata as any).answers as Array<string[]> : []
    const completed = answers.length > 0
    const count = questions.length
    const subtitle = completed ? `Answered ${count}` : `${count} question${count > 1 ? "s" : ""}`

    return (
      <BasicTool
        icon="bubble-5"
        status={props.status}
        defaultOpen={completed}
        hideDetails={props.hideDetails}
        trigger={{
          title: "Questions",
          subtitle,
        }}
      >
        {completed && (
          <div data-component="question-answers" className="px-2 pb-2 space-y-2">
            {questions.map((q, i) => (
              <div key={i} data-slot="question-answer-item">
                <div data-slot="question-text">{q.question}</div>
                <div data-slot="answer-text">{answers[i]?.join(", ") || "No answer"}</div>
              </div>
            ))}
          </div>
        )}
      </BasicTool>
    )
  },
})

// ── SKILL ──
ToolRegistry.register({
  name: "skill",
  render(props: ToolProps) {
    const title = typeof (props.input as any)?.name === "string" ? (props.input as any).name : "Skill"
    const running = props.status === "pending" || props.status === "running"

    const trigger = (
      <div data-slot="basic-tool-tool-info-structured">
        <div data-slot="basic-tool-tool-info-main">
          <span data-slot="basic-tool-tool-title" className="capitalize">
            <TextShimmer text={title} active={running} />
          </span>
        </div>
      </div>
    )

    return (
      <BasicTool
        icon="brain"
        status={props.status}
        trigger={trigger}
        hideDetails
      />
    )
  },
})

// ── Export a fallback for tools that might be rendered outside the Registry ──
export { ToolRegistry }
