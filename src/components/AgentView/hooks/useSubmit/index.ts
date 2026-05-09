import { useRef, useCallback } from "react"
import { useStore } from "@/store"
import { nativeApi } from "@/services/native"
import {
  parseOpenCodeModelValue,
} from "@/utils/opencode-models"
import type {
  ElectronOpencodeEventEnvelope,
} from "@/electron"
import {
  createOpenCodeID,
  buildAssistantParts,
  sortOpenCodeParts,
  extractTextFromParts,
  extractToolCallsFromParts,
  describeOpenCodeError,
  getOpenCodeCreateTitle,
} from "../../utils"
import type { OpenCodePartView } from "../../types"
import type { UseSubmitParams, PromptState } from "./types"
import { useEventHandling } from "./useEventHandling"
import { useTitleSync } from "./useTitleSync"

export const useSubmit = ({
  project,
  session,
  selectedModel,
  modelOptions,
  modelPower,
  composerMode,
  setIsThinking,
  setLiveAssistant,
  setInput,
  setAttachedFiles,
  autoGrow,
  setPreferredOpencodeModel,
  setPreferredOpencodeVariant,
  openSubagentSessionAutoCreate,
}: UseSubmitParams) => {
  const activePromptRef = useRef<string | null>(null)
  const opencodeEventHandlersRef = useRef(new Set<(payload: ElectronOpencodeEventEnvelope) => void>())

  const appendAgentMessage = useStore((state) => state.appendAgentMessage)
  const updateSession = useStore((state) => state.updateSession)

  const titleSync = useTitleSync({
    project: project!,
    session: session!,
    updateSession,
  })

  const handleSubmit = useCallback(async () => {
    if (!project || !session) return

    const text = ""
    const attachmentLine = ""
    const promptText = `${text}${attachmentLine}`.trim()
    if (!promptText) return

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

      const promptState: PromptState = {
        partsByMessageID: new Map<string, Map<string, OpenCodePartView>>(),
        requestMessageID: createOpenCodeID("message"),
        resolvedSessionID: session.opencodeSessionId ?? null,
        assistantMessageID: null,
        completedFromEvent: false,
        eventError: null,
        finalContent: "",
        finalToolCalls: [],
        finalParts: [],
        finalError: null,
      }

      const { handleOpencodeEvent, applyPromptStatusSnapshot } = useEventHandling({
        promptRunId,
        activePromptRef,
        promptState,
        setLiveAssistant,
        updateSession,
        project,
        session,
      })

      opencodeEventHandlersRef.current.add(handleOpencodeEvent)
      removeOpencodeEventHandler = () => {
        opencodeEventHandlersRef.current.delete(handleOpencodeEvent)
      }

      const started = await nativeApi.invoke("opencode_session_prompt_async", {
        directory: project.path,
        sessionID: session.opencodeSessionId,
        title: getOpenCodeCreateTitle(session.name ?? undefined, session.opencodeSessionId),
        prompt: promptText,
        parts: [{ id: createOpenCodeID("part"), type: "text", text: promptText }],
        providerID: model.providerID,
        modelID: model.modelID,
        agent: composerMode,
        variant: modelPower,
        messageID: promptState.requestMessageID,
      })

      if (activePromptRef.current !== promptRunId) return
      promptState.resolvedSessionID = started.sessionID
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
      void titleSync.maybeSyncGeneratedTitle(started.sessionID, 1)

      let lastPollAt = 0
      let pollFailures = 0

      while (activePromptRef.current === promptRunId) {
        if (promptState.completedFromEvent) break

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
            if (pollFailures >= 8 && !promptState.finalContent && promptState.finalToolCalls.length === 0) {
              throw error
            }
          }
        }

        await new Promise((resolve) => setTimeout(resolve, 80))
      }

      if (activePromptRef.current !== promptRunId) return

      try {
        if (promptState.completedFromEvent) {
          await new Promise((resolve) => setTimeout(resolve, 120))
        }

        const finalStatus = await nativeApi.invoke("opencode_session_prompt_status", {
          directory: project.path,
          sessionID: promptState.resolvedSessionID ?? started.sessionID,
          requestMessageID: started.requestMessageID,
        })
        applyPromptStatusSnapshot(finalStatus)
      } catch (error) {
        console.warn("OpenCode final prompt snapshot failed:", error)
      }

      if (promptState.finalParts.length > 0) {
        promptState.finalParts = sortOpenCodeParts(promptState.finalParts)
        promptState.finalContent = extractTextFromParts(promptState.finalParts)
        promptState.finalToolCalls = extractToolCallsFromParts(promptState.finalParts)
      }

      const error = promptState.finalError ? `\n\nOpenCode error: ${describeOpenCodeError(promptState.finalError)}` : ""
      const assistantMessageIDForParts = promptState.assistantMessageID ?? createOpenCodeID("message")
      const assistantParts = promptState.finalParts.length > 0
        ? promptState.finalParts
        : buildAssistantParts(assistantMessageIDForParts, promptState.finalContent, promptState.finalToolCalls)
      await appendAgentMessage(project.id, session.id, {
        role: "assistant",
        content: (promptState.finalContent || "OpenCode completed the request without returning text.") + error,
        toolCalls: promptState.finalToolCalls,
        parts: assistantParts,
        agent: composerMode,
        model: { providerID: model.providerID, modelID: model.modelID },
        error: promptState.finalError ? { name: "OpenCodeError", data: { message: describeOpenCodeError(promptState.finalError) } } : null,
      })

      for (const tc of promptState.finalToolCalls) {
        if (tc.tool === "task") {
          const meta = tc.metadata as Record<string, unknown> | null
          const childSessionId = typeof meta?.sessionId === "string" ? meta.sessionId : typeof meta?.sessionID === "string" ? meta.sessionID : null
          if (childSessionId) {
            void openSubagentSessionAutoCreate(childSessionId, project.id, session.id)
          }
        }
      }

      void titleSync.maybeSyncGeneratedTitle(started.sessionID, 8)
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
  }, [
    project,
    session,
    selectedModel,
    modelOptions,
    modelPower,
    composerMode,
    setIsThinking,
    setLiveAssistant,
    setInput,
    setAttachedFiles,
    autoGrow,
    setPreferredOpencodeModel,
    setPreferredOpencodeVariant,
    openSubagentSessionAutoCreate,
    appendAgentMessage,
    updateSession,
    titleSync,
  ])

  return { handleSubmit, activePromptRef, opencodeEventHandlersRef }
}
