import type { OpenCodeModelOption } from "@/utils/opencode-models"
import type { OpenCodePartView, ToolCallView, LiveAssistantState } from "../../types"

export type PromptStatus = {
  assistantMessageID?: string | null
  parts?: unknown[]
  content?: string
  toolCalls?: ToolCallView[]
  error?: unknown
  completed?: boolean
}

export type PromptState = {
  partsByMessageID: Map<string, Map<string, OpenCodePartView>>
  requestMessageID: string
  resolvedSessionID: string | null
  assistantMessageID: string | null
  completedFromEvent: boolean
  eventError: unknown
  finalContent: string
  finalToolCalls: ToolCallView[]
  finalParts: OpenCodePartView[]
  finalError: unknown
}

export interface UseSubmitParams {
  project: { id: string; path: string } | null
  session: {
    id: string
    name?: string | null
    opencodeSessionId?: string | null
    opencodeProviderId?: string | null
    opencodeModelId?: string | null
    opencodeModelVariant?: string | null
  } | null
  selectedModel: string
  modelOptions: OpenCodeModelOption[]
  modelPower: string
  composerMode: "build" | "plan"
  setIsThinking: (value: boolean) => void
  setLiveAssistant: (value: LiveAssistantState | null) => void
  setInput: (value: string) => void
  setAttachedFiles: (value: File[]) => void
  autoGrow: () => void
  setPreferredOpencodeModel: (providerID: string, modelID: string) => void
  setPreferredOpencodeVariant: (variant: string) => void
  openSubagentSessionAutoCreate: (opencodeSessionID: string, projectId: string, parentSessionId: string) => Promise<void>
}
