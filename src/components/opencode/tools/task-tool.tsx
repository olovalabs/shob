import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function TaskTool(props: ToolProps) {
  const input = props.toolCall.input as { description?: string; prompt?: string } | null
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="task"
      status={status}
      trigger={{
        title: "Task",
        subtitle: input?.description,
      }}
    >
      <div data-component="tool-input">&ldquo;{input?.prompt}&rdquo;</div>
      <div data-component="tool-output">
        {props.toolCall.output && (
          <div className="text-[11px] text-muted-foreground">{props.toolCall.output}</div>
        )}
      </div>
    </BasicTool>
  )
}
