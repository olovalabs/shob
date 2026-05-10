import { useEffect, useRef } from "react"
import { useStore } from "@/store"
import { nativeApi } from "@/services/native"
import type {
  ElectronOpencodeEventEnvelope,
} from "@/electron"
import type { SubagentTracker, OpenCodePartView } from "../types"
import type { AgentMessage } from "@/types"
import { isOpenCodeDefaultTitle } from "../utils"

interface UseSubagentManagementParams {
  project: { id: string; path: string; sessions: Array<{ id: string; kind?: string | null; opencodeSessionId?: string | null; parentSessionId?: string | null }> } | null
  session: {
    id: string
    opencodeProviderId?: string | null
    opencodeModelId?: string | null
    opencodeModelVariant?: string | null
  } | null
  subagentTrackersRef: React.MutableRefObject<Map<string, SubagentTracker>>
  isThinking: boolean
  liveAssistantContent: string | undefined
  liveAssistantToolCallsLength: number
}

export const useSubagentManagement = ({
  project,
  session,
  subagentTrackersRef,
  isThinking,
  liveAssistantContent,
  liveAssistantToolCallsLength,
}: UseSubagentManagementParams) => {
  const scrollRef = useRef<HTMLDivElement>(null)

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
  }, [isThinking, liveAssistantContent, liveAssistantToolCallsLength])

  useEffect(() => {
    const openSubagentSession = async (opencodeSessionID: string, label?: string, autoLoad = false) => {
      const state = useStore.getState()
      const currentProject = state.projects.find((item: { id: string }) => item.id === project?.id)
      if (!currentProject) return

      const existing = currentProject.sessions.find((item: { id: string; kind?: string | null; opencodeSessionId?: string | null }) => item.kind === "agent" && item.opencodeSessionId === opencodeSessionID)
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
        parentSessionId: parentID ? currentProject.sessions.find((s: { opencodeSessionId?: string | null }) => s.opencodeSessionId === parentID)?.id ?? null : null,
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

  const openSubagentSessionAutoCreate = async (opencodeSessionID: string, projectId: string, _parentSessionId: string) => {
    const state = useStore.getState()
    const currentProject = state.projects.find((item: { id: string }) => item.id === projectId)
    if (!currentProject) return

    const existing = currentProject.sessions.find((item: { id: string; kind?: string | null; opencodeSessionId?: string | null }) => item.kind === "agent" && item.opencodeSessionId === opencodeSessionID)
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
      parentSessionId: parentOpenCodeID ? currentProject.sessions.find((s: { opencodeSessionId?: string | null }) => s.opencodeSessionId === parentOpenCodeID)?.id ?? null : null,
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
      const proj = state.projects.find((p: { id: string }) => p.id === project.id)
      if (!proj) return

      const allParts: OpenCodePartView[] = []
      for (const partsMap of tracker.partsByMessageID.values()) {
        allParts.push(...partsMap.values())
      }

      const agentMessages: AgentMessage[] = []
      const messageOrder = new Map<string, number>()
      let orderCounter = 0

      for (const [messageID, partsMap] of tracker.partsByMessageID.entries()) {
        messageOrder.set(messageID, orderCounter++)
        const parts = [...partsMap.values()].sort((a, b) => a.id.localeCompare(b.id))
        const textParts = parts.filter((p) => p.type === "text")
        const toolParts = parts.filter((p) => p.type === "tool")
        const content = textParts.map((p) => p.text ?? "").join("\n").trim()

        agentMessages.push({
          id: messageID,
          role: "assistant" as const,
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
      const event = payload.event as { type?: string; properties?: { sessionID?: string; part?: OpenCodePartView; messageID?: string; partID?: string; field?: string; delta?: string; info?: { role?: string; id?: string } } }
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

    const opencodeEventHandlersRef = new Set<(payload: ElectronOpencodeEventEnvelope) => void>()
    opencodeEventHandlersRef.add(handleSubagentEvent)

    return () => {
      opencodeEventHandlersRef.delete(handleSubagentEvent)
    }
  }, [project?.id, project?.path])

  return { scrollRef, openSubagentSessionAutoCreate }
}
