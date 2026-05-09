import type { ToolCallView } from "@/components/AgentView"
import {
  BashTool,
  ReadTool,
  WriteTool,
  EditTool,
  GlobTool,
  GrepTool,
  ListTool,
  WebFetchTool,
  TodoWriteTool,
  TaskTool,
  FallbackTool,
} from "./index"

interface PartProps {
  toolCall: ToolCallView
  className?: string
}

export function ToolPart(props: PartProps) {
  const { toolCall, className } = props
  const error = toolCall.error

  return (
    <div
      data-component="tool-part"
      data-tool={toolCall.tool}
      data-status={toolCall.status}
      className={className}
    >
      {toolCall.tool === "bash" && <BashTool toolCall={toolCall} />}
      {toolCall.tool === "read" && <ReadTool toolCall={toolCall} />}
      {toolCall.tool === "write" && <WriteTool toolCall={toolCall} />}
      {toolCall.tool === "edit" && <EditTool toolCall={toolCall} />}
      {toolCall.tool === "glob" && <GlobTool toolCall={toolCall} />}
      {toolCall.tool === "grep" && <GrepTool toolCall={toolCall} />}
      {toolCall.tool === "list" && <ListTool toolCall={toolCall} />}
      {toolCall.tool === "webfetch" && <WebFetchTool toolCall={toolCall} />}
      {toolCall.tool === "todowrite" && <TodoWriteTool toolCall={toolCall} />}
      {toolCall.tool === "task" && <TaskTool toolCall={toolCall} />}
      {!["bash", "read", "write", "edit", "glob", "grep", "list", "webfetch", "todowrite", "task"].includes(toolCall.tool) && (
        <FallbackTool toolCall={toolCall} />
      )}
      {error && (
        <div className="mx-2 mb-1.5 rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1 text-[11px] text-destructive">
          {error}
        </div>
      )}
    </div>
  )
}