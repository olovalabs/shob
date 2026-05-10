import { memo, useMemo } from "react"
import { useStore } from "@/store"
import type { AgentViewProps } from "./types"
export type { ToolCallView } from "./types"
import { getDirectoryParts, formatRelativeTime } from "./utils"
import { useTodos } from "./useTodos"
import { MessageGroupRenderer } from "./MessageGroupRenderer"
import { EmptyState } from "./EmptyState"
import { Composer } from "./Composer"
import { useAgentViewHooks } from "./hooks"
import { useSubmit } from "./hooks/useSubmit"

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

  const setPreferredOpencodeModel = useStore((state) => state.setPreferredOpencodeModel)
  const setPreferredOpencodeVariant = useStore((state) => state.setPreferredOpencodeVariant)

  const preferredOpencodeProviderId = useStore((state) => state.preferredOpencodeProviderId)
  const preferredOpencodeModelId = useStore((state) => state.preferredOpencodeModelId)
  const preferredOpencodeVariant = useStore((state) => state.preferredOpencodeVariant)
  const visibleOpencodeModels = useStore((state) => state.visibleOpencodeModels)

  const hooks = useAgentViewHooks({
    isActive,
    sessionId,
    project,
    session,
    preferredOpencodeProviderId: preferredOpencodeProviderId ?? undefined,
    preferredOpencodeModelId: preferredOpencodeModelId ?? undefined,
    preferredOpencodeVariant: preferredOpencodeVariant ?? undefined,
    visibleOpencodeModels,
  })

  const { handleSubmit } = useSubmit({
    project,
    session,
    selectedModel: hooks.selectedModel,
    modelOptions: hooks.modelOptions,
    modelPower: hooks.modelPower,
    composerMode: hooks.composerMode,
    input: hooks.input,
    setIsThinking: hooks.setIsThinking,
    setLiveAssistant: hooks.setLiveAssistant,
    setInput: hooks.setInput,
    setAttachedFiles: hooks.setAttachedFiles,
    autoGrow: hooks.autoGrow,
    setPreferredOpencodeModel,
    setPreferredOpencodeVariant,
    openSubagentSessionAutoCreate: hooks.openSubagentSessionAutoCreate,
  })

  const messages = useMemo(() => session?.agentMessages ?? [], [session?.agentMessages])
  const projectPathParts = useMemo(() => getDirectoryParts(project?.path), [project?.path])
  const lastUpdatedLabel = useMemo(() => {
    const ts = session?.lastActiveAt ?? session?.createdAt ?? null
    return ts ? formatRelativeTime(ts) : null
  }, [session?.lastActiveAt, session?.createdAt])

  const dockTodos = useTodos({
    isThinking: hooks.isThinking,
    liveAssistant: hooks.liveAssistant,
    messages,
  })

  const handleKeyDownWithSubmit = (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault()
      if (hooks.canSubmit) {
        void handleSubmit()
      }
    }
  }

  return (
    <div
      className="agent-container absolute inset-0 flex h-full w-full flex-col"
      data-active={isActive ? "true" : "false"}
      style={{
        display: isActive ? "flex" : "none",
      }}
    >
      <div className="relative w-full h-full min-w-0">
        <div
          ref={hooks.scrollRef}
          className="thin-scrollbar relative min-w-0 w-full h-full overflow-y-auto"
        >
          <div className="min-w-0 w-full">
            {messages.length === 0 ? (
              <EmptyState
                projectPathParts={projectPathParts}
                lastUpdatedLabel={lastUpdatedLabel}
                project={project}
                onSuggestionClick={hooks.handleSuggestion}
              />
            ) : (
              <div
                role="log"
                data-slot="session-turn-list"
                className="flex flex-col items-start justify-start pb-16 transition-[margin] w-full mt-0.5 md:max-w-[800px] md:mx-auto 2xl:max-w-[1000px]"
              >
                <MessageGroupRenderer
                  messages={messages}
                  isThinking={hooks.isThinking}
                  liveAssistant={hooks.liveAssistant}
                />
              </div>
            )}
          </div>
        </div>
      </div>

      <Composer
        input={hooks.input}
        setInput={hooks.setInput}
        isThinking={hooks.isThinking}
        canSubmit={hooks.canSubmit}
        composerMode={hooks.composerMode}
        setComposerMode={hooks.setComposerMode}
        selectedModel={hooks.selectedModel}
        setSelectedModel={hooks.setSelectedModel}
        modelOptions={hooks.modelOptions}
        providerStatus={hooks.providerStatus}
        modelPower={hooks.modelPower}
        setModelPower={hooks.setModelPower}
        attachedFiles={hooks.attachedFiles}
        dockTodos={dockTodos}
        project={project}
        onSubmit={handleSubmit}
        onStop={hooks.handleStop}
        onPickFiles={hooks.handlePickFiles}
        onFilesSelected={hooks.handleFilesSelected}
        onKeyDown={handleKeyDownWithSubmit}
        textareaRef={hooks.textareaRef}
        fileInputRef={hooks.fileInputRef}
        setPreferredOpencodeModel={setPreferredOpencodeModel}
        setPreferredOpencodeVariant={setPreferredOpencodeVariant}
      />
    </div>
  )
}

export const AgentView = memo(
  AgentViewComponent,
  (prev, next) => prev.sessionId === next.sessionId && prev.isActive === next.isActive,
)
