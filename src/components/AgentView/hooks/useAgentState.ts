import { useRef, useState } from "react"
import type {
  ElectronOpencodeEventSubscription,
} from "@/electron"
import type { SubagentTracker, LiveAssistantState } from "../types"

interface UseAgentStateParams {
  preferredOpencodeVariant: string | undefined
}

export const useAgentState = ({ preferredOpencodeVariant }: UseAgentStateParams) => {
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const opencodeEventSubscriptionRef = useRef<ElectronOpencodeEventSubscription | null>(null)
  const subagentTrackersRef = useRef<Map<string, SubagentTracker>>(new Map())

  const [input, setInput] = useState("")
  const [isThinking, setIsThinking] = useState(false)
  const [composerMode, setComposerMode] = useState<"build" | "plan">("build")
  const [selectedModel, setSelectedModel] = useState("")
  const [modelPower, setModelPower] = useState(preferredOpencodeVariant || "high")
  const [modelOptions, setModelOptions] = useState<Array<{ value: string; label: string; shortLabel: string; providerID: string; providerName: string; modelID: string }>>([])
  const [providerStatus, setProviderStatus] = useState<"idle" | "loading" | "ready" | "error">("idle")
  const [attachedFiles, setAttachedFiles] = useState<File[]>([])
  const [liveAssistant, setLiveAssistant] = useState<LiveAssistantState | null>(null)

  return {
    textareaRef,
    fileInputRef,
    scrollRef,
    opencodeEventSubscriptionRef,
    subagentTrackersRef,
    input,
    setInput,
    isThinking,
    setIsThinking,
    composerMode,
    setComposerMode,
    selectedModel,
    setSelectedModel,
    modelPower,
    setModelPower,
    modelOptions,
    setModelOptions,
    providerStatus,
    setProviderStatus,
    attachedFiles,
    setAttachedFiles,
    liveAssistant,
    setLiveAssistant,
  }
}
