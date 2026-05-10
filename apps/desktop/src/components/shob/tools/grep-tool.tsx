import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function GrepTool(props: ToolProps) {
  const input = props.toolCall.input as { pattern?: string; path?: string } | null
  const metadata = props.toolCall.metadata as { matches?: number; error?: string } | null
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="magnifying-glass"
      status={status}
      trigger={{
        title: "Grep",
        subtitle: input?.pattern,
        args: input?.path ? [`path=${input.path}`] : [],
      }}
    >
      {metadata?.error && (
        <div className="text-[11px] text-destructive">{metadata.error}</div>
      )}
      {metadata?.matches && metadata.matches > 0 && (
        <div className="text-[11px] text-muted-foreground">
          {metadata.matches} {metadata.matches === 1 ? "match" : "matches"} found
        </div>
      )}
      {props.toolCall.output && !metadata?.matches && (
        <div className="text-[11px] text-muted-foreground">{props.toolCall.output}</div>
      )}
    </BasicTool>
  )
}
