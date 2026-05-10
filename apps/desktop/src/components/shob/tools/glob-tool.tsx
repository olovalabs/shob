import { useMemo } from "react"
import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function GlobTool(props: ToolProps) {
  const input = useMemo(() => props.toolCall.input as { path?: string; pattern?: string } | null, [props.toolCall.input])
  const metadata = useMemo(() => props.toolCall.metadata as { count?: number; error?: string } | null, [props.toolCall.metadata])
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="magnifying-glass-menu"
      status={status}
      trigger={{
        title: "Glob",
        subtitle: input?.pattern,
        args: input?.path ? [`path=${input.path}`] : [],
      }}
    >
      {metadata?.error && (
        <div className="text-[11px] text-destructive">{metadata.error}</div>
      )}
      {metadata?.count && metadata.count > 0 && (
        <div className="text-[11px] text-muted-foreground">
          {metadata.count} {metadata.count === 1 ? "match" : "matches"} found
        </div>
      )}
      {props.toolCall.output && !metadata?.count && (
        <div className="text-[11px] text-muted-foreground">{props.toolCall.output}</div>
      )}
    </BasicTool>
  )
}