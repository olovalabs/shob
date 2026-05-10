import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type JSX,
} from "react"
import stripAnsi from "strip-ansi"
import {
  AgentPart,
  AssistantMessage,
  FilePart,
  Message as MessageType,
  Part as PartType,
  ReasoningPart,
  Session,
  TextPart,
  ToolPart,
  UserMessage,
  Todo,
  QuestionAnswer,
  QuestionInfo,
} from "@shob/sdk/v2"
import { useData } from "../context"
import { useFileComponent } from "../context/file"
import { useDialog } from "../context/dialog"
import { type UiI18n, useI18n } from "../context/i18n"
import { BasicTool, GenericTool } from "./basic-tool"
import { Accordion } from "./accordion"
import { StickyAccordionHeader } from "./sticky-accordion-header"
import { Collapsible } from "./collapsible"
import { FileIcon } from "./file-icon"
import { Icon } from "./icon"
import { ToolErrorCard } from "./tool-error-card"
import { Checkbox } from "./checkbox"
import { DiffChanges } from "./diff-changes"
import { Markdown } from "./markdown"
import { ImagePreview } from "./image-preview"
import { getDirectory as _getDirectory, getFilename } from "@opencode-ai/core/util/path"
import { checksum } from "../lib/encode"
import { Tooltip } from "./tooltip"
import { IconButton } from "./icon-button"
import { Spinner } from "./spinner"
import { TextShimmer } from "./text-shimmer"
import { AnimatedCountList } from "./tool-count-summary"
import { ToolStatusTitle } from "./tool-status-title"
import { patchFiles } from "./apply-patch-file"
import { animate } from "motion"
import { attached, inline, kind } from "./message-file"

const getPathname = () => (typeof window !== "undefined" ? window.location.pathname : "")

function ShellSubmessage(props: { text: string; animate?: boolean }) {
  let widthRef: HTMLSpanElement | undefined
  let valueRef: HTMLSpanElement | undefined

  useEffect(() => {
    if (!props.animate) return
    requestAnimationFrame(() => {
      if (widthRef) {
        animate(widthRef, { width: "auto" }, { type: "spring", visualDuration: 0.25, bounce: 0 })
      }
      if (valueRef) {
        animate(valueRef, { opacity: 1, filter: "blur(0px)" }, { duration: 0.32, ease: [0.16, 1, 0.3, 1] })
      }
    })
  })

  return (
    <span data-component="shell-submessage">
      <span ref={(el) => { widthRef = el! }} data-slot="shell-submessage-width" style={{ width: props.animate ? "0px" : undefined }}>
        <span data-slot="basic-tool-tool-subtitle">
          <span
            ref={(el) => { valueRef = el! }}
            data-slot="shell-submessage-value"
            style={props.animate ? { opacity: 0, filter: "blur(2px)" } : undefined}
          >
            {props.text}
          </span>
        </span>
      </span>
    </span>
  )
}

interface Diagnostic {
  range: {
    start: { line: number; character: number }
    end: { line: number; character: number }
  }
  message: string
  severity?: number
}

function getDiagnostics(
  diagnosticsByFile: Record<string, Diagnostic[]> | undefined,
  filePath: string | undefined,
): Diagnostic[] {
  if (!diagnosticsByFile || !filePath) return []
  const diagnostics = diagnosticsByFile[filePath] ?? []
  return diagnostics.filter((d) => d.severity === 1).slice(0, 3)
}

function DiagnosticsDisplay(props: { diagnostics: Diagnostic[] }): JSX.Element {
  const i18n = useI18n()
  return props.diagnostics.length > 0 ? (
    <div data-component="diagnostics">
      {props.diagnostics.map((diagnostic) => (
        <div key={diagnostic.range.start.line + ":" + diagnostic.range.start.character} data-slot="diagnostic">
          <span data-slot="diagnostic-label">{i18n.t("ui.messagePart.diagnostic.error")}</span>
          <span data-slot="diagnostic-location">
            [{diagnostic.range.start.line + 1}:{diagnostic.range.start.character + 1}]
          </span>
          <span data-slot="diagnostic-message">{diagnostic.message}</span>
        </div>
      ))}
    </div>
  ) : null
}

export interface MessageProps {
  message: MessageType
  parts: PartType[]
  actions?: UserActions
  showAssistantCopyPartID?: string | null
  showReasoningSummaries?: boolean
}

export type SessionAction = (input: { sessionID: string; messageID: string }) => Promise<void> | void

export type UserActions = {
  fork?: SessionAction
  revert?: SessionAction
}

export interface MessagePartProps {
  part: PartType
  message: MessageType
  hideDetails?: boolean
  defaultOpen?: boolean
  showAssistantCopyPartID?: string | null
  turnDurationMs?: number
}

export type PartComponent = React.FC<MessagePartProps>

export const PART_MAPPING: Record<string, PartComponent | undefined> = {}

const TEXT_RENDER_PACE_MS = 24
const TEXT_RENDER_SNAP = /[\s.,!?;:)\]]/

function step(size: number) {
  if (size <= 12) return 2
  if (size <= 48) return 4
  if (size <= 96) return 8
  return Math.min(24, Math.ceil(size / 8))
}

function next(text: string, start: number) {
  const end = Math.min(text.length, start + step(text.length - start))
  const max = Math.min(text.length, end + 8)
  for (let i = end; i < max; i++) {
    if (TEXT_RENDER_SNAP.test(text[i] ?? "")) return i + 1
  }
  return end
}

function createPacedValue(getValue: () => string, live?: () => boolean) {
  const [value, setValue] = useState(getValue())
  const shownRef = useRef(getValue())
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)

  const clear = () => {
    if (!timeoutRef.current) return
    clearTimeout(timeoutRef.current)
    timeoutRef.current = undefined
  }

  const sync = (text: string) => {
    shownRef.current = text
    setValue(text)
  }

  const run = () => {
    timeoutRef.current = undefined
    const text = getValue()
    if (!live?.()) {
      sync(text)
      return
    }
    if (!text.startsWith(shownRef.current) || text.length <= shownRef.current.length) {
      sync(text)
      return
    }
    const end = next(text, shownRef.current.length)
    sync(text.slice(0, end))
    if (end < text.length) timeoutRef.current = setTimeout(run, TEXT_RENDER_PACE_MS)
  }

  useEffect(() => {
    const text = getValue()
    if (!live?.()) {
      clear()
      sync(text)
      return
    }
    if (!text.startsWith(shownRef.current) || text.length < shownRef.current.length) {
      clear()
      sync(text)
      return
    }
    if (text.length === shownRef.current.length || timeoutRef.current) return
    timeoutRef.current = setTimeout(run, TEXT_RENDER_PACE_MS)
  })

  useEffect(() => {
    return () => clear()
  }, [])

  return value
}

function PacedMarkdown(props: { text: string; cacheKey: string; streaming: boolean }) {
  const value = createPacedValue(
    () => props.text,
    () => props.streaming,
  )

  return value ? (
    <Markdown text={value} cacheKey={props.cacheKey} streaming={props.streaming} />
  ) : null
}

function relativizeProjectPath(path: string, directory?: string) {
  if (!path) return ""
  if (!directory) return path
  if (directory === "/") return path
  if (directory === "\\") return path
  if (path === directory) return ""

  const separator = directory.includes("\\") ? "\\" : "/"
  const prefix = directory.endsWith(separator) ? directory : directory + separator
  if (!path.startsWith(prefix)) return path
  return path.slice(directory.length)
}

function getDirectory(path: string | undefined) {
  const data = useData()
  return relativizeProjectPath(_getDirectory(path), data.directory)
}

import type { IconProps } from "./icon"

export type ToolInfo = {
  icon: IconProps["name"]
  title: string
  subtitle?: string
}

function agentTitle(i18n: UiI18n, type?: string) {
  if (!type) return i18n.t("ui.tool.agent.default")
  return i18n.t("ui.tool.agent", { type })
}

const agentTones: Record<string, string> = {
  ask: "var(--icon-agent-ask-base)",
  build: "var(--icon-agent-build-base)",
  docs: "var(--icon-agent-docs-base)",
  plan: "var(--icon-agent-plan-base)",
}

const agentPalette = [
  "var(--icon-agent-ask-base)",
  "var(--icon-agent-build-base)",
  "var(--icon-agent-docs-base)",
  "var(--icon-agent-plan-base)",
  "var(--syntax-info)",
  "var(--syntax-success)",
  "var(--syntax-warning)",
  "var(--syntax-property)",
  "var(--syntax-constant)",
  "var(--text-diff-add-base)",
  "var(--text-diff-delete-base)",
  "var(--icon-warning-base)",
]

function tone(name: string) {
  let hash = 0
  for (const char of name) hash = (hash * 31 + char.charCodeAt(0)) >>> 0
  return agentPalette[hash % agentPalette.length]
}

function taskAgent(
  raw: unknown,
  list?: readonly { name: string; color?: string }[],
): { name?: string; color?: string } {
  if (typeof raw !== "string" || !raw) return {}
  const key = raw.toLowerCase()
  const item = list?.find((entry) => entry.name === raw || entry.name.toLowerCase() === key)
  return {
    name: item?.name ?? `${raw[0]!.toUpperCase()}${raw.slice(1)}`,
    color: item?.color ?? agentTones[key] ?? tone(key),
  }
}

function webSearchProviderLabel(provider: unknown) {
  if (provider === "parallel") return "Parallel Web Search"
  if (provider === "exa") return "Exa Web Search"
  return "Web Search"
}

export function getToolInfo(
  tool: string,
  input: any = {},
  metadata: Record<string, unknown> | undefined = {},
): ToolInfo {
  const i18n = useI18n()
  switch (tool) {
    case "read":
      return {
        icon: "glasses",
        title: i18n.t("ui.tool.read"),
        subtitle: input.filePath ? getFilename(input.filePath) : undefined,
      }
    case "list":
      return {
        icon: "bullet-list",
        title: i18n.t("ui.tool.list"),
        subtitle: input.path ? getFilename(input.path) : undefined,
      }
    case "glob":
      return {
        icon: "magnifying-glass-menu",
        title: i18n.t("ui.tool.glob"),
        subtitle: input.pattern,
      }
    case "grep":
      return {
        icon: "magnifying-glass-menu",
        title: i18n.t("ui.tool.grep"),
        subtitle: input.pattern,
      }
    case "webfetch":
      return {
        icon: "window-cursor",
        title: i18n.t("ui.tool.webfetch"),
        subtitle: input.url,
      }
    case "websearch":
      return {
        icon: "window-cursor",
        title: webSearchProviderLabel(metadata?.provider),
        subtitle: input.query,
      }
    case "task": {
      const type =
        typeof input.subagent_type === "string" && input.subagent_type
          ? input.subagent_type[0]!.toUpperCase() + input.subagent_type.slice(1)
          : undefined
      return {
        icon: "task",
        title: agentTitle(i18n, type),
        subtitle: input.description,
      }
    }
    case "bash":
      return {
        icon: "console",
        title: i18n.t("ui.tool.shell"),
        subtitle: input.description,
      }
    case "edit":
      return {
        icon: "code-lines",
        title: i18n.t("ui.messagePart.title.edit"),
        subtitle: input.filePath ? getFilename(input.filePath) : undefined,
      }
    case "write":
      return {
        icon: "code-lines",
        title: i18n.t("ui.messagePart.title.write"),
        subtitle: input.filePath ? getFilename(input.filePath) : undefined,
      }
    case "apply_patch":
      return {
        icon: "code-lines",
        title: i18n.t("ui.tool.patch"),
        subtitle: input.files?.length
          ? `${input.files.length} ${i18n.t(input.files.length > 1 ? "ui.common.file.other" : "ui.common.file.one")}`
          : undefined,
      }
    case "todowrite":
      return {
        icon: "checklist",
        title: i18n.t("ui.tool.todos"),
      }
    case "question":
      return {
        icon: "bubble-5",
        title: i18n.t("ui.tool.questions"),
      }
    case "skill":
      return {
        icon: "brain",
        title: input.name || i18n.t("ui.tool.skill"),
      }
    default:
      return {
        icon: "mcp",
        title: tool,
      }
  }
}

function urls(text: string | undefined) {
  if (!text) return []
  const seen = new Set<string>()
  return [...text.matchAll(/https?:\/\/[^\s<>"'`)\]]+/g)]
    .map((item) => item[0].replace(/[),.;:!?]+$/g, ""))
    .filter((item) => {
      if (seen.has(item)) return false
      seen.add(item)
      return true
    })
}

function sessionLink(id: string | undefined, path: string, href?: (id: string) => string | undefined) {
  if (!id) return

  const direct = href?.(id)
  if (direct) return direct

  const idx = path.indexOf("/session")
  if (idx === -1) return
  return `${path.slice(0, idx)}/session/${id}`
}

function currentSession(path: string) {
  return path.match(/\/session\/([^/?#]+)/)?.[1]
}

function taskSession(
  input: Record<string, any>,
  path: string,
  sessions: Session[] | undefined,
  agents?: readonly { name: string; color?: string }[],
) {
  const parentID = currentSession(path)
  if (!parentID) return
  const description = typeof input.description === "string" ? input.description : ""
  const agent = taskAgent(input.subagent_type, agents).name
  return (sessions ?? [])
    .filter((session) => session.parentID === parentID && !session.time?.archived)
    .filter((session) => (description ? session.title.startsWith(description) : true))
    .filter((session) => (agent ? session.title.includes(`@${agent}`) : true))
    .sort((a, b) => (b.time.created ?? 0) - (a.time.created ?? 0))[0]?.id
}

const CONTEXT_GROUP_TOOLS = new Set(["read", "glob", "grep", "list"])
const HIDDEN_TOOLS = new Set(["todowrite"])

function list<T>(value: T[] | undefined | null, fallback: T[]) {
  if (Array.isArray(value)) return value
  return fallback
}

function same<T>(a: readonly T[] | undefined, b: readonly T[] | undefined) {
  if (a === b) return true
  if (!a || !b) return false
  if (a.length !== b.length) return false
  return a.every((x, i) => x === b[i])
}

type PartRef = {
  messageID: string
  partID: string
}

type PartGroup =
  | {
      key: string
      type: "part"
      ref: PartRef
    }
  | {
      key: string
      type: "context"
      refs: PartRef[]
    }

function sameRef(a: PartRef, b: PartRef) {
  return a.messageID === b.messageID && a.partID === b.partID
}

function sameGroup(a: PartGroup, b: PartGroup) {
  if (a === b) return true
  if (a.key !== b.key) return false
  if (a.type !== b.type) return false
  if (a.type === "part") {
    if (b.type !== "part") return false
    return sameRef(a.ref, b.ref)
  }
  if (b.type !== "context") return false
  if (a.refs.length !== b.refs.length) return false
  return a.refs.every((ref, i) => sameRef(ref, b.refs[i]!))
}

function sameGroups(a: readonly PartGroup[] | undefined, b: readonly PartGroup[] | undefined) {
  if (a === b) return true
  if (!a || !b) return false
  if (a.length !== b.length) return false
  return a.every((item, i) => sameGroup(item, b[i]!))
}

function groupParts(parts: { messageID: string; part: PartType }[]) {
  const result: PartGroup[] = []
  let start = -1

  const flush = (end: number) => {
    if (start < 0) return
    const first = parts[start]
    const last = parts[end]
    if (!first || !last) {
      start = -1
      return
    }
    result.push({
      key: `context:${first.part.id}`,
      type: "context",
      refs: parts.slice(start, end + 1).map((item) => ({
        messageID: item.messageID,
        partID: item.part.id,
      })),
    })
    start = -1
  }

  parts.forEach((item, index) => {
    if (isContextGroupTool(item.part)) {
      if (start < 0) start = index
      return
    }

    flush(index - 1)
    result.push({
      key: `part:${item.messageID}:${item.part.id}`,
      type: "part",
      ref: {
        messageID: item.messageID,
        partID: item.part.id,
      },
    })
  })

  flush(parts.length - 1)
  return result
}

function index<T extends { id: string }>(items: readonly T[]) {
  return new Map(items.map((item) => [item.id, item] as const))
}

function renderable(part: PartType, showReasoningSummaries = true) {
  if (part.type === "tool") {
    if (HIDDEN_TOOLS.has(part.tool)) return false
    if (part.tool === "question") return part.state.status !== "pending" && part.state.status !== "running"
    return true
  }
  if (part.type === "text") return !!part.text?.trim()
  if (part.type === "reasoning") return showReasoningSummaries && !!part.text?.trim()
  return !!PART_MAPPING[part.type]
}

function toolDefaultOpen(tool: string, shell = false, edit = false) {
  if (tool === "bash") return shell
  if (tool === "edit" || tool === "write" || tool === "apply_patch") return edit
}

function partDefaultOpen(part: PartType, shell = false, edit = false) {
  if (part.type !== "tool") return
  return toolDefaultOpen(part.tool, shell, edit)
}

function ContextToolItem(props: { part: ToolPart; i18n: ReturnType<typeof useI18n> }) {
  const trigger = useMemo(() => contextToolTrigger(props.part, props.i18n), [props.part, props.i18n])
  const running = useMemo(
    () => props.part.state.status === "pending" || props.part.state.status === "running",
    [props.part.state.status],
  )
  return (
    <div data-slot="context-tool-group-item">
      <div data-component="tool-trigger">
        <div data-slot="basic-tool-tool-trigger-content">
          <div data-slot="basic-tool-tool-info">
            <div data-slot="basic-tool-tool-info-structured">
              <div data-slot="basic-tool-tool-info-main">
                <span data-slot="basic-tool-tool-title">
                  <TextShimmer text={trigger.title} active={running} />
                </span>
                {!running && trigger.subtitle && (
                  <span data-slot="basic-tool-tool-subtitle">{trigger.subtitle}</span>
                )}
                {!running && trigger.args?.length && trigger.args.map((arg, i) => (
                  <span key={i} data-slot="basic-tool-tool-arg">{arg}</span>
                ))}
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

function AssistantPartsGroupEntry(props: {
  entry: PartGroup
  msgs: Map<string, AssistantMessage>
  partMap: Map<string, Map<string, PartType>>
  working?: boolean
  last?: string
  showAssistantCopyPartID?: string | null
  turnDurationMs?: number
  shellToolDefaultOpen?: boolean
  editToolDefaultOpen?: boolean
}) {
  const entry = props.entry
  const emptyTools: ToolPart[] = []

  if (entry.type === "context") {
    const contextParts = useMemo(
      () => {
        return entry.refs
          .map((ref) => props.partMap.get(ref.messageID)?.get(ref.partID))
          .filter((p): p is ToolPart => !!p && isContextGroupTool(p))
      },
      [entry, props.partMap],
    )
    const busy = !!(props.working && props.last === entry.key)

    return contextParts.length > 0 ? <ContextToolGroup parts={contextParts} busy={busy} /> : null
  }

  if (entry.type === "part") {
    const message = useMemo(
      () => props.msgs.get(entry.ref.messageID),
      [props.msgs, entry.ref.messageID],
    )
    const item = useMemo(
      () => props.partMap.get(entry.ref.messageID)?.get(entry.ref.partID),
      [props.partMap, entry.ref.messageID, entry.ref.partID],
    )

    return message && item ? (
      <Part
        part={item}
        message={message}
        showAssistantCopyPartID={props.showAssistantCopyPartID}
        turnDurationMs={props.turnDurationMs}
        defaultOpen={partDefaultOpen(item, props.shellToolDefaultOpen, props.editToolDefaultOpen)}
      />
    ) : null
  }

  return null
}

function AssistantMessageGroupEntry(props: {
  entry: PartGroup
  partMap: Map<string, PartType>
  message: AssistantMessage
  showAssistantCopyPartID?: string | null
  turnDurationMs?: number
  shellToolDefaultOpen?: boolean
  editToolDefaultOpen?: boolean
}) {
  const entry = props.entry
  const emptyTools: ToolPart[] = []

  if (entry.type === "context") {
    const contextParts = useMemo(
      () => {
        return entry.refs
          .map((ref) => props.partMap.get(ref.partID))
          .filter((p): p is ToolPart => !!p && isContextGroupTool(p))
      },
      [entry, props.partMap],
    )

    return contextParts.length > 0 ? <ContextToolGroup parts={contextParts} /> : null
  }

  if (entry.type === "part") {
    const item = useMemo(
      () => props.partMap.get(entry.ref.partID),
      [props.partMap, entry.ref.partID],
    )

    return item ? (
      <Part
        part={item}
        message={props.message}
        showAssistantCopyPartID={props.showAssistantCopyPartID}
      />
    ) : null
  }

  return null
}

function PatchFileItem(props: {
  file: { filePath: string; relativePath: string; type: string; additions: number; deletions: number; view: { fileDiff: any } }
  expanded: string[]
  fileComponent: React.ComponentType<any>
  i18n: ReturnType<typeof useI18n>
  setExpanded: React.Dispatch<React.SetStateAction<string[]>>
}) {
  const { file, expanded, setExpanded, fileComponent: FileComponent, i18n } = props
  const isActive = expanded.includes(file.filePath)
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    if (!isActive) {
      setVisible(false)
      return
    }

    const id = requestAnimationFrame(() => {
      if (!expanded.includes(file.filePath)) return
      setVisible(true)
    })
    return () => cancelAnimationFrame(id)
  }, [isActive])

  return (
    <Accordion.Item value={file.filePath} data-type={file.type}>
      <StickyAccordionHeader>
        <Accordion.Trigger>
          <div data-slot="apply-patch-trigger-content">
            <div data-slot="apply-patch-file-info">
              <FileIcon node={{ path: file.relativePath, type: "file" }} />
              <div data-slot="apply-patch-file-name-container">
                {file.relativePath.includes("/") && (
                  <span data-slot="apply-patch-directory">{`\u202A${getDirectory(file.relativePath)}\u202C`}</span>
                )}
                <span data-slot="apply-patch-filename">{getFilename(file.relativePath)}</span>
              </div>
            </div>
            <div data-slot="apply-patch-trigger-actions">
              {file.type === "add" ? (
                <span data-slot="apply-patch-change" data-type="added">
                  {i18n.t("ui.patch.action.created")}
                </span>
              ) : file.type === "delete" ? (
                <span data-slot="apply-patch-change" data-type="removed">
                  {i18n.t("ui.patch.action.deleted")}
                </span>
              ) : file.type === "move" ? (
                <span data-slot="apply-patch-change" data-type="modified">
                  {i18n.t("ui.patch.action.moved")}
                </span>
              ) : (
                <DiffChanges changes={{ additions: file.additions, deletions: file.deletions }} />
              )}
              <Icon name="chevron-grabber-vertical" size="small" />
            </div>
          </div>
        </Accordion.Trigger>
      </StickyAccordionHeader>
      <Accordion.Content>
        {visible && (
          <div data-component="apply-patch-file-diff">
            <FileComponent mode="diff" fileDiff={file.view.fileDiff} />
          </div>
        )}
      </Accordion.Content>
    </Accordion.Item>
  )
}

export function AssistantParts(props: {
  messages: AssistantMessage[]
  showAssistantCopyPartID?: string | null
  turnDurationMs?: number
  working?: boolean
  showReasoningSummaries?: boolean
  shellToolDefaultOpen?: boolean
  editToolDefaultOpen?: boolean
}) {
  const data = useData()
  const emptyParts: PartType[] = []
  const emptyTools: ToolPart[] = []
  const msgs = useMemo(() => index(props.messages), [props.messages])
  const partMap = useMemo(
    () =>
      new Map(
        props.messages.map((message) => [message.id, index(list(data.store.part?.[message.id], emptyParts))] as const),
      ),
    [props.messages, data.store.part],
  )

  const grouped = useMemo(
    () =>
      groupParts(
        props.messages.flatMap((message) =>
          list(data.store.part?.[message.id], emptyParts)
            .filter((part) => renderable(part, props.showReasoningSummaries ?? true))
            .map((part) => ({
              messageID: message.id,
              part,
            })),
        ),
      ),
    [props.messages, data.store.part, props.showReasoningSummaries],
  )

  const last = useMemo(() => grouped.at(-1)?.key, [grouped])

  return (
    <>
      {grouped.map((entry) => (
        <AssistantPartsGroupEntry
          key={entry.key}
          entry={entry}
          msgs={msgs}
          partMap={partMap}
          working={props.working}
          last={last}
          showAssistantCopyPartID={props.showAssistantCopyPartID}
          turnDurationMs={props.turnDurationMs}
          shellToolDefaultOpen={props.shellToolDefaultOpen}
          editToolDefaultOpen={props.editToolDefaultOpen}
        />
      ))}
    </>
  )
}

function isContextGroupTool(part: PartType): part is ToolPart {
  return part.type === "tool" && CONTEXT_GROUP_TOOLS.has(part.tool)
}

function contextToolDetail(part: ToolPart): string | undefined {
  const info = getToolInfo(
    part.tool,
    part.state.input ?? {},
    "metadata" in part.state ? part.state.metadata : undefined,
  )
  if (info.subtitle) return info.subtitle
  if (part.state.status === "error") return part.state.error
  if ((part.state.status === "running" || part.state.status === "completed") && part.state.title)
    return part.state.title
  const description = part.state.input?.description
  if (typeof description === "string") return description
  return undefined
}

function contextToolTrigger(part: ToolPart, i18n: ReturnType<typeof useI18n>) {
  const input = (part.state.input ?? {}) as Record<string, unknown>
  const path = typeof input.path === "string" ? input.path : "/"
  const filePath = typeof input.filePath === "string" ? input.filePath : undefined
  const pattern = typeof input.pattern === "string" ? input.pattern : undefined
  const include = typeof input.include === "string" ? input.include : undefined
  const offset = typeof input.offset === "number" ? input.offset : undefined
  const limit = typeof input.limit === "number" ? input.limit : undefined

  switch (part.tool) {
    case "read": {
      const args: string[] = []
      if (offset !== undefined) args.push("offset=" + offset)
      if (limit !== undefined) args.push("limit=" + limit)
      return {
        title: i18n.t("ui.tool.read"),
        subtitle: filePath ? getFilename(filePath) : "",
        args,
      }
    }
    case "list":
      return {
        title: i18n.t("ui.tool.list"),
        subtitle: getDirectory(path),
      }
    case "glob":
      return {
        title: i18n.t("ui.tool.glob"),
        subtitle: getDirectory(path),
        args: pattern ? ["pattern=" + pattern] : [],
      }
    case "grep": {
      const args: string[] = []
      if (pattern) args.push("pattern=" + pattern)
      if (include) args.push("include=" + include)
      return {
        title: i18n.t("ui.tool.grep"),
        subtitle: getDirectory(path),
        args,
      }
    }
    default: {
      const info = getToolInfo(part.tool, input, "metadata" in part.state ? part.state.metadata : undefined)
      return {
        title: info.title,
        subtitle: info.subtitle || contextToolDetail(part),
        args: [],
      }
    }
  }
}

function contextToolSummary(parts: ToolPart[]) {
  const read = parts.filter((part) => part.tool === "read").length
  const search = parts.filter((part) => part.tool === "glob" || part.tool === "grep").length
  const list = parts.filter((part) => part.tool === "list").length
  return { read, search, list }
}

function ExaOutput(props: { output?: string }) {
  const links = useMemo(() => urls(props.output), [props.output])

  return links.length > 0 ? (
    <div data-component="exa-tool-output">
      <div data-slot="exa-tool-links">
        {links.map((url) => (
          <a
            key={url}
            data-slot="exa-tool-link"
            href={url}
            target="_blank"
            rel="noopener noreferrer"
            onClick={(event) => event.stopPropagation()}
          >
            {url}
          </a>
        ))}
      </div>
    </div>
  ) : null
}

export function registerPartComponent(type: string, component: PartComponent) {
  PART_MAPPING[type] = component
}

export function Message(props: MessageProps) {
  if (props.message.role === "user" && props.message) {
    return (
      <UserMessageDisplay message={props.message as UserMessage} parts={props.parts} actions={props.actions} />
    )
  }
  if (props.message.role === "assistant" && props.message) {
    return (
      <AssistantMessageDisplay
        message={props.message as AssistantMessage}
        parts={props.parts}
        showAssistantCopyPartID={props.showAssistantCopyPartID}
        showReasoningSummaries={props.showReasoningSummaries}
      />
    )
  }
  return null
}

export function AssistantMessageDisplay(props: {
  message: AssistantMessage
  parts: PartType[]
  showAssistantCopyPartID?: string | null
  showReasoningSummaries?: boolean
}) {
  const partMap = useMemo(() => index(props.parts), [props.parts])
  const grouped = useMemo(
    () =>
      groupParts(
        props.parts
          .filter((part) => renderable(part, props.showReasoningSummaries ?? true))
          .map((part) => ({
            messageID: props.message.id,
            part,
          })),
      ),
    [props.parts, props.showReasoningSummaries, props.message.id],
  )

  return (
    <>
      {grouped.map((entry) => (
        <AssistantMessageGroupEntry
          key={entry.key}
          entry={entry}
          partMap={partMap}
          message={props.message}
          showAssistantCopyPartID={props.showAssistantCopyPartID}
          turnDurationMs={props.turnDurationMs}
          shellToolDefaultOpen={props.shellToolDefaultOpen}
          editToolDefaultOpen={props.editToolDefaultOpen}
        />
      ))}
    </>
  )
}

function ContextToolGroup(props: { parts: ToolPart[]; busy?: boolean }) {
  const i18n = useI18n()
  const [open, setOpen] = useState(false)
  const pending = !!(props.busy || props.parts.some((part) => part.state.status === "pending" || part.state.status === "running"))
  const summary = useMemo(() => contextToolSummary(props.parts), [props.parts])

  return (
    <Collapsible open={open} onOpenChange={setOpen} variant="ghost" className="tool-collapsible">
      <Collapsible.Trigger>
        <div data-component="context-tool-group-trigger">
          <span
            data-slot="context-tool-group-title"
            className="min-w-0 flex items-center gap-2 text-14-medium text-text-strong"
          >
            <span data-slot="context-tool-group-label" className="shrink-0">
              <ToolStatusTitle
                active={pending}
                activeText={i18n.t("ui.sessionTurn.status.gatheringContext")}
                doneText={i18n.t("ui.sessionTurn.status.gatheredContext")}
                split={false}
              />
            </span>
            <span
              data-slot="context-tool-group-summary"
              className="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap font-normal text-text-base"
            >
              <AnimatedCountList
                items={[
                  {
                    key: "read",
                    count: summary.read,
                    one: i18n.t("ui.messagePart.context.read.one"),
                    other: i18n.t("ui.messagePart.context.read.other"),
                  },
                  {
                    key: "search",
                    count: summary.search,
                    one: i18n.t("ui.messagePart.context.search.one"),
                    other: i18n.t("ui.messagePart.context.search.other"),
                  },
                  {
                    key: "list",
                    count: summary.list,
                    one: i18n.t("ui.messagePart.context.list.one"),
                    other: i18n.t("ui.messagePart.context.list.other"),
                  },
                ]}
                fallback=""
              />
            </span>
          </span>
          <Collapsible.Arrow />
        </div>
      </Collapsible.Trigger>
      <Collapsible.Content>
        <div data-component="context-tool-group-list">
          {props.parts.map((part, index) => (
            <ContextToolItem key={part.id} part={part} i18n={i18n} />
          ))}
        </div>
      </Collapsible.Content>
    </Collapsible>
  )
}

export function UserMessageDisplay(props: { message: UserMessage; parts: PartType[]; actions?: UserActions }) {
  const data = useData()
  const dialog = useDialog()
  const i18n = useI18n()
  const [copied, setCopied] = useState(false)
  const [busy, setBusy] = useState(false)

  const textPart = useMemo(
    () => props.parts?.find((p) => p.type === "text" && !(p as TextPart).synthetic) as TextPart | undefined,
    [props.parts],
  )

  const text = useMemo(() => textPart?.text || "", [textPart])

  const files = useMemo(() => (props.parts?.filter((p) => p.type === "file") as FilePart[]) ?? [], [props.parts])

  const attachments = useMemo(() => files.filter(attached), [files])

  const inlineFiles = useMemo(() => files.filter(inline), [files])

  const agents = useMemo(() => (props.parts?.filter((p) => p.type === "agent") as AgentPart[]) ?? [], [props.parts])

  const model = useMemo(() => {
    const providerID = props.message.model?.providerID
    const modelID = props.message.model?.modelID
    if (!providerID || !modelID) return ""
    const match = data.store.provider?.all?.find((p) => p.id === providerID)
    return match?.models?.[modelID]?.name ?? modelID
  }, [props.message.model, data.store.provider])

  const timefmt = useMemo(() => new Intl.DateTimeFormat(i18n.locale(), { timeStyle: "short" }), [i18n])

  const stamp = useMemo(() => {
    const created = props.message.time?.created
    if (typeof created !== "number") return ""
    return timefmt.format(created)
  }, [props.message.time?.created, timefmt])

  const metaHead = useMemo(() => {
    const agent = props.message.agent
    const items = [agent ? agent[0]?.toUpperCase() + agent.slice(1) : "", model]
    return items.filter((x) => !!x).join("\u00A0\u00B7\u00A0")
  }, [props.message.agent, model])

  const metaTail = stamp

  const openImagePreview = (url: string, alt?: string) => {
    dialog.show(() => <ImagePreview src={url} alt={alt} />)
  }

  const handleCopy = async () => {
    const content = text
    if (!content) return
    await navigator.clipboard.writeText(content)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  const revert = () => {
    const act = props.actions?.revert
    if (!act || busy) return
    setBusy(true)
    void Promise.resolve()
      .then(() =>
        act({
          sessionID: props.message.sessionID,
          messageID: props.message.id,
        }),
      )
      .finally(() => setBusy(false))
  }

  return (
    <div data-component="user-message">
      {attachments.length > 0 && (
        <div data-slot="user-message-attachments">
          {attachments.map((file) => {
            const fileType = kind(file)
            const name = file.filename ?? i18n.t("ui.message.attachment.alt")

            return (
              <div
                key={file.url}
                data-slot="user-message-attachment"
                data-type={fileType}
                data-clickable={fileType === "image" ? "true" : undefined}
                title={fileType === "file" ? name : undefined}
                onClick={() => {
                  if (fileType === "image") openImagePreview(file.url, name)
                }}
              >
                {fileType === "image" ? (
                  <img data-slot="user-message-attachment-image" src={file.url} alt={name} />
                ) : (
                  <div data-slot="user-message-attachment-file">
                    <FileIcon node={{ path: name, type: "file" }} />
                    <span data-slot="user-message-attachment-name">{name}</span>
                  </div>
                )}
              </div>
            )
          })}
        </div>
      )}
      {text && (
        <>
          <div data-slot="user-message-body">
            <div data-slot="user-message-text">
              <HighlightedText text={text} references={inlineFiles} agents={agents} />
            </div>
          </div>
          <div data-slot="user-message-copy-wrapper">
            {(metaHead || metaTail) && (
              <span data-slot="user-message-meta-wrap">
                {metaHead && (
                  <span data-slot="user-message-meta" className="text-12-regular text-text-weak cursor-default">
                    {metaHead}
                  </span>
                )}
                {metaHead && metaTail && (
                  <span data-slot="user-message-meta-sep" className="text-12-regular text-text-weak cursor-default">
                    {"\u00A0\u00B7\u00A0"}
                  </span>
                )}
                {metaTail && (
                  <span data-slot="user-message-meta-tail" className="text-12-regular text-text-weak cursor-default">
                    {metaTail}
                  </span>
                )}
              </span>
            )}
            {props.actions?.revert && (
              <Tooltip value={i18n.t("ui.message.revertMessage")} placement="top" gutter={4}>
                <IconButton
                  icon="reset"
                  size="normal"
                  variant="ghost"
                  disabled={!!busy}
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={(event) => {
                    event.stopPropagation()
                    revert()
                  }}
                  aria-label={i18n.t("ui.message.revertMessage")}
                />
              </Tooltip>
            )}
            <Tooltip
              value={copied ? i18n.t("ui.message.copied") : i18n.t("ui.message.copyMessage")}
              placement="top"
              gutter={4}
            >
              <IconButton
                icon={copied ? "check" : "copy"}
                size="normal"
                variant="ghost"
                onMouseDown={(e) => e.preventDefault()}
                onClick={(event) => {
                  event.stopPropagation()
                  void handleCopy()
                }}
                aria-label={copied ? i18n.t("ui.message.copied") : i18n.t("ui.message.copyMessage")}
              />
            </Tooltip>
          </div>
        </>
      )}
    </div>
  )
}

type HighlightSegment = { text: string; type?: "file" | "agent" }

function HighlightedText(props: { text: string; references: FilePart[]; agents: AgentPart[] }) {
  const segments = useMemo(() => {
    const text = props.text

    const allRefs: { start: number; end: number; type: "file" | "agent" }[] = [
      ...props.references
        .filter((r) => r.source?.text?.start !== undefined && r.source?.text?.end !== undefined)
        .map((r) => ({ start: r.source!.text!.start, end: r.source!.text!.end, type: "file" as const })),
      ...props.agents
        .filter((a) => a.source?.start !== undefined && a.source?.end !== undefined)
        .map((a) => ({ start: a.source!.start, end: a.source!.end, type: "agent" as const })),
    ].sort((a, b) => a.start - b.start)

    const result: HighlightSegment[] = []
    let lastIndex = 0

    for (const ref of allRefs) {
      if (ref.start < lastIndex) continue

      if (ref.start > lastIndex) {
        result.push({ text: text.slice(lastIndex, ref.start) })
      }

      result.push({ text: text.slice(ref.start, ref.end), type: ref.type })
      lastIndex = ref.end
    }

    if (lastIndex < text.length) {
      result.push({ text: text.slice(lastIndex) })
    }

    return result
  }, [props.text, props.references, props.agents])

  return segments.map((segment) => (
    <span key={segment.text + (segment.type ?? "")} data-highlight={segment.type}>{segment.text}</span>
  ))
}

export function Part(props: MessagePartProps) {
  const Component = useMemo(() => PART_MAPPING[props.part.type], [props.part.type])
  if (!Component) return null
  return (
    <Component
      part={props.part}
      message={props.message}
      hideDetails={props.hideDetails}
      defaultOpen={props.defaultOpen}
      showAssistantCopyPartID={props.showAssistantCopyPartID}
      turnDurationMs={props.turnDurationMs}
    />
  )
}

export interface ToolProps {
  input: Record<string, any>
  metadata: Record<string, any>
  tool: string
  sessionID?: string
  output?: string
  status?: string
  hideDetails?: boolean
  defaultOpen?: boolean
  forceOpen?: boolean
  locked?: boolean
}

export type ToolComponent = React.FC<ToolProps>

const toolRegistry: Record<
  string,
  {
    name: string
    render?: ToolComponent
  }
> = {}

export function registerTool(input: { name: string; render?: ToolComponent }) {
  toolRegistry[input.name] = input
  return input
}

export function getTool(name: string) {
  return toolRegistry[name]?.render
}

export const ToolRegistry = {
  register: registerTool,
  render: getTool,
}

function ToolFileAccordion(props: { path: string; actions?: React.ReactNode; children: React.ReactNode }) {
  const value = useMemo(() => props.path || "tool-file", [props.path])

  return (
    <Accordion
      multiple
      data-scope="apply-patch"
      style={{ "--sticky-accordion-offset": "calc(32px + var(--tool-content-gap))" } as React.CSSProperties}
      defaultValue={[value]}
    >
      <Accordion.Item value={value}>
        <StickyAccordionHeader>
          <Accordion.Trigger>
            <div data-slot="apply-patch-trigger-content">
              <div data-slot="apply-patch-file-info">
                <FileIcon node={{ path: props.path, type: "file" }} />
                <div data-slot="apply-patch-file-name-container">
                  {props.path.includes("/") && (
                    <span data-slot="apply-patch-directory">{`\u202A${getDirectory(props.path)}\u202C`}</span>
                  )}
                  <span data-slot="apply-patch-filename">{getFilename(props.path)}</span>
                </div>
              </div>
              <div data-slot="apply-patch-trigger-actions">
                {props.actions}
                <Icon name="chevron-grabber-vertical" size="small" />
              </div>
            </div>
          </Accordion.Trigger>
        </StickyAccordionHeader>
        <Accordion.Content>{props.children}</Accordion.Content>
      </Accordion.Item>
    </Accordion>
  )
}

PART_MAPPING["tool"] = function ToolPartDisplay(props) {
  const data = useData()
  const i18n = useI18n()
  const part = props.part as ToolPart
  if (part.tool === "todowrite") return null

  const hideQuestion = part.tool === "question" && (part.state.status === "pending" || part.state.status === "running")

  const emptyInput: Record<string, any> = {}
  const emptyMetadata: Record<string, any> = {}

  const input = part.state?.input ?? emptyInput
  // @ts-expect-error
  const partMetadata = part.state?.metadata ?? emptyMetadata
  const taskId = useMemo(() => {
    if (part.tool !== "task") return
    const value = partMetadata.sessionId
    if (typeof value === "string" && value) return value
  }, [part.tool, partMetadata.sessionId])
  const taskHref = useMemo(() => {
    if (part.tool !== "task") return
    return sessionLink(taskId, getPathname(), data.sessionHref)
  }, [part.tool, taskId, data.sessionHref])
  const taskSubtitle = useMemo(() => {
    if (part.tool !== "task") return undefined
    const value = input.description
    if (typeof value === "string" && value) return value
    return taskId
  }, [part.tool, input.description, taskId])

  const RenderComponent = useMemo(() => ToolRegistry.render(part.tool) ?? GenericTool, [part.tool])

  return !hideQuestion ? (
    <div data-component="tool-part-wrapper">
      {part.state.status === "error" && (part.state as any).error ? (
        (() => {
          const cleaned = String((part.state as any).error).replace("Error: ", "")
          if (part.tool === "question" && cleaned.includes("dismissed this question")) {
            return (
              <div style={{ width: "100%", display: "flex", justifyContent: "flex-end" }}>
                <span className="text-13-regular text-text-weak cursor-default">
                  {i18n.t("ui.messagePart.questions.dismissed")}
                </span>
              </div>
            )
          }
          return (
            <ToolErrorCard
              tool={part.tool}
              error={cleaned}
              title={part.tool === "websearch" ? webSearchProviderLabel(partMetadata.provider) : undefined}
              defaultOpen={props.defaultOpen}
              subtitle={taskSubtitle}
              href={taskHref}
            />
          )
        })()
      ) : (
        <RenderComponent
          input={input}
          tool={part.tool}
          sessionID={part.sessionID}
          metadata={partMetadata}
          // @ts-expect-error
          output={part.state.output}
          status={part.state.status}
          hideDetails={props.hideDetails}
          defaultOpen={props.defaultOpen}
        />
      )}
    </div>
  ) : null
}

export function MessageDivider(props: { label: string }) {
  return (
    <div data-component="compaction-part">
      <div data-slot="compaction-part-divider">
        <span data-slot="compaction-part-line" />
        <span data-slot="compaction-part-label" className="text-12-regular text-text-weak">
          {props.label}
        </span>
        <span data-slot="compaction-part-line" />
      </div>
    </div>
  )
}

PART_MAPPING["compaction"] = function CompactionPartDisplay() {
  const i18n = useI18n()
  return <MessageDivider label={i18n.t("ui.messagePart.compaction")} />
}

PART_MAPPING["text"] = function TextPartDisplay(props) {
  const data = useData()
  const i18n = useI18n()
  const numfmt = useMemo(() => new Intl.NumberFormat(i18n.locale()), [i18n])
  const part = props.part as TextPart
  const interrupted = useMemo(
    () =>
      props.message.role === "assistant" && (props.message as AssistantMessage).error?.name === "MessageAbortedError",
    [props.message.role, props.message],
  )

  const model = useMemo(() => {
    if (props.message.role !== "assistant") return ""
    const message = props.message as AssistantMessage
    const match = data.store.provider?.all?.find((p) => p.id === message.providerID)
    return match?.models?.[message.modelID]?.name ?? message.modelID
  }, [props.message.role, props.message, data.store.provider])

  const duration = useMemo(() => {
    if (props.message.role !== "assistant") return ""
    const message = props.message as AssistantMessage
    const completed = message.time.completed
    const ms =
      typeof props.turnDurationMs === "number"
        ? props.turnDurationMs
        : typeof completed === "number"
          ? completed - message.time.created
          : -1
    if (!(ms >= 0)) return ""
    const total = Math.round(ms / 1000)
    if (total < 60) return i18n.t("ui.message.duration.seconds", { count: numfmt.format(total) })
    const minutes = Math.floor(total / 60)
    const seconds = total % 60
    return i18n.t("ui.message.duration.minutesSeconds", {
      minutes: numfmt.format(minutes),
      seconds: numfmt.format(seconds),
    })
  }, [props.message.role, props.message, props.turnDurationMs, i18n, numfmt])

  const meta = useMemo(() => {
    if (props.message.role !== "assistant") return ""
    const agent = (props.message as AssistantMessage).agent
    const items = [
      agent ? agent[0]?.toUpperCase() + agent.slice(1) : "",
      model,
      duration,
      interrupted ? i18n.t("ui.message.interrupted") : "",
    ]
    return items.filter((x) => !!x).join(" \u00B7 ")
  }, [props.message.role, props.message, model, duration, interrupted, i18n])

  const streaming = useMemo(
    () => props.message.role === "assistant" && typeof (props.message as AssistantMessage).time.completed !== "number",
    [props.message.role, props.message],
  )
  const text = (part.text ?? "").trim()
  const isLastTextPart = useMemo(() => {
    const last = (data.store.part?.[props.message.id] ?? [])
      .filter((item): item is TextPart => item?.type === "text" && !!item.text?.trim())
      .at(-1)
    return last?.id === part.id
  }, [data.store.part, props.message.id, part.id])
  const showCopy = useMemo(() => {
    if (props.message.role !== "assistant") return isLastTextPart
    if (props.showAssistantCopyPartID === null) return false
    if (typeof props.showAssistantCopyPartID === "string") return props.showAssistantCopyPartID === part.id
    return isLastTextPart
  }, [props.message.role, props.showAssistantCopyPartID, part.id, isLastTextPart])
  const [copied, setCopied] = useState(false)

  const handleCopy = async () => {
    const content = text
    if (!content) return
    await navigator.clipboard.writeText(content)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  return text ? (
    <div data-component="text-part">
      <div data-slot="text-part-body">
        {streaming ? (
          <PacedMarkdown text={text} cacheKey={part.id} streaming={streaming} />
        ) : (
          <Markdown text={text} cacheKey={part.id} streaming={false} />
        )}
      </div>
      {showCopy && (
        <div data-slot="text-part-copy-wrapper" data-interrupted={interrupted ? "" : undefined}>
          <Tooltip
            value={copied ? i18n.t("ui.message.copied") : i18n.t("ui.message.copyResponse")}
            placement="top"
            gutter={4}
          >
            <IconButton
              icon={copied ? "check" : "copy"}
              size="normal"
              variant="ghost"
              onMouseDown={(e) => e.preventDefault()}
              onClick={handleCopy}
              aria-label={copied ? i18n.t("ui.message.copied") : i18n.t("ui.message.copyResponse")}
            />
          </Tooltip>
          {meta && (
            <span data-slot="text-part-meta" className="text-12-regular text-text-weak cursor-default">
              {meta}
            </span>
          )}
        </div>
      )}
    </div>
  ) : null
}

PART_MAPPING["reasoning"] = function ReasoningPartDisplay(props) {
  const part = props.part as ReasoningPart
  const streaming = useMemo(
    () => props.message.role === "assistant" && typeof (props.message as AssistantMessage).time.completed !== "number",
    [props.message.role, props.message],
  )
  const text = part.text.trim()

  return text ? (
    <div data-component="reasoning-part">
      {streaming ? (
        <PacedMarkdown text={text} cacheKey={part.id} streaming={streaming} />
      ) : (
        <Markdown text={text} cacheKey={part.id} streaming={false} />
      )}
    </div>
  ) : null
}

ToolRegistry.register({
  name: "read",
  render(props) {
    const data = useData()
    const i18n = useI18n()
    const args: string[] = []
    if (props.input.offset) args.push("offset=" + props.input.offset)
    if (props.input.limit) args.push("limit=" + props.input.limit)
    const loaded = useMemo(() => {
      if (props.status !== "completed") return []
      const value = props.metadata.loaded
      if (!value || !Array.isArray(value)) return []
      return value.filter((p): p is string => typeof p === "string")
    }, [props.status, props.metadata.loaded])
    return (
      <>
        <BasicTool
          {...props}
          icon="glasses"
          trigger={{
            title: i18n.t("ui.tool.read"),
            subtitle: props.input.filePath ? getFilename(props.input.filePath) : "",
            args,
          }}
        />
        {loaded.length > 0 && loaded.map((filepath) => (
          <div key={filepath} data-component="tool-loaded-file">
            <Icon name="enter" size="small" />
            <span>
              {i18n.t("ui.tool.loaded")} {relativizeProjectPath(filepath, data.directory)}
            </span>
          </div>
        ))}
      </>
    )
  },
})

ToolRegistry.register({
  name: "list",
  render(props) {
    const i18n = useI18n()
    return (
      <BasicTool
        {...props}
        icon="bullet-list"
        trigger={{ title: i18n.t("ui.tool.list"), subtitle: getDirectory(props.input.path || "/") }}
      >
        {props.output && (
          <div data-component="tool-output" data-scrollable>
            <Markdown text={props.output} />
          </div>
        )}
      </BasicTool>
    )
  },
})

ToolRegistry.register({
  name: "glob",
  render(props) {
    const i18n = useI18n()
    return (
      <BasicTool
        {...props}
        icon="magnifying-glass-menu"
        trigger={{
          title: i18n.t("ui.tool.glob"),
          subtitle: getDirectory(props.input.path || "/"),
          args: props.input.pattern ? ["pattern=" + props.input.pattern] : [],
        }}
      >
        {props.output && (
          <div data-component="tool-output" data-scrollable>
            <Markdown text={props.output} />
          </div>
        )}
      </BasicTool>
    )
  },
})

ToolRegistry.register({
  name: "grep",
  render(props) {
    const i18n = useI18n()
    const args: string[] = []
    if (props.input.pattern) args.push("pattern=" + props.input.pattern)
    if (props.input.include) args.push("include=" + props.input.include)
    return (
      <BasicTool
        {...props}
        icon="magnifying-glass-menu"
        trigger={{
          title: i18n.t("ui.tool.grep"),
          subtitle: getDirectory(props.input.path || "/"),
          args,
        }}
      >
        {props.output && (
          <div data-component="tool-output" data-scrollable>
            <Markdown text={props.output} />
          </div>
        )}
      </BasicTool>
    )
  },
})

ToolRegistry.register({
  name: "webfetch",
  render(props) {
    const i18n = useI18n()
    const pending = useMemo(() => props.status === "pending" || props.status === "running", [props.status])
    const url = useMemo(() => {
      const value = props.input.url
      if (typeof value !== "string") return ""
      return value
    }, [props.input.url])
    return (
      <BasicTool
        {...props}
        hideDetails
        icon="window-cursor"
        trigger={
          <div data-slot="basic-tool-tool-info-structured">
            <div data-slot="basic-tool-tool-info-main">
              <span data-slot="basic-tool-tool-title">
                <TextShimmer text={i18n.t("ui.tool.webfetch")} active={pending} />
              </span>
              {!pending && url && (
                <a
                  data-slot="basic-tool-tool-subtitle"
                  className="clickable subagent-link"
                  href={url}
                  target="_blank"
                  rel="noopener noreferrer"
                  onClick={(event) => event.stopPropagation()}
                >
                  {url}
                </a>
              )}
            </div>
            {!pending && url && (
              <div data-component="tool-action">
                <Icon name="square-arrow-top-right" size="small" />
              </div>
            )}
          </div>
        }
      />
    )
  },
})

ToolRegistry.register({
  name: "websearch",
  render(props) {
    const query = useMemo(() => {
      const value = props.input.query
      if (typeof value !== "string") return ""
      return value
    }, [props.input.query])
    const title = useMemo(() => webSearchProviderLabel(props.metadata.provider), [props.metadata.provider])

    return (
      <BasicTool
        {...props}
        icon="window-cursor"
        trigger={{
          title: title,
          subtitle: query,
          subtitleClass: "exa-tool-query",
        }}
      >
        <ExaOutput output={props.output} />
      </BasicTool>
    )
  },
})

ToolRegistry.register({
  name: "task",
  render(props) {
    const data = useData()
    const i18n = useI18n()
    const pathname = getPathname()
    const childSessionId = useMemo(() => {
      const value = props.metadata.sessionId
      if (typeof value === "string" && value) return value
      return taskSession(props.input, pathname, data.store.session, data.store.agent)
    }, [props.metadata.sessionId, props.input, pathname, data.store.session, data.store.agent])
    const agent = useMemo(() => taskAgent(props.input.subagent_type, data.store.agent), [props.input.subagent_type, data.store.agent])
    const title = useMemo(() => agent.name ?? i18n.t("ui.tool.agent.default"), [agent, i18n])
    const tone = useMemo(() => agent.color, [agent])
    const subtitle = useMemo(() => {
      const value = props.input.description
      if (typeof value === "string" && value) return value
      return childSessionId
    }, [props.input.description, childSessionId])
    const running = useMemo(() => props.status === "pending" || props.status === "running", [props.status])

    const href = useMemo(() => sessionLink(childSessionId, pathname, data.sessionHref), [childSessionId, pathname, data.sessionHref])
    const clickable = !!(childSessionId && (data.navigateToSession || href))

    const open = () => {
      const id = childSessionId
      if (!id) return
      if (data.navigateToSession) {
        data.navigateToSession(id)
        return
      }
      const value = href
      if (value) window.location.assign(value)
    }

    const navigate = (event: MouseEvent) => {
      if (!data.navigateToSession) return
      if (event.button !== 0 || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return
      event.preventDefault()
      open()
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
          <div data-component="task-tool-action">
            <Icon name="square-arrow-top-right" size="small" />
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
        triggerHref={href}
        clickable={clickable}
        onTriggerClick={navigate}
      />
    )
  },
})

ToolRegistry.register({
  name: "bash",
  render(props) {
    const i18n = useI18n()
    const pending = props.status === "pending" || props.status === "running"
    const sawPending = pending
    const text = useMemo(() => {
      const cmd = props.input.command ?? props.metadata.command ?? ""
      const out = stripAnsi(props.output || props.metadata.output || "").replace(/\r\n?/g, "\n")
      return `$ ${cmd}${out ? "\n\n" + out : ""}`
    }, [props.input.command, props.metadata.command, props.output, props.metadata.output])
    const [copied, setCopied] = useState(false)

    const handleCopy = async () => {
      const content = text
      if (!content) return
      await navigator.clipboard.writeText(content)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    }

    return (
      <BasicTool
        {...props}
        icon="console"
        trigger={
          <div data-slot="basic-tool-tool-info-structured">
            <div data-slot="basic-tool-tool-info-main">
              <span data-slot="basic-tool-tool-title">
                <TextShimmer text={i18n.t("ui.tool.shell")} active={pending} />
              </span>
              {!pending && props.input.description && (
                <ShellSubmessage text={props.input.description} animate={sawPending} />
              )}
            </div>
          </div>
        }
      >
        <div data-component="bash-output">
          <div data-slot="bash-copy">
            <Tooltip
              value={copied ? i18n.t("ui.message.copied") : i18n.t("ui.message.copy")}
              placement="top"
              gutter={4}
            >
              <IconButton
                icon={copied ? "check" : "copy"}
                size="small"
                variant="secondary"
                onMouseDown={(e) => e.preventDefault()}
                onClick={handleCopy}
                aria-label={copied ? i18n.t("ui.message.copied") : i18n.t("ui.message.copy")}
              />
            </Tooltip>
          </div>
          <div data-slot="bash-scroll" data-scrollable>
            <pre data-slot="bash-pre">
              <code>{text}</code>
            </pre>
          </div>
        </div>
      </BasicTool>
    )
  },
})

ToolRegistry.register({
  name: "edit",
  render(props) {
    const i18n = useI18n()
    const fileComponent = useFileComponent()
    const diagnostics = useMemo(() => getDiagnostics(props.metadata.diagnostics, props.input.filePath), [props.metadata.diagnostics, props.input.filePath])
    const path = useMemo(() => props.metadata?.filediff?.file || props.input.filePath || "", [props.metadata?.filediff?.file, props.input.filePath])
    const filename = getFilename(props.input.filePath ?? "")
    const pending = props.status === "pending" || props.status === "running"
    const FileComponent = fileComponent as React.ComponentType<any>
    return (
      <div data-component="edit-tool">
        <BasicTool
          {...props}
          icon="code-lines"
          defer
          trigger={
            <div data-component="edit-trigger">
              <div data-slot="message-part-title-area">
                <div data-slot="message-part-title">
                  <span data-slot="message-part-title-text">
                    <TextShimmer text={i18n.t("ui.messagePart.title.edit")} active={pending} />
                  </span>
                  {!pending && (
                    <span data-slot="message-part-title-filename">{filename}</span>
                  )}
                </div>
                {!pending && props.input.filePath?.includes("/") && (
                  <div data-slot="message-part-path">
                    <span data-slot="message-part-directory">{getDirectory(props.input.filePath)}</span>
                  </div>
                )}
              </div>
              <div data-slot="message-part-actions">
                {!pending && props.metadata.filediff && (
                  <DiffChanges changes={props.metadata.filediff} />
                )}
              </div>
            </div>
          }
        >
          {path && (
            <ToolFileAccordion
              path={path}
              actions={
                !pending && props.metadata.filediff ? (
                  <DiffChanges changes={props.metadata.filediff} />
                ) : undefined
              }
            >
              <div data-component="edit-content">
                <FileComponent
                  mode="diff"
                  before={{
                    name: props.metadata?.filediff?.file || props.input.filePath,
                    contents: props.metadata?.filediff?.before || props.input.oldString || "",
                  }}
                  after={{
                    name: props.metadata?.filediff?.file || props.input.filePath,
                    contents: props.metadata?.filediff?.after || props.input.newString || "",
                  }}
                />
              </div>
            </ToolFileAccordion>
          )}
          <DiagnosticsDisplay diagnostics={diagnostics} />
        </BasicTool>
      </div>
    )
  },
})

ToolRegistry.register({
  name: "write",
  render(props) {
    const i18n = useI18n()
    const fileComponent = useFileComponent()
    const diagnostics = useMemo(() => getDiagnostics(props.metadata.diagnostics, props.input.filePath), [props.metadata.diagnostics, props.input.filePath])
    const path = useMemo(() => props.input.filePath || "", [props.input.filePath])
    const filename = getFilename(props.input.filePath ?? "")
    const pending = props.status === "pending" || props.status === "running"
    const FileComponent = fileComponent as React.ComponentType<any>
    return (
      <div data-component="write-tool">
        <BasicTool
          {...props}
          icon="code-lines"
          defer
          trigger={
            <div data-component="write-trigger">
              <div data-slot="message-part-title-area">
                <div data-slot="message-part-title">
                  <span data-slot="message-part-title-text">
                    <TextShimmer text={i18n.t("ui.messagePart.title.write")} active={pending} />
                  </span>
                  {!pending && (
                    <span data-slot="message-part-title-filename">{filename}</span>
                  )}
                </div>
                {!pending && props.input.filePath?.includes("/") && (
                  <div data-slot="message-part-path">
                    <span data-slot="message-part-directory">{getDirectory(props.input.filePath!)}</span>
                  </div>
                )}
              </div>
              <div data-slot="message-part-actions">{/* <DiffChanges diff={diff} /> */}</div>
            </div>
          }
        >
          {props.input.content && path && (
            <ToolFileAccordion path={path}>
              <div data-component="write-content">
                <FileComponent
                  mode="text"
                  file={{
                    name: props.input.filePath,
                    contents: props.input.content,
                    cacheKey: checksum(props.input.content),
                  }}
                  overflow="scroll"
                />
              </div>
            </ToolFileAccordion>
          )}
          <DiagnosticsDisplay diagnostics={diagnostics} />
        </BasicTool>
      </div>
    )
  },
})

ToolRegistry.register({
  name: "apply_patch",
  render(props) {
    const i18n = useI18n()
    const fileComponent = useFileComponent()
    const files = useMemo(() => patchFiles(props.metadata.files), [props.metadata.files])
    const pending = useMemo(() => props.status === "pending" || props.status === "running", [props.status])
    const single = useMemo(() => {
      const list = files
      if (list.length !== 1) return
      return list[0]
    }, [files])
    const [expanded, setExpanded] = useState<string[]>([])
    let seeded = false

    useEffect(() => {
      const list = files
      if (list.length === 0) return
      if (seeded) return
      seeded = true
      setExpanded(list.filter((f) => f.type !== "delete").map((f) => f.filePath))
    })

    const subtitle = useMemo(() => {
      const count = files.length
      if (count === 0) return ""
      return `${count} ${i18n.t(count > 1 ? "ui.common.file.other" : "ui.common.file.one")}`
    }, [files, i18n])

    const FileComponent = fileComponent as React.ComponentType<any>

    return single ? (
      <div data-component="apply-patch-tool">
        <BasicTool
          {...props}
          icon="code-lines"
          defer
          trigger={
            <div data-component="edit-trigger">
              <div data-slot="message-part-title-area">
                <div data-slot="message-part-title">
                  <span data-slot="message-part-title-text">
                    <TextShimmer text={i18n.t("ui.tool.patch")} active={pending} />
                  </span>
                  {!pending && (
                    <span data-slot="message-part-title-filename">{getFilename(single.relativePath)}</span>
                  )}
                </div>
                {!pending && single.relativePath.includes("/") && (
                  <div data-slot="message-part-path">
                    <span data-slot="message-part-directory">{getDirectory(single.relativePath)}</span>
                  </div>
                )}
              </div>
              <div data-slot="message-part-actions">
                {!pending && (
                  <DiffChanges changes={{ additions: single.additions, deletions: single.deletions }} />
                )}
              </div>
            </div>
          }
        >
          <ToolFileAccordion
            path={single.relativePath}
            actions={
              single.type === "add" ? (
                <span data-slot="apply-patch-change" data-type="added">
                  {i18n.t("ui.patch.action.created")}
                </span>
              ) : single.type === "delete" ? (
                <span data-slot="apply-patch-change" data-type="removed">
                  {i18n.t("ui.patch.action.deleted")}
                </span>
              ) : single.type === "move" ? (
                <span data-slot="apply-patch-change" data-type="modified">
                  {i18n.t("ui.patch.action.moved")}
                </span>
              ) : (
                <DiffChanges changes={{ additions: single.additions, deletions: single.deletions }} />
              )
            }
          >
            <div data-component="apply-patch-file-diff">
              <FileComponent mode="diff" fileDiff={single.view.fileDiff} />
            </div>
          </ToolFileAccordion>
        </BasicTool>
      </div>
    ) : (
      <div data-component="apply-patch-tool">
        <BasicTool
          {...props}
          icon="code-lines"
          defer
          trigger={{
            title: i18n.t("ui.tool.patch"),
            subtitle: subtitle,
          }}
        >
          {files.length > 0 && (
            <Accordion
              multiple
              data-scope="apply-patch"
              style={{ "--sticky-accordion-offset": "calc(32px + var(--tool-content-gap))" } as React.CSSProperties}
              value={expanded}
              onChange={(value) => setExpanded(Array.isArray(value) ? value : value ? [value] : [])}
            >
              {files.map((file) => (
                <PatchFileItem
                  key={file.filePath}
                  file={file}
                  expanded={expanded}
                  fileComponent={fileComponent}
                  i18n={i18n}
                  setExpanded={setExpanded}
                />
              ))}
            </Accordion>
          )}
        </BasicTool>
      </div>
    )
  },
})

ToolRegistry.register({
  name: "todowrite",
  render(props) {
    const i18n = useI18n()
    const todos = useMemo(() => {
      const meta = props.metadata?.todos
      if (Array.isArray(meta)) return meta

      const input = props.input.todos
      if (Array.isArray(input)) return input

      return []
    }, [props.metadata?.todos, props.input.todos])

    const subtitle = useMemo(() => {
      const list = todos
      if (list.length === 0) return ""
      return `${list.filter((t: Todo) => t.status === "completed").length}/${list.length}`
    }, [todos])

    return (
      <BasicTool
        {...props}
        defaultOpen
        icon="checklist"
        trigger={{
          title: i18n.t("ui.tool.todos"),
          subtitle: subtitle,
        }}
      >
        {todos.length > 0 && (
          <div data-component="todos">
            {todos.map((todo: Todo) => (
              <Checkbox key={todo.content} readOnly checked={todo.status === "completed"}>
                <span
                  data-slot="message-part-todo-content"
                  data-completed={todo.status === "completed" ? "completed" : undefined}
                >
                  {todo.content}
                </span>
              </Checkbox>
            ))}
          </div>
        )}
      </BasicTool>
    )
  },
})

ToolRegistry.register({
  name: "question",
  render(props) {
    const i18n = useI18n()
    const questions = useMemo(() => (props.input.questions ?? []) as QuestionInfo[], [props.input.questions])
    const answers = useMemo(() => (props.metadata.answers ?? []) as QuestionAnswer[], [props.metadata.answers])
    const completed = answers.length > 0

    const subtitle = useMemo(() => {
      const count = questions.length
      if (count === 0) return ""
      if (completed) return i18n.t("ui.question.subtitle.answered", { count })
      return `${count} ${i18n.t(count > 1 ? "ui.common.question.other" : "ui.common.question.one")}`
    }, [questions, completed, i18n])

    return (
      <BasicTool
        {...props}
        defaultOpen={completed}
        icon="bubble-5"
        trigger={{
          title: i18n.t("ui.tool.questions"),
          subtitle: subtitle,
        }}
      >
        {completed && (
          <div data-component="question-answers">
            {questions.map((q, i) => {
              const answer = answers[i] ?? []
              return (
                <div key={q.question} data-slot="question-answer-item">
                  <div data-slot="question-text">{q.question}</div>
                  <div data-slot="answer-text">{answer.join(", ") || i18n.t("ui.question.answer.none")}</div>
                </div>
              )
            })}
          </div>
        )}
      </BasicTool>
    )
  },
})

ToolRegistry.register({
  name: "skill",
  render(props) {
    const i18n = useI18n()
    const title = useMemo(() => props.input.name || i18n.t("ui.tool.skill"), [props.input.name, i18n])
    const running = useMemo(() => props.status === "pending" || props.status === "running", [props.status])

    const titleContent = <TextShimmer text={title} active={running} />

    const trigger = (
      <div data-slot="basic-tool-tool-info-structured">
        <div data-slot="basic-tool-tool-info-main">
          <span data-slot="basic-tool-tool-title" className="capitalize agent-title">
            {titleContent}
          </span>
        </div>
      </div>
    )

    return <BasicTool icon="brain" status={props.status} trigger={trigger} hideDetails />
  },
})
