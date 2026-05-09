import { useMemo } from "react"
import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"
import { getFilename } from "@/lib/utils"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function WriteTool(props: ToolProps) {
  const input = useMemo(() => props.toolCall.input as { filePath?: string; content?: string } | null, [props.toolCall.input])
  const metadata = useMemo(() => props.toolCall.metadata as { error?: string; diagnostics?: unknown[] } | null, [props.toolCall.metadata])
  const filePath = useMemo(() => input?.filePath ?? "", [input])
  const filename = getFilename(filePath)
  const content = useMemo(() => input?.content ?? "", [input])
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="code-lines"
      status={status}
      trigger={{
        title: "Write",
        subtitle: filename,
      }}
    >
      {metadata?.error && (
        <div className="text-[11px] text-destructive">{metadata.error as string}</div>
      )}
      {content && (
        <pre className="thin-scrollbar overflow-auto whitespace-pre-wrap text-[11px] leading-5 rounded-md border border-border/60 bg-background/55 p-2">
          {content}
        </pre>
      )}
    </BasicTool>
  )
}