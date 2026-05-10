import type { ElectronOpencodeEventEnvelope } from "@/electron"
import type { OpenCodePartView, ToolCallView, LiveAssistantState } from "../../types"
import type { PromptState } from "./types"
import {
  normalizeRawPartsForView,
  sortOpenCodeParts,
  extractTextFromParts,
  extractToolCallsFromParts,
  buildAssistantParts,
  shouldAdoptOpenCodeTitle,
} from "../../utils"

interface UseEventHandlingParams {
  promptRunId: string
  activePromptRef: React.MutableRefObject<string | null>
  promptState: PromptState
  setLiveAssistant: (value: LiveAssistantState | null) => void
  updateSession: (projectId: string, sessionId: string, updates: Record<string, unknown>) => Promise<void>
  project: { id: string; path: string }
  session: { id: string; name?: string | null }
}

export const createEventHandling = ({
  promptRunId,
  activePromptRef,
  promptState,
  setLiveAssistant,
  updateSession,
  project,
  session,
}: UseEventHandlingParams) => {
  const liveCreatedAt = Date.now()
  let pendingLiveSnapshot: Omit<LiveAssistantState, "createdAt"> | null = null
  let liveSnapshotFrame: number | null = null

  const collectAllParts = (): OpenCodePartView[] => {
    const all: OpenCodePartView[] = []
    for (const partsMap of promptState.partsByMessageID.values()) {
      all.push(...partsMap.values())
    }
    return sortOpenCodeParts(all)
  }

  const setLiveAssistantSnapshot = (snapshot: Omit<LiveAssistantState, "createdAt">) => {
    pendingLiveSnapshot = snapshot

    const flush = () => {
      liveSnapshotFrame = null
      if (activePromptRef.current !== promptRunId || !pendingLiveSnapshot) return
      const next = pendingLiveSnapshot
      pendingLiveSnapshot = null
      setLiveAssistant({
        ...next,
        createdAt: liveCreatedAt,
      })
    }

    if (typeof window === "undefined") {
      flush()
      return
    }

    if (liveSnapshotFrame === null) {
      liveSnapshotFrame = window.requestAnimationFrame(flush)
    }
  }

  const renderFromParts = () => {
    const parts = collectAllParts()
    promptState.finalParts = parts
    promptState.finalContent = extractTextFromParts(parts)
    promptState.finalToolCalls = extractToolCallsFromParts(parts)
    promptState.finalError = promptState.eventError
    setLiveAssistantSnapshot({
      content: promptState.finalContent,
      toolCalls: promptState.finalToolCalls,
      parts: promptState.finalParts,
      error: promptState.finalError,
    })
  }

  const applyPromptStatusSnapshot = (status: {
    assistantMessageID?: string | null
    parts?: unknown[]
    content?: string
    toolCalls?: ToolCallView[]
    error?: unknown
  }) => {
    if (status.assistantMessageID) promptState.assistantMessageID = status.assistantMessageID

    const statusParts = normalizeRawPartsForView(status.parts, promptState.assistantMessageID)
    if (statusParts.length > 0 && promptState.assistantMessageID) {
      const messageParts = promptState.partsByMessageID.get(promptState.assistantMessageID) ?? new Map<string, OpenCodePartView>()
      statusParts.forEach((part) => messageParts.set(part.id, part))
      promptState.partsByMessageID.set(promptState.assistantMessageID, messageParts)
    }

    const mergedParts = collectAllParts()
    const hasSnapshot =
      mergedParts.length > 0 ||
      Boolean(status.content) ||
      Boolean(status.toolCalls?.length) ||
      Boolean(status.error)

    if (!hasSnapshot) return

    promptState.finalParts = mergedParts.length > 0 ? mergedParts : promptState.finalParts
    promptState.finalContent = mergedParts.length > 0 ? extractTextFromParts(mergedParts) : status.content || promptState.finalContent
    promptState.finalToolCalls = mergedParts.length > 0
      ? extractToolCallsFromParts(mergedParts)
      : status.toolCalls?.length ? status.toolCalls : promptState.finalToolCalls
    promptState.finalError = status.error ?? promptState.finalError

    if (promptState.finalParts.length === 0) {
      promptState.finalParts = buildAssistantParts(promptState.assistantMessageID ?? "assistant", promptState.finalContent, promptState.finalToolCalls)
    }

    setLiveAssistantSnapshot({
      content: promptState.finalContent,
      toolCalls: promptState.finalToolCalls,
      parts: promptState.finalParts,
      error: promptState.finalError,
    })
  }

  const handleOpencodeEvent = (payload: ElectronOpencodeEventEnvelope) => {
    if (activePromptRef.current !== promptRunId) return
    if (payload.type === "error") {
      console.warn("OpenCode event stream error:", payload.error)
      return
    }
    if (payload.type !== "event") return

    const event = payload.event as { type?: string; properties?: Record<string, unknown> }
    const properties = event?.properties ?? {}
    if (properties.sessionID && promptState.resolvedSessionID && properties.sessionID !== promptState.resolvedSessionID) return

    if (event?.type === "session.updated") {
      const eventSessionID = properties.sessionID ?? (properties.info as { id?: string })?.id
      if (!eventSessionID || !promptState.resolvedSessionID) return
      if (eventSessionID && promptState.resolvedSessionID && eventSessionID !== promptState.resolvedSessionID) return
      const nextTitle = (properties.info as { title?: string })?.title
      if (shouldAdoptOpenCodeTitle(nextTitle, session.name ?? undefined) && nextTitle) {
        void updateSession(project.id, session.id, { name: nextTitle.trim() })
      }
      return
    }

    if (event?.type === "session.error") {
      promptState.finalError = properties.error ?? "OpenCode session failed."
      promptState.eventError = promptState.finalError
      promptState.completedFromEvent = true
      setLiveAssistantSnapshot({
        content: promptState.finalContent,
        toolCalls: promptState.finalToolCalls,
        parts: promptState.finalParts,
        error: promptState.finalError,
      })
      return
    }

    if (event?.type === "message.updated") {
      const info = properties.info as { role?: string; parentID?: string; id?: string; time?: { completed?: number }; error?: unknown }
      if (!info || info.role !== "assistant") return
      if (info.parentID !== promptState.requestMessageID) return
      promptState.resolvedSessionID = (properties.sessionID as string) ?? promptState.resolvedSessionID
      promptState.assistantMessageID = info.id ?? promptState.assistantMessageID
      promptState.completedFromEvent = typeof info.time?.completed === "number"
      promptState.eventError = info.error ?? null
      renderFromParts()
      return
    }

    if (event?.type === "message.part.updated" && (properties.part as OpenCodePartView)?.messageID) {
      const part = properties.part as OpenCodePartView
      const messageID = part.messageID
      if (!messageID) return
      const messageParts = promptState.partsByMessageID.get(messageID) ?? new Map<string, OpenCodePartView>()
      messageParts.set(part.id, part)
      promptState.partsByMessageID.set(messageID, messageParts)
      renderFromParts()
      return
    }

    if (event?.type === "message.part.removed" && properties.messageID && properties.partID) {
      const messageID = properties.messageID as string
      const partID = properties.partID as string
      const messageParts = promptState.partsByMessageID.get(messageID)
      if (!messageParts) return
      messageParts.delete(partID)
      if (messageParts.size === 0) {
        promptState.partsByMessageID.delete(messageID)
      } else {
        promptState.partsByMessageID.set(messageID, messageParts)
      }
      renderFromParts()
      return
    }

    if (event?.type === "message.part.delta" && properties.messageID && properties.partID && properties.field) {
      const messageParts = promptState.partsByMessageID.get(properties.messageID as string) ?? new Map<string, OpenCodePartView>()
      const current = messageParts.get(properties.partID as string) ?? {
        id: properties.partID,
        messageID: properties.messageID,
        type: properties.field === "text" ? "text" : "unknown",
      }
      const previous = typeof (current as Record<string, unknown>)[properties.field as string] === "string"
        ? String((current as Record<string, unknown>)[properties.field as string])
        : ""
      messageParts.set(properties.partID as string, {
        ...current,
        type: current.type === "unknown" && properties.field === "text" ? "text" : current.type,
        [properties.field as string]: `${previous}${(properties.delta as string) ?? ""}`,
      } as OpenCodePartView)
      promptState.partsByMessageID.set(properties.messageID as string, messageParts)
      renderFromParts()
    }
  }

  return { handleOpencodeEvent, applyPromptStatusSnapshot, renderFromParts, collectAllParts }
}
