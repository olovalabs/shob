import { Sparkles, SquareTerminal } from "lucide-react"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"

interface NewSessionChoiceDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSelectAgent: () => void
  onSelectTerminal: () => void
}

export function NewSessionChoiceDialog({
  open,
  onOpenChange,
  onSelectAgent,
  onSelectTerminal,
}: NewSessionChoiceDialogProps) {
  const handlePick = (kind: "agent" | "terminal") => {
    onOpenChange(false)
    if (kind === "agent") onSelectAgent()
    else onSelectTerminal()
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="w-[520px] max-w-[calc(100vw-2rem)] gap-5 overflow-hidden border-border/70 bg-popover/95 p-6 backdrop-blur sm:max-w-[520px]">
        <DialogHeader className="gap-1.5">
          <DialogTitle className="text-[17px] font-medium tracking-tight">
            Start a new session
          </DialogTitle>
          <DialogDescription className="text-[13px]">
            Pick how you want to work in this project.
          </DialogDescription>
        </DialogHeader>

        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <button
            type="button"
            onClick={() => handlePick("agent")}
            data-variant="agent"
            className="agent-choice-card agent-glass group flex flex-col items-start gap-3 rounded-xl p-4 text-left transition focus-visible:outline-none focus-visible:ring-3 focus-visible:ring-ring/40"
          >
            <div className="flex h-9 w-9 items-center justify-center rounded-lg border border-border/70 bg-background/60 text-foreground/85 transition-colors group-hover:text-foreground">
              <Sparkles className="h-[18px] w-[18px]" strokeWidth={1.7} />
            </div>
            <div className="space-y-1">
              <div className="flex items-center gap-2">
                <span className="text-[14px] font-medium text-foreground">Agent</span>
                <span className="rounded-[5px] border border-border/70 bg-background/60 px-1.5 py-[1px] text-[10px] font-medium uppercase tracking-[0.14em] text-muted-foreground">
                  New
                </span>
              </div>
              <p className="text-[12px] leading-[1.55] text-muted-foreground">
                Chat with an AI agent that understands your codebase.
              </p>
            </div>
          </button>

          <button
            type="button"
            onClick={() => handlePick("terminal")}
            data-variant="terminal"
            className="agent-choice-card agent-glass group flex flex-col items-start gap-3 rounded-xl p-4 text-left transition focus-visible:outline-none focus-visible:ring-3 focus-visible:ring-ring/40"
          >
            <div className="flex h-9 w-9 items-center justify-center rounded-lg border border-border/70 bg-background/60 text-foreground/85 transition-colors group-hover:text-foreground">
              <SquareTerminal className="h-[18px] w-[18px]" strokeWidth={1.7} />
            </div>
            <div className="space-y-1">
              <span className="text-[14px] font-medium text-foreground">Terminal</span>
              <p className="text-[12px] leading-[1.55] text-muted-foreground">
                Open a shell tab with your preferred CLI tools.
              </p>
            </div>
          </button>
        </div>
      </DialogContent>
    </Dialog>
  )
}
