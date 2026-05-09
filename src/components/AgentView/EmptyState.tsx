import { GitBranch, Sparkles } from "lucide-react"

const SUGGESTED_PROMPTS = [
  "Explain the structure of this project",
  "Find bugs in the recent changes",
  "Refactor the active file for clarity",
  "Write unit tests for the current module",
] as const

interface EmptyStateProps {
  projectPathParts: { parent: string; name: string }
  lastUpdatedLabel: string | null
  project: { path: string } | null
  onSuggestionClick: (prompt: string) => void
}

export function EmptyState({
  projectPathParts,
  lastUpdatedLabel,
  project,
  onSuggestionClick,
}: EmptyStateProps) {
  return (
    <div className="flex min-h-full w-full items-start justify-center px-6 py-14 sm:py-20">
      <div className="flex w-full max-w-[720px] flex-col items-center text-center">
        <div className="mb-5 flex h-12 w-12 items-center justify-center rounded-2xl border border-border/70 bg-card/70">
          <Sparkles className="h-5 w-5 text-foreground/85" strokeWidth={1.7} />
        </div>

        <h1 className="agent-hero-text mb-3 text-[30px] font-medium leading-tight tracking-tight sm:text-[36px]">
          How can I help you today?
        </h1>

        <p className="mb-8 max-w-[520px] text-[13px] leading-[1.6] text-muted-foreground">
          Ask anything about the project. The agent has access to this workspace
          and can read files, run tools, and coordinate sub-agents.
        </p>

        <div className="mb-10 flex flex-col items-center gap-2 text-[12px] text-muted-foreground">
          {project && (
            <div className="flex max-w-[520px] items-baseline gap-0 break-words text-center">
              <span className="text-muted-foreground/75">{projectPathParts.parent}</span>
              <span className="font-medium text-foreground">{projectPathParts.name}</span>
            </div>
          )}
          <div className="flex items-center gap-1.5 text-muted-foreground/80">
            <GitBranch className="h-3.5 w-3.5" strokeWidth={1.7} />
            <span>main</span>
            {lastUpdatedLabel && (
              <>
                <span className="mx-1 text-muted-foreground/40">{"\u00b7"}</span>
                <span>Last activity {lastUpdatedLabel}</span>
              </>
            )}
          </div>
        </div>

        <div className="grid w-full max-w-[560px] grid-cols-1 gap-2 sm:grid-cols-2">
          {SUGGESTED_PROMPTS.map((prompt) => (
            <button
              key={prompt}
              type="button"
              onClick={() => onSuggestionClick(prompt)}
              className="agent-glass rounded-xl px-3.5 py-3 text-left text-[12.5px] leading-[1.5] text-foreground/82 transition-colors hover:text-foreground"
            >
              {prompt}
            </button>
          ))}
        </div>
      </div>
    </div>
  )
}
