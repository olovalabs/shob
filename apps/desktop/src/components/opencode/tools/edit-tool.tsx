import { useMemo } from "react"
import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"
import { getFilename } from "@/lib/utils"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function EditTool(props: ToolProps) {
  const input = useMemo(() => props.toolCall.input as { filePath?: string; oldString?: string; newString?: string } | null, [props.toolCall.input])
  const metadata = useMemo(() => props.toolCall.metadata as { diff?: string; error?: string; filediff?: { file: string; before: string; after: string } } | null, [props.toolCall.metadata])
  const filePath = useMemo(() => input?.filePath ?? "", [input])
  const filename = getFilename(filePath)
  const diff = metadata?.diff ?? ""
  const filediff = metadata?.filediff
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="code-lines"
      status={status}
      trigger={{
        title: "Edit",
        subtitle: filename,
      }}
    >
      {metadata?.error && (
        <div className="text-[11px] text-destructive">{metadata.error}</div>
      )}
      {filediff && (
        <div data-component="edit-content">
          <pre className="thin-scrollbar overflow-auto whitespace-pre-wrap text-[11px] leading-5 rounded-md border border-border/60 bg-background/55 p-2 font-mono">
            {filediff.before} → {filediff.after}
          </pre>
        </div>
      )}
      {diff && !filediff && (
        <pre className="thin-scrollbar overflow-auto whitespace-pre-wrap text-[11px] leading-5 rounded-md border border-border/60 bg-background/55 p-2 font-mono">
          {diff}
        </pre>
      )}
    </BasicTool>
  )
}