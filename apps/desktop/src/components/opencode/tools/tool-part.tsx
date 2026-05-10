import type { ToolCallView } from "@/components/AgentView"
import { FallbackTool } from "./fallback-tool"
import { ToolRegistry } from "./tool-registry"

interface PartProps {
  toolCall: ToolCallView
  className?: string
}

export function ToolPart(props: PartProps) {
  const { toolCall, className } = props
  const error = toolCall.error
  const RegisteredTool = ToolRegistry.render(toolCall.tool)
  const input = toolCall.input && typeof toolCall.input === "object" && !Array.isArray(toolCall.input)
    ? toolCall.input as Record<string, unknown>
    : {}
  const metadata = toolCall.metadata ?? {}

  return (
    <div
      data-component="tool-part"
      data-tool={toolCall.tool}
      data-status={toolCall.status}
      className={className}
    >
      {RegisteredTool ? (
        <RegisteredTool
          tool={toolCall.tool}
          status={toolCall.status}
          input={input}
          output={toolCall.output ?? undefined}
          metadata={metadata}
        />
      ) : (
        <FallbackTool toolCall={toolCall} />
      )}
      {error && (
        <div className="mx-2 mb-1.5 rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1 text-[11px] text-destructive">
          {error}
        </div>
      )}
    </div>
  )
}
