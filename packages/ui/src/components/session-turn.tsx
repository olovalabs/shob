import {
  AssistantMessage,
  type SnapshotFileDiff,
  Message as MessageType,
  Part as PartType,
} from "@shob/sdk/v2/client"
import type { SessionStatus } from "@shob/sdk/v2"
import { useData } from "../context"
import { useFileComponent } from "../context/file"

import { Binary } from "@opencode-ai/core/util/binary"
import { getDirectory, getFilename } from "@opencode-ai/core/util/path"
import { useEffect, useMemo, useRef, useState } from "react"
import { AssistantParts, Message, MessageDivider, PART_MAPPING, type UserActions } from "./message-part"
import { Card } from "./card"
import { Accordion } from "./accordion"
import { StickyAccordionHeader } from "./sticky-accordion-header"
import { DiffChanges } from "./diff-changes"
import { Icon } from "./icon"
import { TextShimmer } from "./text-shimmer"
import { SessionRetry } from "./session-retry"
import { TextReveal } from "./text-reveal"
import { createAutoScroll } from "../hooks"
import { useI18n } from "../context/i18n"
import { normalize } from "./session-diff"

function record(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value)
}

function unwrap(message: string) {
  const text = message.replace(/^Error:\s*/, "").trim()

  const parse = (value: string) => {
    try {
      return JSON.parse(value) as unknown
    } catch {
      return undefined
    }
  }

  const read = (value: string) => {
    const first = parse(value)
    if (typeof first !== "string") return first
    return parse(first.trim())
  }

  let json = read(text)

  if (json === undefined) {
    const start = text.indexOf("{")
    const end = text.lastIndexOf("}")
    if (start !== -1 && end > start) {
      json = read(text.slice(start, end + 1))
    }
  }

  if (!record(json)) return message

  const err = record(json.error) ? json.error : undefined
  if (err) {
    const type = typeof err.type === "string" ? err.type : undefined
    const msg = typeof err.message === "string" ? err.message : undefined
    if (type && msg) return `${type}: ${msg}`
    if (msg) return msg
    if (type) return type
    const code = typeof err.code === "string" ? err.code : undefined
    if (code) return code
  }

  const msg = typeof json.message === "string" ? json.message : undefined
  if (msg) return msg

  const reason = typeof json.error === "string" ? json.error : undefined
  if (reason) return reason

  return message
}

function same<T>(a: readonly T[], b: readonly T[]) {
  if (a === b) return true
  if (a.length !== b.length) return false
  return a.every((x, i) => x === b[i])
}

function list<T>(value: T[] | undefined | null, fallback: T[]) {
  if (Array.isArray(value)) return value
  return fallback
}

const hidden = new Set(["todowrite"])

function partState(part: PartType, showReasoningSummaries: boolean) {
  if (part.type === "tool") {
    if (hidden.has(part.tool)) return
    if (part.tool === "question" && (part.state.status === "pending" || part.state.status === "running")) return
    return "visible" as const
  }
  if (part.type === "text") return part.text?.trim() ? ("visible" as const) : undefined
  if (part.type === "reasoning") {
    if (showReasoningSummaries && part.text?.trim()) return "visible" as const
    return
  }
  if (PART_MAPPING[part.type]) return "visible" as const
  return
}

function clean(value: string) {
  return value
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1")
    .replace(/[*_~]+/g, "")
    .trim()
}

function heading(text: string) {
  const markdown = text.replace(/\r\n?/g, "\n")

  const html = markdown.match(/<h[1-6][^>]*>([\s\S]*?)<\/h[1-6]>/i)
  if (html?.[1]) {
    const value = clean(html[1].replace(/<[^>]+>/g, " "))
    if (value) return value
  }

  const atx = markdown.match(/^\s{0,3}#{1,6}[ \t]+(.+?)(?:[ \t]+#+[ \t]*)?$/m)
  if (atx?.[1]) {
    const value = clean(atx[1])
    if (value) return value
  }

  const setext = markdown.match(/^([^\n]+)\n(?:=+|-+)\s*$/m)
  if (setext?.[1]) {
    const value = clean(setext[1])
    if (value) return value
  }

  const strong = markdown.match(/^\s*(?:\*\*|__)(.+?)(?:\*\*|__)\s*$/m)
  if (strong?.[1]) {
    const value = clean(strong[1])
    if (value) return value
  }
}

function SessionTurnDiffItem(props: {
  diff: SnapshotFileDiff
  fileComponent: React.ComponentType<any>
  expanded: string[]
  showAll: boolean
  overflow: number
  toggleAll: () => void
  onExpandedChange: (value: string[]) => void
  i18n: ReturnType<typeof useI18n>
}) {
  const { diff, fileComponent: FileComponent, expanded, onExpandedChange, i18n } = props
  const view = normalize(diff)
  const isActive = expanded.includes(diff.file)
  const [shown, setShown] = useState(false)
  const isFirstRun = useRef(true)

  useEffect(() => {
    if (isFirstRun.current) {
      isFirstRun.current = false
      return
    }
    if (!isActive) {
      setShown(false)
      return
    }

    const id = requestAnimationFrame(() => {
      if (!expanded.includes(diff.file)) return
      setShown(true)
    })
    return () => cancelAnimationFrame(id)
  }, [isActive])

  return (
    <Accordion.Item value={diff.file}>
      <StickyAccordionHeader>
        <Accordion.Trigger>
          <div data-slot="session-turn-diff-trigger">
            <span data-slot="session-turn-diff-path">
              {diff.file.includes("/") && (
                <span data-slot="session-turn-diff-directory">
                  {`\u202A${getDirectory(diff.file)}\u202C`}
                </span>
              )}
              <span data-slot="session-turn-diff-filename">{getFilename(diff.file)}</span>
            </span>
            <div data-slot="session-turn-diff-meta">
              <span data-slot="session-turn-diff-changes">
                <DiffChanges changes={diff} />
              </span>
              <span data-slot="session-turn-diff-chevron">
                <Icon name="chevron-down" size="small" />
              </span>
            </div>
          </div>
        </Accordion.Trigger>
      </StickyAccordionHeader>
      <Accordion.Content>
        {shown && (
          <div data-slot="session-turn-diff-view" data-scrollable>
            <FileComponent mode="diff" fileDiff={view.fileDiff} />
          </div>
        )}
      </Accordion.Content>
    </Accordion.Item>
  )
}

export function SessionTurn(
  props: React.PropsWithChildren<{
    sessionID: string
    messageID: string
    messages?: MessageType[]
    actions?: UserActions
    showReasoningSummaries?: boolean
    shellToolDefaultOpen?: boolean
    editToolDefaultOpen?: boolean
    active?: boolean
    status?: SessionStatus
    onUserInteracted?: () => void
    classes?: {
      root?: string
      content?: string
      container?: string
    }
  }>,
) {
  const data = useData()
  const i18n = useI18n()
  const fileComponent = useFileComponent()

  const emptyMessages: MessageType[] = []
  const emptyParts: PartType[] = []
  const emptyAssistant: AssistantMessage[] = []
  const emptyDiffs: SnapshotFileDiff[] = []
  const idle = { type: "idle" as const }

  const allMessages = useMemo(() => props.messages ?? list(data.store.message?.[props.sessionID], emptyMessages), [data.store.message, props.sessionID, props.messages])

  const messageIndex = useMemo(() => {
    const messages = allMessages ?? emptyMessages
    const result = Binary.search(messages, props.messageID, (m) => m.id)

    const index = result.found ? result.index : messages.findIndex((m) => m.id === props.messageID)
    if (index < 0) return -1

    const msg = messages[index]
    if (!msg || msg.role !== "user") return -1

    return index
  }, [allMessages, props.messageID])

  const message = useMemo(() => {
    const index = messageIndex
    if (index < 0) return undefined

    const messages = allMessages ?? emptyMessages
    const msg = messages[index]
    if (!msg || msg.role !== "user") return undefined

    return msg
  }, [messageIndex, allMessages])

  const pending = useMemo(() => {
    if (typeof props.active === "boolean") return
    const messages = allMessages ?? emptyMessages
    return messages.findLast(
      (item): item is AssistantMessage => item.role === "assistant" && typeof item.time.completed !== "number",
    )
  }, [allMessages, props.active])

  const pendingUser = useMemo(() => {
    const item = pending
    if (!item?.parentID) return
    const messages = allMessages ?? emptyMessages
    const result = Binary.search(messages, item.parentID, (m) => m.id)
    const msg = result.found ? messages[result.index] : messages.find((m) => m.id === item.parentID)
    if (!msg || msg.role !== "user") return
    return msg
  }, [pending, allMessages])

  const active = useMemo(() => {
    if (typeof props.active === "boolean") return props.active
    const msg = message
    const parent = pendingUser
    if (!msg || !parent) return false
    return parent.id === msg.id
  }, [props.active, message, pendingUser])

  const parts = useMemo(() => {
    const msg = message
    if (!msg) return emptyParts
    return list(data.store.part?.[msg.id], emptyParts)
  }, [message, data.store.part])

  const compaction = useMemo(() => parts.find((part) => part.type === "compaction"), [parts])

  const diffs = useMemo(() => {
    const files = message?.summary?.diffs
    if (!files?.length) return emptyDiffs

    const seen = new Set<string>()
    return files
      .reduceRight<SnapshotFileDiff[]>((result, diff) => {
        if (seen.has(diff.file)) return result
        seen.add(diff.file)
        result.push(diff)
        return result
      }, [])
      .reverse()
  }, [message])

  const MAX_FILES = 10
  const edited = diffs.length
  const [showAll, setShowAll] = useState(false)
  const [expanded, setExpanded] = useState<string[]>([])
  const overflow = Math.max(0, edited - MAX_FILES)
  const visible = showAll ? diffs : diffs.slice(0, MAX_FILES)
  const toggleAll = () => {
    autoScroll.pause()
    setShowAll(!showAll)
  }

  const assistantMessages = useMemo(
    () => {
      const msg = message
      if (!msg) return emptyAssistant

      const messages = allMessages ?? emptyMessages
      if (messageIndex < 0) return emptyAssistant

      const result: AssistantMessage[] = []
      for (let i = 0; i < messages.length; i++) {
        const item = messages[i]
        if (!item) continue
        if (item.role === "assistant" && item.parentID === msg.id) result.push(item as AssistantMessage)
      }
      return result
    },
    [message, allMessages, messageIndex],
  )

  const interrupted = assistantMessages.some((m) => m.error?.name === "MessageAbortedError")
  const divider = useMemo(() => {
    if (compaction) return i18n.t("ui.messagePart.compaction")
    if (interrupted) return i18n.t("ui.message.interrupted")
    return ""
  }, [compaction, interrupted, i18n])

  const error = useMemo(
    () => assistantMessages.find((m) => m.error && m.error.name !== "MessageAbortedError")?.error,
    [assistantMessages],
  )

  const showAssistantCopyPartID = useMemo(() => {
    const messages = assistantMessages

    for (let i = messages.length - 1; i >= 0; i--) {
      const msg = messages[i]
      if (!msg) continue

      const parts = list(data.store.part?.[msg.id], emptyParts)
      for (let j = parts.length - 1; j >= 0; j--) {
        const part = parts[j]
        if (!part || part.type !== "text" || !part.text?.trim()) continue
        return part.id
      }
    }

    return undefined
  }, [assistantMessages, data.store.part])

  const errorText = useMemo(() => {
    const msg = error?.data?.message
    if (typeof msg === "string") return unwrap(msg)
    if (msg === undefined || msg === null) return ""
    return unwrap(String(msg))
  }, [error])

  const status = useMemo(() => {
    if (props.status !== undefined) return props.status
    if (typeof props.active === "boolean" && !props.active) return idle
    return data.store.session_status[props.sessionID] ?? idle
  }, [props.status, props.active, data.store.session_status, props.sessionID])

  const working = status.type !== "idle" && active
  const showReasoningSummaries = useMemo(() => props.showReasoningSummaries ?? true, [props.showReasoningSummaries])

  const assistantCopyPartID = useMemo(() => {
    if (working) return null
    return showAssistantCopyPartID ?? null
  }, [working, showAssistantCopyPartID])

  const turnDurationMs = useMemo(() => {
    const start = message?.time.created
    if (typeof start !== "number") return undefined

    const end = assistantMessages.reduce<number | undefined>((max, item) => {
      const completed = item.time.completed
      if (typeof completed !== "number") return max
      if (max === undefined) return completed
      return Math.max(max, completed)
    }, undefined)

    if (typeof end !== "number") return undefined
    if (end < start) return undefined
    return end - start
  }, [message, assistantMessages])

  const assistantDerived = useMemo(() => {
    let visible = 0
    let reason: string | undefined
    const show = showReasoningSummaries
    for (const msg of assistantMessages) {
      for (const part of list(data.store.part?.[msg.id], emptyParts)) {
        if (partState(part, show) === "visible") {
          visible++
        }
        if (part.type === "reasoning" && part.text) {
          const h = heading(part.text)
          if (h) reason = h
        }
      }
    }
    return { visible, reason }
  }, [assistantMessages, showReasoningSummaries, data.store.part])

  const assistantVisible = assistantDerived.visible
  const reasoningHeading = assistantDerived.reason

  const showThinking = useMemo(() => {
    if (!working || !!error) return false
    if (status.type === "retry") return false
    if (showReasoningSummaries) return assistantVisible === 0
    return true
  }, [working, error, status, showReasoningSummaries, assistantVisible])

  const autoScroll = createAutoScroll({
    working: () => working,
    onUserInteracted: props.onUserInteracted,
    overflowAnchor: "dynamic",
  })

  return (
    <div data-component="session-turn" className={props.classes?.root}>
      <div
        ref={autoScroll.scrollRef}
        onScroll={autoScroll.handleScroll}
        data-slot="session-turn-content"
        className={props.classes?.content}
      >
        <div onClick={autoScroll.handleInteraction}>
          {message && (
            <div
              ref={autoScroll.contentRef}
              data-message={message.id}
              data-slot="session-turn-message-container"
              className={props.classes?.container}
            >
              <div data-slot="session-turn-message-content" aria-live="off">
                <Message message={message} parts={parts} actions={props.actions} />
              </div>
              {divider && (
                <div data-slot="session-turn-compaction">
                  <MessageDivider label={divider} />
                </div>
              )}
              {assistantMessages.length > 0 && (
                <div data-slot="session-turn-assistant-content" aria-hidden={working}>
                  <AssistantParts
                    messages={assistantMessages}
                    showAssistantCopyPartID={assistantCopyPartID}
                    turnDurationMs={turnDurationMs}
                    working={working}
                    showReasoningSummaries={showReasoningSummaries}
                    shellToolDefaultOpen={props.shellToolDefaultOpen}
                    editToolDefaultOpen={props.editToolDefaultOpen}
                  />
                </div>
              )}
              {showThinking && (
                <div data-slot="session-turn-thinking">
                  <TextShimmer text={i18n.t("ui.sessionTurn.status.thinking")} />
                  {!showReasoningSummaries && (
                    <TextReveal
                      text={reasoningHeading}
                      className="session-turn-thinking-heading"
                      travel={25}
                      duration={700}
                    />
                  )}
                </div>
              )}
              <SessionRetry status={status} show={active} />
              {edited > 0 && !working && (
                <div
                  data-slot="session-turn-diffs"
                  data-component="session-turn-diffs-group"
                  data-show-all={showAll || undefined}
                >
                  <div data-slot="session-turn-diffs-header">
                    <span data-slot="session-turn-diffs-label">
                      {edited} {i18n.t("ui.sessionTurn.diffs.changed")}{" "}
                      {i18n.t(edited === 1 ? "ui.common.file.one" : "ui.common.file.other")}
                    </span>
                    <DiffChanges changes={diffs} />
                    {overflow > 0 && (
                      <span data-slot="session-turn-diffs-toggle" onClick={toggleAll}>
                        {showAll ? i18n.t("ui.sessionTurn.diffs.showLess") : i18n.t("ui.sessionTurn.diffs.showAll")}
                      </span>
                    )}
                  </div>
                  <div data-component="session-turn-diffs-content">
                    <Accordion
                      multiple
                      style={{ "--sticky-accordion-offset": "44px" } as React.CSSProperties}
                      value={expanded}
                      onChange={(value) => setExpanded(Array.isArray(value) ? value : value ? [value] : [])}
                    >
                      {visible.map((diff) => (
                        <SessionTurnDiffItem
                          key={diff.file}
                          diff={diff}
                          fileComponent={fileComponent}
                          expanded={expanded}
                          showAll={showAll}
                          overflow={overflow}
                          toggleAll={toggleAll}
                          onExpandedChange={setExpanded}
                          i18n={i18n}
                        />
                      ))}
                    </Accordion>
                    {!showAll && overflow > 0 && (
                      <div data-slot="session-turn-diffs-more" onClick={toggleAll}>
                        {i18n.t("ui.sessionTurn.diffs.more", { count: String(overflow) })}
                      </div>
                    )}
                  </div>
                </div>
              )}
              {error && (
                <Card variant="error" className="error-card">
                  {errorText}
                </Card>
              )}
            </div>
          )}
          {props.children}
        </div>
      </div>
    </div>
  )
}
