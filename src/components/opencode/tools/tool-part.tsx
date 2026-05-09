import type { ToolCallView } from "@/components/AgentView"
import { BashTool } from "./bash-tool"
import { ReadTool } from "./read-tool"
import { WriteTool } from "./write-tool"
import { EditTool } from "./edit-tool"
import { GlobTool } from "./glob-tool"
import { GrepTool } from "./grep-tool"
import { ListTool } from "./list-tool"
import { WebFetchTool } from "./webfetch-tool"
import { TodoWriteTool } from "./todo-tool"
import { TaskTool } from "./task-tool"
import { FallbackTool } from "./fallback-tool"

interface PartProps {
  toolCall: ToolCallView
  className?: string
}

export function ToolPart(props: PartProps) {
  const { toolCall, className } = props
  const error = toolCall.error

  const renderTool = () => {
    switch (toolCall.tool) {
      case "bash": return <BashTool toolCall={toolCall} />
      case "read": return <ReadTool toolCall={toolCall} />
      case "write": return <WriteTool toolCall={toolCall} />
      case "edit": return <EditTool toolCall={toolCall} />
      case "glob": return <GlobTool toolCall={toolCall} />
      case "grep": return <GrepTool toolCall={toolCall} />
      case "list": return <ListTool toolCall={toolCall} />
      case "webfetch": return <WebFetchTool toolCall={toolCall} />
      case "todowrite": return <TodoWriteTool toolCall={toolCall} />
      case "task": return <TaskTool toolCall={toolCall} />
      default: return <FallbackTool toolCall={toolCall} />
    }
  }

  return (
    <div
      data-component="tool-part"
      data-tool={toolCall.tool}
      data-status={toolCall.status}
      className={className}
    >
      {renderTool()}
      {error && (
        <div className="mx-2 mb-1.5 rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1 text-[11px] text-destructive">
          {error}
        </div>
      )}
    </div>
  )
}
