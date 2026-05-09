import { useMemo } from "react"
import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"
import { getFilename } from "@/lib/utils"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function ReadTool(props: ToolProps) {
  const input = useMemo(() => props.toolCall.input as { filePath?: string; cwd?: string } | null, [props.toolCall.input])
  const metadata = useMemo(() => props.toolCall.metadata as { error?: string; preview?: string; loaded?: string[] } | null, [props.toolCall.metadata])
  const filePath = useMemo(() => input?.filePath ?? "", [input])
  const filename = getFilename(filePath)
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="glasses"
      status={status}
      trigger={{
        title: "Read",
        subtitle: filename,
      }}
    >
      {metadata?.error && (
        <div className="text-[11px] text-destructive">{metadata.error}</div>
      )}
      {metadata?.preview && (
        <pre className="thin-scrollbar overflow-auto whitespace-pre-wrap text-[11px] leading-5 rounded-md border border-border/60 bg-background/55 p-2">
          {metadata.preview}
        </pre>
      )}
      {props.toolCall.output && !metadata?.preview && (
        <div className="text-[11px] text-muted-foreground">{props.toolCall.output}</div>
      )}
      {metadata?.loaded && metadata.loaded.length > 0 && (
        <div data-component="tool-loaded-file">
          <span>Loaded: {metadata.loaded.join(", ")}</span>
        </div>
      )}
    </BasicTool>
  )
}
