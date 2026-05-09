import { useMemo } from "react"
import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"
import { ChevronDown } from "lucide-react"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function BashTool(props: ToolProps) {
  const input = useMemo(() => props.toolCall.input as { command?: string; description?: string } | null, [props.toolCall.input])
  const metadata = useMemo(() => props.toolCall.metadata as { output?: string; stdout?: string } | null, [props.toolCall.metadata])
  const command = input?.command ?? ""
  const description = input?.description ?? "Shell"
  const output = metadata?.output ?? metadata?.stdout ?? ""
  const status = props.toolCall.status

  const text = useMemo(() => {
    const cmd = command || ""
    const out = output || ""
    return `$ ${cmd}${out ? "\n\n" + out : ""}`
  }, [command, output])

  const handleCopy = async () => {
    if (!text) return
    await navigator.clipboard.writeText(text)
  }

  return (
    <BasicTool
      icon="console"
      status={status}
      trigger={
        <div data-slot="basic-tool-tool-info-structured">
          <div data-slot="basic-tool-tool-info-main">
            <span data-slot="basic-tool-tool-title">Shell</span>
            {!status?.includes("pending") && !status?.includes("running") && description && (
              <span data-slot="basic-tool-tool-subtitle">{description}</span>
            )}
          </div>
        </div>
      }
    >
      <div data-component="bash-output">
        <div data-slot="bash-copy" className="absolute right-1 top-1">
          <button
            onClick={handleCopy}
            className="text-muted-foreground hover:text-foreground"
            title="Copy"
          >
            <ChevronDown className="h-3.5 w-3.5" />
          </button>
        </div>
        <div data-slot="bash-scroll" data-scrollable className="overflow-x-auto">
          <pre data-slot="bash-pre" className="text-[11px] whitespace-pre-wrap">
            <code>{text}</code>
          </pre>
        </div>
      </div>
    </BasicTool>
  )
}
