import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"

interface Todo {
  id: string
  content: string
  status: "pending" | "in_progress" | "completed"
  priority?: "low" | "medium" | "high"
}

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function TodoWriteTool(props: ToolProps) {
  const input = props.toolCall.input as { todos?: Todo[] } | null
  const todos = input?.todos ?? []
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="todo"
      status={status}
      trigger={{
        title: "Todos",
        subtitle: todos.length > 0 ? `Updated ${todos.length} todo${todos.length === 1 ? "" : "s"}` : undefined,
      }}
    />
  )
}
