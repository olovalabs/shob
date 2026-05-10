import { useEffect } from "react"
import { useAgentState } from "./useAgentState"
import { useSubagentManagement } from "./useSubagentManagement"
import { useOpenCodeStream } from "./useOpenCodeStream"
import { useProviderModels } from "./useProviderModels"
import { useAutoGrow } from "./useAutoGrow"
import { useUIHandlers } from "./useUIHandlers"

interface UseAgentViewHooksParams {
  isActive: boolean
  sessionId: string
  project: { id: string; path: string; sessions: Array<{ id: string; kind?: string | null; opencodeSessionId?: string | null; parentSessionId?: string | null }> } | null
  session: {
    id: string
    opencodeSessionId?: string | null
    opencodeProviderId?: string | null
    opencodeModelId?: string | null
    opencodeModelVariant?: string | null
  } | null
  preferredOpencodeProviderId: string | undefined
  preferredOpencodeModelId: string | undefined
  preferredOpencodeVariant: string | undefined
  visibleOpencodeModels: string[]
}

export const useAgentViewHooks = ({
  isActive,
  sessionId,
  project,
  session,
  preferredOpencodeProviderId,
  preferredOpencodeModelId,
  preferredOpencodeVariant,
  visibleOpencodeModels,
}: UseAgentViewHooksParams) => {
  const state = useAgentState({ preferredOpencodeVariant })
  const { scrollRef, openSubagentSessionAutoCreate } = useSubagentManagement({
    project,
    session,
    subagentTrackersRef: state.subagentTrackersRef,
    isThinking: state.isThinking,
    liveAssistantContent: state.liveAssistant?.content ?? "",
    liveAssistantToolCallsLength: state.liveAssistant?.toolCalls.length ?? 0,
  })
  useOpenCodeStream({ project })
  const providerModels = useProviderModels({
    isActive,
    project,
    session,
    preferredOpencodeProviderId,
    preferredOpencodeModelId,
    preferredOpencodeVariant,
    visibleOpencodeModels,
  })
  const { textareaRef, autoGrow } = useAutoGrow({ input: state.input })

  useEffect(() => {
    if (!isActive) return
    textareaRef.current?.focus()
  }, [isActive, sessionId])

  const uiHandlers = useUIHandlers({
    input: state.input,
    attachedFiles: state.attachedFiles,
    isThinking: state.isThinking,
    project,
    session,
    setIsThinking: state.setIsThinking,
    setLiveAssistant: state.setLiveAssistant,
    setInput: state.setInput,
    setAttachedFiles: state.setAttachedFiles,
    onSubmit: () => {},
  })

  return {
    input: state.input,
    setInput: state.setInput,
    isThinking: state.isThinking,
    setIsThinking: state.setIsThinking,
    composerMode: state.composerMode,
    setComposerMode: state.setComposerMode,
    selectedModel: providerModels.selectedModel,
    setSelectedModel: providerModels.setSelectedModel,
    modelPower: providerModels.modelPower,
    setModelPower: providerModels.setModelPower,
    modelOptions: providerModels.modelOptions,
    providerStatus: providerModels.providerStatus,
    attachedFiles: state.attachedFiles,
    setAttachedFiles: state.setAttachedFiles,
    liveAssistant: state.liveAssistant,
    setLiveAssistant: state.setLiveAssistant,
    textareaRef,
    fileInputRef: uiHandlers.fileInputRef,
    scrollRef,
    subagentTrackersRef: state.subagentTrackersRef,
    autoGrow,
    canSubmit: uiHandlers.canSubmit,
    handleKeyDown: uiHandlers.handleKeyDown,
    handleSuggestion: uiHandlers.handleSuggestion,
    handlePickFiles: uiHandlers.handlePickFiles,
    handleFilesSelected: uiHandlers.handleFilesSelected,
    handleStop: uiHandlers.handleStop,
    openSubagentSessionAutoCreate,
  }
}
