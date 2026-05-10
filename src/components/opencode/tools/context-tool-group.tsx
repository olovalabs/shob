import { useMemo, useState } from "react"
import { ChevronDown } from "lucide-react"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { ToolStatusTitle } from "./tool-status-title"
import { AnimatedCountList } from "./tool-count-summary"
import { TextShimmer } from "./text-shimmer"
import { getFilename } from "@/lib/utils"

interface ContextToolPart {
  tool: string
  status: string
  input?: Record<string, unknown>
}

function getDirectory(path: string): string {
  const idx = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"))
  return idx >= 0 ? path.slice(0, idx) : path
}

export function ContextToolGroup({
  parts,
  busy,
}: {
  parts: ContextToolPart[]
  busy?: boolean
}) {
  const [open, setOpen] = useState(false)
  const pending = busy || parts.some((p) => p.status === "pending" || p.status === "running")

  const summary = useMemo(() => {
    const read = parts.filter((p) => p.tool === "read").length
    const search = parts.filter((p) => p.tool === "glob" || p.tool === "grep").length
    const list = parts.filter((p) => p.tool === "list").length
    return { read, search, list }
  }, [parts])

  const toolTrigger = (part: ContextToolPart) => {
    const input = (part.input ?? {}) as Record<string, unknown>
    const filePath = typeof input.filePath === "string" ? input.filePath : undefined
    const path = typeof input.path === "string" ? input.path : "/"
    const pattern = typeof input.pattern === "string" ? input.pattern : undefined
    const include = typeof input.include === "string" ? input.include : undefined
    const offset = typeof input.offset === "number" ? input.offset : undefined
    const limit = typeof input.limit === "number" ? input.limit : undefined

    switch (part.tool) {
      case "read": {
        const args: string[] = []
        if (offset !== undefined) args.push("offset=" + offset)
        if (limit !== undefined) args.push("limit=" + limit)
        return { title: "Read", subtitle: filePath ? getFilename(filePath) : "", args }
      }
      case "list":
        return { title: "List", subtitle: getDirectory(path) }
      case "glob":
        return {
          title: "Glob",
          subtitle: getDirectory(path),
          args: pattern ? ["pattern=" + pattern] : [],
        }
      case "grep": {
        const args: string[] = []
        if (pattern) args.push("pattern=" + pattern)
        if (include) args.push("include=" + include)
        return { title: "Grep", subtitle: getDirectory(path), args }
      }
      default:
        return { title: part.tool, subtitle: "", args: [] }
    }
  }

  return (
    <Collapsible open={open} onOpenChange={setOpen} className="tool-collapsible">
      <CollapsibleTrigger>
        <div data-component="context-tool-group-trigger">
          <span data-slot="context-tool-group-title">
            <span data-slot="context-tool-group-label">
              <ToolStatusTitle
                active={pending}
                activeText="Gathering context..."
                doneText="Gathered context"
                split={false}
              />
            </span>
            <span data-slot="context-tool-group-summary">
              <AnimatedCountList
                items={[
                  { key: "read", count: summary.read, one: "{{count}} read", other: "{{count}} reads" },
                  { key: "search", count: summary.search, one: "{{count}} search", other: "{{count}} searches" },
                  { key: "list", count: summary.list, one: "{{count}} list", other: "{{count}} lists" },
                ]}
                fallback=""
              />
            </span>
          </span>
          <ChevronDown className="h-4 w-4 text-muted-foreground" data-slot="collapsible-arrow-icon" />
        </div>
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div data-component="context-tool-group-list">
          {parts.map((part, index) => {
            const trigger = toolTrigger(part)
            const running = part.status === "pending" || part.status === "running"
            return (
              <div key={`${part.tool}-${part.input?.filePath ?? part.input?.pattern ?? index}`} data-slot="context-tool-group-item">
                <div data-component="tool-trigger">
                  <div data-slot="basic-tool-tool-trigger-content">
                    <div data-slot="basic-tool-tool-info">
                      <div data-slot="basic-tool-tool-info-structured">
                        <div data-slot="basic-tool-tool-info-main">
                          <span data-slot="basic-tool-tool-title">
                            <TextShimmer text={trigger.title} active={running} />
                          </span>
                          {!running && trigger.subtitle && (
                            <span data-slot="basic-tool-tool-subtitle">{trigger.subtitle}</span>
                          )}
                        </div>
                        {!running && (trigger.args?.length ?? 0) > 0 && (
                          <div data-slot="basic-tool-tool-args-block">
                            {(trigger.args ?? []).map((arg, i) => (
                              <span key={i} data-slot="basic-tool-tool-arg">{arg}</span>
                            ))}
                          </div>
                        )}
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            )
          })}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}
