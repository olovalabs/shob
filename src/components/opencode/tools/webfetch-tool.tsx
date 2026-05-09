import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function WebFetchTool(props: ToolProps) {
  const input = props.toolCall.input as { url?: string; format?: string } | null
  const metadata = props.toolCall.metadata as { error?: string } | null
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="webfetch"
      status={status}
      trigger={{
        title: "Fetch",
        subtitle: input?.url,
      }}
    >
      {metadata?.error && (
        <div className="text-[11px] text-destructive">{metadata.error}</div>
      )}
      {props.toolCall.output && (
        <pre className="thin-scrollbar overflow-auto whitespace-pre-wrap text-[11px] leading-5 rounded-md border border-border/60 bg-background/55 p-2">
          {props.toolCall.output}
        </pre>
      )}
    </BasicTool>
  )
}
