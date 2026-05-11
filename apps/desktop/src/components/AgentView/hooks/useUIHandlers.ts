import { useRef } from "react"
import { nativeApi } from "@/services/native"
import type { LiveAssistantState } from "../types"

interface UseUIHandlersParams {
  input: string
  attachedFiles: File[]
  isThinking: boolean
  project: { path: string } | null
  session: { opencodeSessionId?: string | null } | null
  setIsThinking: (value: boolean) => void
  setLiveAssistant: (value: LiveAssistantState | null) => void
  setInput: (value: string) => void
  setAttachedFiles: React.Dispatch<React.SetStateAction<File[]>>
  onSubmit: () => void
}

export const useUIHandlers = ({
  input,
  attachedFiles,
  isThinking,
  project,
  session,
  setIsThinking,
  setLiveAssistant,
  setInput,
  setAttachedFiles,
  onSubmit,
}: UseUIHandlersParams) => {
  const fileInputRef = useRef<HTMLInputElement>(null)

  const hasSelectedConnectedModel = Boolean(input.trim().length > 0 || attachedFiles.length > 0)
  const computedCanSubmit =
    (input.trim().length > 0 || attachedFiles.length > 0) &&
    !isThinking &&
    !!project &&
    !!session &&
    hasSelectedConnectedModel

  const handleKeyDown = (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault()
      void onSubmit()
    }
  }

  const handleSuggestion = (prompt: string) => {
    setInput(prompt)
    requestAnimationFrame(() => {
    })
  }

  const handlePickFiles = () => fileInputRef.current?.click()

  const handleFilesSelected = (event: React.ChangeEvent<HTMLInputElement>) => {
    const files = event.target.files
    if (!files || files.length === 0) return
    setAttachedFiles((prev) => [...prev, ...Array.from(files)])
    event.target.value = ""
  }

  const handleRemoveFile = (index: number) => {
    setAttachedFiles((prev) => prev.filter((_, i) => i !== index))
  }

  const handleStop = () => {
    if (project?.path && session?.opencodeSessionId) {
      void nativeApi.invoke("opencode_session_abort", {
        directory: project.path,
        sessionID: session.opencodeSessionId,
      }).catch((error) => console.warn("Failed to abort OpenCode session:", error))
    }
    setIsThinking(false)
    setLiveAssistant(null)
  }

  return {
    fileInputRef,
    canSubmit: computedCanSubmit,
    handleKeyDown,
    handleSuggestion,
    handlePickFiles,
    handleFilesSelected,
    handleRemoveFile,
    handleStop,
  }
}
