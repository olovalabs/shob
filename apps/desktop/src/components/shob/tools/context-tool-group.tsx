import { useMemo, useState, useCallback } from "react"
import { Collapsible, CollapsibleTrigger, CollapsibleContent } from "@/components/ui/collapsible"
import { TextShimmer } from "./text-shimmer"
import { getFilename } from "@/lib/utils"
import { ChevronDown } from "lucide-react"

interface ToolPart {
  id: string
  tool?: string
  status?: string
  input?: unknown
  state?: {
    status?: string
    input?: unknown
  }
}

function getDirectory(path: string): string {
  const idx = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"))
  return idx >= 0 ? path.slice(0, idx) : path
}

function contextToolTrigger(part: ToolPart) {
  const input = (part.state?.input ?? part.input ?? {}) as Record<string, unknown>
  const path = typeof input.path === "string" ? input.path : "/"
  const filePath = typeof input.filePath === "string" ? input.filePath : undefined
  const pattern = typeof input.pattern === "string" ? input.pattern : undefined
  const include = typeof input.include === "string" ? input.include : undefined
  const offset = typeof input.offset === "number" ? input.offset : undefined
  const limit = typeof input.limit === "number" ? input.limit : undefined

  const tool = part.tool ?? ""

  switch (tool) {
    case "read": {
      const args: string[] = []
      if (offset !== undefined) args.push(`offset=${offset}`)
      if (limit !== undefined) args.push(`limit=${limit}`)
      return {
        title: "Read",
        subtitle: filePath ? getFilename(filePath) : "",
        args,
      }
    }
    case "list":
      return {
        title: "List",
        subtitle: getDirectory(path),
      }
    case "glob": {
      const args: string[] = []
      return {
        title: "Glob",
        subtitle: getDirectory(path),
        args: pattern ? [`pattern=${pattern}`] : [],
      }
    }
    case "grep": {
      const args: string[] = []
      if (pattern) args.push(`pattern=${pattern}`)
      if (include) args.push(`include=${include}`)
      return {
        title: "Grep",
        subtitle: getDirectory(path),
        args,
      }
    }
    default:
      return {
        title: tool,
        subtitle: "",
        args: [] as string[],
      }
  }
}

function contextToolSummary(parts: ToolPart[]) {
  const read = parts.filter((part) => part.tool === "read").length
  const search = parts.filter((part) => part.tool === "glob" || part.tool === "grep").length
  const list = parts.filter((part) => part.tool === "list").length

  const items: string[] = []
  if (read) items.push(`${read} read${read !== 1 ? "s" : ""}`)
  if (search) items.push(`${search} search${search !== 1 ? "es" : ""}`)
  if (list) items.push(`${list} list${list !== 1 ? "s" : ""}`)
  return items.join(", ")
}

export function ContextToolGroup({
  parts,
  busy,
}: {
  parts: ToolPart[]
  busy?: boolean
}) {
  const pending = Boolean(
    busy ||
      parts.some((p) => {
        const status = p.state?.status ?? p.status
        return status === "pending" || status === "running"
      }),
  )
  const [open, setOpen] = useState(pending)

  const summary = useMemo(() => contextToolSummary(parts), [parts])

  return (
    <Collapsible
      open={open}
      onOpenChange={setOpen}
      className="tool-collapsible"
      data-component="collapsible"
      data-variant="ghost"
    >
      <CollapsibleTrigger asChild>
        <div data-component="context-tool-group-trigger">
          <span
            data-slot="context-tool-group-title"
            className="min-w-0 flex items-center gap-2 text-14-medium text-text-strong"
          >
            <span data-slot="context-tool-group-label" className="shrink-0">
              <TextShimmer
                text={pending ? "Gathering context..." : "Gathered context"}
                active={pending}
              />
            </span>
            <span
              data-slot="context-tool-group-summary"
              className="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap font-normal text-text-base"
            >
              {summary}
            </span>
          </span>
          <ChevronDown
            data-slot="collapsible-arrow"
            className={`size-3.5 transition-transform duration-150 ${open ? "rotate-0" : "-rotate-90"}`}
          />
        </div>
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div data-component="context-tool-group-list">
          {parts.map((part) => {
            const trigger = contextToolTrigger(part)
            const running = part.state?.status === "pending" || part.state?.status === "running" || part.status === "pending" || part.status === "running"
            return (
              <div key={part.id} data-slot="context-tool-group-item">
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
                          {!running &&
                            trigger.args &&
                            trigger.args.length > 0 &&
                            trigger.args.map((arg, i) => (
                              <span key={i} data-slot="basic-tool-tool-arg">
                                {arg}
                              </span>
                            ))}
                        </div>
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            )
          })}
          {pending && (
            <div data-slot="context-tool-group-item">
              <div data-component="tool-trigger">
                <div data-slot="basic-tool-tool-trigger-content">
                  <div data-slot="basic-tool-tool-info">
                    <div data-slot="basic-tool-tool-info-structured">
                      <div data-slot="basic-tool-tool-info-main">
                        <span data-slot="basic-tool-tool-title">
                          <TextShimmer text="Working..." active={true} />
                        </span>
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            </div>
          )}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}
