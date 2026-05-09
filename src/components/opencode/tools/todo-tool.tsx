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

  const priority: Record<string, number> = {
    in_progress: 0,
    pending: 1,
    completed: 2,
  }

  const sortedTodos = [...todos].sort((a, b) => {
    const aPriority = priority[a.status] ?? 1
    const bPriority = priority[b.status] ?? 1
    return aPriority - bPriority
  })

  return (
    <BasicTool
      icon="todo"
      status={status}
      trigger={{
        title: "Todo",
      }}
    >
      {sortedTodos.length > 0 && (
        <ul data-component="todos">
          {sortedTodos.map((todo) => (
            <li key={todo.id} data-slot="item" data-status={todo.status}>
              <span></span>
              {todo.content}
            </li>
          ))}
        </ul>
      )}
    </BasicTool>
  )
}
