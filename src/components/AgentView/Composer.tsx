import { ArrowUp, ChevronDown, Plus, StopCircle } from "lucide-react"
import { Button } from "@/components/ui/button"
import { TodoDock } from "@/components/TodoDock"
import { ModelSelector } from "./ModelSelector"
import {
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
  onKeyDown,
  textareaRef,
  fileInputRef,
  setPreferredOpencodeModel,
  setPreferredOpencodeVariant,
}: ComposerProps) {
  return (
    <div className="relative z-[1] shrink-0 px-4 pb-5 pt-3 sm:px-6">
      <TodoDock
        todos={dockTodos}
        live={isThinking}
      />
      <div className="mx-auto w-full max-w-[800px] 2xl:max-w-[1000px]">
        <div className="agent-composer relative overflow-hidden rounded-[16px] border border-border/70 bg-card/85">
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
            className="w-full max-h-[200px] min-h-[52px] resize-none overflow-y-auto bg-transparent px-3.5 pb-14 pt-3 text-[14px] leading-[1.5] text-foreground placeholder:text-muted-foreground/70 focus:outline-none disabled:cursor-not-allowed disabled:opacity-60"
          />

          <div
            aria-hidden="true"
            className="pointer-events-none absolute inset-x-0 bottom-[42px] h-7"
            style={{ background: "linear-gradient(to top, var(--card), transparent)" }}
          />

          <div className="absolute inset-x-0 bottom-0 z-[2] flex items-center justify-between gap-2 px-2.5 py-2">
            <input
              ref={fileInputRef}
              type="file"
              multiple
              className="hidden"
              onChange={onFilesSelected}
            />

            <div className="flex min-w-0 flex-1 items-center gap-1 pointer-events-auto">
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                className="h-6 w-6 rounded-full border border-border/70 bg-card/90 text-foreground shadow-xs hover:bg-accent/70"
                onClick={onPickFiles}
                title="Add files"
              >
                <Plus className="h-3.5 w-3.5" />
              </Button>

              <div className="inline-flex items-center gap-0.5 rounded-md border border-border/70 bg-card/90 p-0.5 shadow-xs">
                <button
                  type="button"
                  onClick={() => setComposerMode("build")}
                  className={`rounded px-2 py-1 text-[11px] leading-none transition-colors ${
                    composerMode === "build"
                      ? "bg-accent/85 text-foreground"
                      : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                  }`}
                >
                  Build
                </button>
                <button
                  type="button"
                  onClick={() => setComposerMode("plan")}
                  className={`rounded px-2 py-1 text-[11px] leading-none transition-colors ${
                    composerMode === "plan"
                      ? "bg-accent/85 text-foreground"
                      : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
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
              />

              <div className="relative max-w-[95px]">
                <select
                  value={modelPower}
                  onChange={(event) => {
                    setModelPower(event.target.value)
                    setPreferredOpencodeVariant(event.target.value)
                  }}
                  className="h-7 w-full appearance-none rounded-md border border-border/70 bg-card/90 pl-2 pr-6 text-[11px] text-foreground shadow-xs outline-none transition-colors hover:bg-accent/55"
                  title="Model power"
                >
                  <option value="low">Low</option>
                  <option value="medium">Medium</option>
                  <option value="high">High</option>
                  <option value="xhigh">XHigh</option>
                </select>
                <ChevronDown className="pointer-events-none absolute right-1.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
              </div>

              {attachedFiles.length > 0 && (
                <span className="truncate text-[11px] text-muted-foreground">
                  {attachedFiles.length} file{attachedFiles.length > 1 ? "s" : ""} selected
                </span>
              )}
            </div>

            {isThinking ? (
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={onStop}
                className="pointer-events-auto h-8 gap-1.5 rounded-full px-3"
              >
                <StopCircle className="h-3.5 w-3.5" />
                Stop
              </Button>
            ) : (
              <Button
                type="button"
                size="icon-sm"
                onClick={() => void onSubmit()}
                disabled={!canSubmit}
                className="pointer-events-auto h-8 w-8 rounded-full"
                title="Send (Enter)"
              >
                <ArrowUp className="h-4 w-4" strokeWidth={2.2} />
              </Button>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
