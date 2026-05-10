import { useMemo, useState } from "react"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Copy, AlertCircle } from "lucide-react"

export function ToolErrorCard({
  tool,
  error,
  title,
  defaultOpen = false,
  subtitle,
}: {
  tool: string
  error: string
  title?: string
  defaultOpen?: boolean
  subtitle?: string
}) {
  const [open, setOpen] = useState(defaultOpen)
  const [copied, setCopied] = useState(false)

  const name = useMemo(() => {
    if (title) return title
    const map: Record<string, string> = {
      read: "Read",
      list: "List",
      glob: "Glob",
      grep: "Grep",
      task: "Task",
      webfetch: "Web Fetch",
      websearch: "Web Search",
      bash: "Shell",
      apply_patch: "Patch",
      question: "Questions",
    }
    return map[tool] ?? tool
  }, [tool, title])

  const cleaned = useMemo(() => error.replace(/^Error:\s*/, "").trim(), [error])

  const tail = useMemo(() => {
    const prefix = `${tool} `
    return cleaned.startsWith(prefix) ? cleaned.slice(prefix.length) : cleaned
  }, [cleaned, tool])

  const body = useMemo(() => {
    const parts = tail.split(": ")
    if (parts.length <= 1) return cleaned
    return parts.slice(1).join(": ").trim() || cleaned
  }, [tail, cleaned])

  const displaySubtitle = useMemo(() => {
    if (subtitle) return subtitle
    const parts = tail.split(": ")
    if (parts.length <= 1) return "Failed"
    const head = (parts[0] ?? "").trim()
    if (!head) return "Failed"
    return head[0] ? head[0].toUpperCase() + head.slice(1) : "Failed"
  }, [subtitle, tail])

  const handleCopy = async () => {
    if (!cleaned) return
    await navigator.clipboard.writeText(cleaned)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  return (
    <div data-component="card" data-kind="tool-error-card" data-open={open ? "true" : "false"}>
      <Collapsible
        className="tool-collapsible"
        data-open={open ? "true" : "false"}
        open={open}
        onOpenChange={setOpen}
      >
        <CollapsibleTrigger>
          <div data-component="tool-trigger">
            <div data-slot="basic-tool-tool-trigger-content">
              <span data-slot="basic-tool-tool-indicator" data-component="tool-error-card-icon">
                <AlertCircle className="h-4 w-4" />
              </span>
              <div data-slot="basic-tool-tool-info">
                <div data-slot="basic-tool-tool-info-structured">
                  <div data-slot="basic-tool-tool-info-main">
                    <span data-slot="basic-tool-tool-title">{name}</span>
                    <span data-slot="basic-tool-tool-subtitle">{displaySubtitle}</span>
                  </div>
                </div>
              </div>
            </div>
            <ChevronDown className="h-4 w-4" data-slot="collapsible-arrow-icon" />
          </div>
        </CollapsibleTrigger>
        <CollapsibleContent>
          <div data-slot="tool-error-card-content">
            {open && (
              <div data-slot="tool-error-card-copy">
                <button
                  onClick={(e) => {
                    e.stopPropagation()
                    handleCopy()
                  }}
                  className="text-muted-foreground hover:text-foreground"
                  aria-label={copied ? "Copied" : "Copy error"}
                >
                  <Copy className="h-3.5 w-3.5" />
                </button>
              </div>
            )}
            {body && <div className="text-[13px] leading-relaxed">{body}</div>}
          </div>
        </CollapsibleContent>
      </Collapsible>
    </div>
  )
}

function ChevronDown({ className, ...props }: React.ComponentPropsWithoutRef<"svg">) {
  return (
    <svg
      className={className}
      {...props}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="m6 9 6 6 6-6" />
    </svg>
  )
}
