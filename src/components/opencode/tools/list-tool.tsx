import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"
import { stripWorkingDirectory } from "./common"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function ListTool(props: ToolProps) {
  const input = props.toolCall.input as { path?: string } | null
  const metadata = props.toolCall.metadata as { cwd?: string } | null
  const path = stripWorkingDirectory(input?.path, metadata?.cwd)
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="list"
      status={status}
      trigger={{
        title: "LS",
        subtitle: path || input?.path,
      }}
    >
      {props.toolCall.output && (
        <div className="text-[11px] text-muted-foreground">{props.toolCall.output}</div>
      )}
    </BasicTool>
  )
}
