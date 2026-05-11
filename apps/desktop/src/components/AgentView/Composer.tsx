import { useEffect, useState } from "react"
import { ArrowUp, ChevronDown, FileText, Plus, StopCircle } from "lucide-react"
import { TodoDock } from "@/components/TodoDock"
import { ModelSelector } from "./ModelSelector"
import {
  getOpenCodeModelVariantOptions,
  parseOpenCodeModelValue,
  type OpenCodeModelOption,
} from "@/utils/opencode-models"
import type { TodoItem } from "./types"

interface ComposerProps {
  input: string
  setInput: (value: string) => void
  isThinking: boolean
  canSubmit: boolean
  composerMode: "build" | "plan"
  setComposerMode: (mode: "build" | "plan") => void
  selectedModel: string
  setSelectedModel: (value: string) => void
  modelOptions: OpenCodeModelOption[]
  providerStatus: "idle" | "loading" | "ready" | "error"
  modelPower: string
  setModelPower: (value: string) => void
  attachedFiles: File[]
  dockTodos: TodoItem[]
  project: { path: string } | null
  onSubmit: () => void
  onStop: () => void
  onPickFiles: () => void
  onFilesSelected: (event: React.ChangeEvent<HTMLInputElement>) => void
  onRemoveFile?: (index: number) => void
  onDropFiles?: (files: File[]) => void
  onKeyDown: (event: React.KeyboardEvent<HTMLTextAreaElement>) => void
  textareaRef: React.RefObject<HTMLTextAreaElement | null>
  fileInputRef: React.RefObject<HTMLInputElement | null>
  setPreferredOpencodeModel: (providerID: string, modelID: string) => void
  setPreferredOpencodeVariant: (variant: string) => void
}

export function Composer({
  input,
  setInput,
  isThinking,
  canSubmit,
  composerMode,
  setComposerMode,
  selectedModel,
  setSelectedModel,
  modelOptions,
  providerStatus,
  modelPower,
  setModelPower,
  attachedFiles,
  dockTodos,
  project,
  onSubmit,
  onStop,
  onPickFiles,
  onFilesSelected,
  onRemoveFile,
  onDropFiles,
  onKeyDown,
  textareaRef,
  fileInputRef,
  setPreferredOpencodeModel,
  setPreferredOpencodeVariant,
}: ComposerProps) {
  const [isDragging, setIsDragging] = useState(false)
  const selectedOption = modelOptions.find((option) => option.value === selectedModel)
  const variantOptions = getOpenCodeModelVariantOptions(selectedOption)
  const showVariantControl = variantOptions.length > 0

  // Auto-resize textarea to prevent scrollbar when not needed
  useEffect(() => {
    if (textareaRef.current) {
      textareaRef.current.style.height = 'auto'
      textareaRef.current.style.height = `${Math.min(textareaRef.current.scrollHeight, 200)}px`
    }
  }, [input])

  return (
    <div
      data-component="session-prompt-dock"
      className="shrink-0 w-full pb-3 flex flex-col justify-center items-center bg-background-stronger pointer-events-none"
    >
      <div className="w-full px-3 pointer-events-auto md:max-w-[800px] md:mx-auto 2xl:max-w-[1000px]">
        <TodoDock
          todos={dockTodos}
          live={isThinking}
        />
        <div className="relative z-10">
          <div
            className={`relative z-10 overflow-clip rounded-[24px] border backdrop-blur-xl transition-all duration-300 focus-within:shadow-[var(--composer-glow)] ${
              isDragging
                ? "border-[var(--ring)] border-dashed bg-[var(--composer-surface-glass)]"
                : "border-[var(--composer-border)] bg-[var(--composer-surface)] hover:bg-[var(--composer-surface-glass)]"
            }`}
            style={{
              boxShadow: isDragging ? undefined : '0 2px 16px -4px rgba(0,0,0,0.3), inset 0 1px 0 0 rgba(255,255,255,0.04)',
            }}
            onDragOver={(e) => {
              e.preventDefault()
              setIsDragging(true)
            }}
            onDragLeave={() => setIsDragging(false)}
            onDrop={(e) => {
              e.preventDefault()
              setIsDragging(false)
              const files = e.dataTransfer.files
              if (files && files.length > 0 && onDropFiles) {
                onDropFiles(Array.from(files))
              }
            }}
          >
            {isDragging && (
              <div className="absolute inset-0 z-20 flex items-center justify-center bg-[var(--composer-surface)]/60 backdrop-blur-sm rounded-[24px]">
                <span className="text-sm text-[var(--composer-icon)] font-medium">Drop files here</span>
              </div>
            )}
            <textarea
              ref={textareaRef}
              value={input}
              onChange={(event) => setInput(event.target.value)}
              onKeyDown={onKeyDown}
              placeholder={
                project
                  ? "Ask the agent anything\u2026"
                  : "Open a project folder to start chatting"
              }
              disabled={!project}
              rows={1}
              className="w-full max-h-[200px] resize-none overflow-y-auto bg-transparent px-4 pt-3 pb-2.5 text-[15px] leading-relaxed text-foreground placeholder:text-[var(--composer-icon)] outline-none disabled:cursor-not-allowed disabled:opacity-60 composer-scrollbar"
              style={{ minHeight: '44px' }}
            />

            {attachedFiles.length > 0 && (
              <div className="flex flex-wrap items-center gap-1.5 px-4 pb-2">
                {attachedFiles.map((file, i) => {
                  const isImage = file.type.startsWith("image/")
                  return (
                    <span
                      key={file.name + i}
                      className="group/chip inline-flex items-center gap-1 rounded-full border border-[var(--composer-border)] bg-[var(--composer-button-hover)] pl-1 pr-[3px] py-[2px] text-[10px] text-[var(--composer-icon)] max-w-[180px]"
                      title={file.name}
                    >
                      {isImage ? (
                        <img
                          src={URL.createObjectURL(file)}
                          alt={file.name}
                          className="h-4 w-4 rounded object-cover shrink-0"
                        />
                      ) : (
                        <FileText className="h-3 w-3 shrink-0 opacity-70" />
                      )}
                      <span className="truncate">{file.name}</span>
                      {onRemoveFile && (
                        <button
                          type="button"
                          onClick={() => onRemoveFile(i)}
                          className="ml-0.5 flex items-center justify-center h-3.5 w-3.5 rounded-full hover:bg-[var(--composer-border)] transition-colors"
                          title="Remove"
                        >
                          <span className="text-[9px] leading-none">×</span>
                        </button>
                      )}
                    </span>
                  )
                })}
              </div>
            )}

            <div className="flex items-center justify-between px-3 pb-3">
              <div className="flex min-w-0 flex-1 items-center gap-2">
                <input
                  ref={fileInputRef}
                  type="file"
                  multiple
                  className="hidden"
                  onChange={onFilesSelected}
                />

                <button
                  type="button"
                  className="flex items-center justify-center w-7 h-7 rounded-full border border-[var(--composer-border)] text-[var(--composer-icon)] hover:bg-[var(--composer-button-hover)] hover:text-[var(--composer-icon-hover)] hover:border-[var(--composer-border-hover)] transition-all duration-200 focus:outline-none"
                  onClick={onPickFiles}
                  title="Add files"
                >
                  <Plus className="h-3.5 w-3.5" />
                </button>

                <div className="inline-flex items-center rounded-full border border-[var(--composer-border)] p-[2px] bg-[var(--composer-surface)]/50">
                  <button
                    type="button"
                    onClick={() => setComposerMode("build")}
                    className={`rounded-full px-2.5 py-1 text-[11px] font-medium transition-colors ${
                      composerMode === "build"
                        ? "bg-[var(--composer-button-active)] text-[var(--composer-icon-hover)]"
                        : "text-[var(--composer-icon)] hover:text-[var(--composer-icon-hover)]"
                    }`}
                  >
                    Build
                  </button>
                  <button
                    type="button"
                    onClick={() => setComposerMode("plan")}
                    className={`rounded-full px-2.5 py-1 text-[11px] font-medium transition-colors ${
                      composerMode === "plan"
                        ? "bg-[var(--composer-button-active)] text-[var(--composer-icon-hover)]"
                        : "text-[var(--composer-icon)] hover:text-[var(--composer-icon-hover)]"
                    }`}
                  >
                    Plan
                  </button>
                </div>

                <ModelSelector
                  selectedModel={selectedModel}
                  modelOptions={modelOptions}
                  providerStatus={providerStatus}
                  onSelect={(value) => {
                    setSelectedModel(value)
                    const model = parseOpenCodeModelValue(value, modelOptions)
                    if (model.providerID && model.modelID) {
                      setPreferredOpencodeModel(model.providerID, model.modelID)
                    }
                  }}
                  className="h-7 max-w-[140px] flex items-center gap-1 rounded-full border border-[var(--composer-border)] bg-transparent pl-2.5 pr-1.5 text-[12px] text-foreground outline-none transition-colors duration-200 hover:bg-[var(--composer-button-hover)] hover:border-[var(--composer-border-hover)] disabled:cursor-not-allowed disabled:opacity-50"
                />

                {showVariantControl && (
                  <div className="relative max-w-[120px]">
                    <select
                      value={variantOptions.includes(modelPower) ? modelPower : "default"}
                      onChange={(event) => {
                        setModelPower(event.target.value)
                        setPreferredOpencodeVariant(event.target.value)
                      }}
                      className="h-7 w-full appearance-none rounded-full border border-[var(--composer-border)] bg-transparent pl-2.5 pr-6 text-[11px] capitalize text-foreground outline-none transition-colors duration-200 hover:bg-[var(--composer-button-hover)] hover:border-[var(--composer-border-hover)]"
                      title="Thinking effort"
                    >
                      <option value="default">Default</option>
                      {variantOptions.map((variant) => (
                        <option key={variant} value={variant}>
                          {variant}
                        </option>
                      ))}
                    </select>
                    <ChevronDown className="pointer-events-none absolute right-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-[var(--composer-icon)]" />
                  </div>
                )}
              </div>

              <div className="flex items-center">
                {isThinking ? (
                  <button
                    type="button"
                    onClick={onStop}
                    className="pointer-events-auto flex h-7 items-center gap-1.5 rounded-full border border-[var(--composer-border)] px-3 text-[12px] text-foreground hover:bg-[var(--composer-button-hover)] hover:border-[var(--composer-border-hover)] transition-all duration-200 focus:outline-none"
                  >
                    <StopCircle className="h-3.5 w-3.5" />
                    Stop
                  </button>
                ) : (
                  <button
                    type="button"
                    onClick={() => void onSubmit()}
                    disabled={!canSubmit}
                    className={`pointer-events-auto flex h-7 w-7 items-center justify-center rounded-full transition-all duration-200 focus:outline-none ${
                      canSubmit
                        ? "bg-[var(--composer-send-bg)] text-[var(--composer-send-fg)] shadow-[0_0_12px_-2px_color-mix(in_oklch,var(--ring)_35%,transparent)] hover:shadow-[0_0_16px_-1px_color-mix(in_oklch,var(--ring)_45%,transparent)] hover:scale-105 active:scale-95"
                        : "bg-[var(--composer-send-disabled-bg)] text-[var(--composer-send-disabled-fg)] cursor-not-allowed"
                    }`}
                    title="Send (Enter)"
                  >
                    <ArrowUp className="h-4 w-4" strokeWidth={2.2} />
                  </button>
                )}
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
