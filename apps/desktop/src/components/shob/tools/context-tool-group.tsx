import { useMemo, useState } from "react"
import { Task, TaskTrigger, TaskContent, TaskItem, TaskItemFile } from "@/components/ai-elements/task"
import { TextShimmer } from "./text-shimmer"
import { getFilename } from "@/lib/utils"
import { SearchIcon, CheckCircle2Icon, ListIcon, FileTextIcon } from "lucide-react"

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

export function ContextToolGroup({
  parts,
  busy,
}: {
  parts: ToolPart[]
  busy?: boolean
}) {
  const pending = Boolean(busy || parts.some((p) => {
    const status = p.state?.status ?? p.status
    return status === "pending" || status === "running"
  }))
  const [open, setOpen] = useState(pending)

  const summary = useMemo(() => {
    const read = parts.filter((p) => p.tool === "read").length
    const search = parts.filter((p) => p.tool === "glob" || p.tool === "grep").length
    const list = parts.filter((p) => p.tool === "list").length
    return { read, search, list }
  }, [parts])

  const toolLine = (part: ToolPart) => {
    const input = (part.state?.input ?? part.input ?? {}) as Record<string, unknown>
    const tool = part.tool || "tool"
    const filePath = typeof input.filePath === "string" ? input.filePath : ""
    const path = typeof input.path === "string" ? input.path : "/"
    const pattern = typeof input.pattern === "string" ? input.pattern : ""

    switch (tool) {
      case "read":
        return (
          <TaskItem key={part.id}>
            Read <TaskItemFile>{filePath ? getFilename(filePath) : "file"}</TaskItemFile>
          </TaskItem>
        )
      case "list":
        return (
          <TaskItem key={part.id}>
            Listed <span className="text-foreground">{getDirectory(path)}</span>
          </TaskItem>
        )
      case "glob":
        return (
          <TaskItem key={part.id}>
            Matched <span className="text-foreground">{pattern || path || "files"}</span>
          </TaskItem>
        )
      case "grep":
        return (
          <TaskItem key={part.id}>
            Searched <span className="text-foreground">{pattern || "text"}</span>
          </TaskItem>
        )
      default:
        return null
    }
  }

  const titleParts = [
    summary.read ? `${summary.read} read${summary.read === 1 ? "" : "s"}` : "",
    summary.search ? `${summary.search} search${summary.search === 1 ? "" : "es"}` : "",
    summary.list ? `${summary.list} list${summary.list === 1 ? "" : "s"}` : "",
  ].filter(Boolean)

  const title = pending
    ? "Gathering context..."
    : titleParts.length > 0
      ? `Gathered ${titleParts.join(", ")}`
      : "Gathered context"
  let Icon = SearchIcon
  if (!pending && summary.read > 0 && summary.search === 0 && summary.list === 0) {
    Icon = FileTextIcon
  } else if (!pending && summary.list > 0 && summary.search === 0) {
    Icon = ListIcon
  } else if (!pending) {
    Icon = CheckCircle2Icon
  }

  return (
    <Task open={open} onOpenChange={setOpen} className="my-1">
      <TaskTrigger title={title}>
        <div className="flex w-full cursor-pointer items-center gap-2 text-muted-foreground text-sm transition-colors hover:text-foreground">
          <Icon className="size-4" />
          <p className="text-sm">{title}</p>
        </div>
      </TaskTrigger>
      <TaskContent>
        <div className="flex flex-col gap-1.5">
          {parts.map((part) => toolLine(part))}
          {pending && (
            <TaskItem>
              <TextShimmer text="Working..." active={true} />
            </TaskItem>
          )}
        </div>
      </TaskContent>
    </Task>
  )
}
