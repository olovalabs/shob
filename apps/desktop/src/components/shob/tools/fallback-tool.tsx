import type { ToolCallView } from "@/components/AgentView"
import { BasicTool } from "./basic-tool"

interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export function FallbackTool(props: ToolProps) {
  const input = props.toolCall.input as Record<string, unknown> | null | undefined
  const status = props.toolCall.status

  return (
    <BasicTool
      icon="mcp"
      status={status}
      trigger={{
        title: `${props.toolCall.tool} called`,
        subtitle: label(input),
        args: args(input),
      }}
    />
  )
}

function label(input: Record<string, unknown> | null | undefined): string | undefined {
  const keys = ["description", "query", "url", "filePath", "path", "pattern", "name"]
  return keys.map((key) => input?.[key]).find((value): value is string => typeof value === "string" && value.length > 0)
}

function args(input: Record<string, unknown> | null | undefined): string[] {
  if (!input) return []
  const skip = new Set(["description", "query", "url", "filePath", "path", "pattern", "name"])
  return Object.entries(input)
    .filter(([key]) => !skip.has(key))
    .flatMap(([key, value]) => {
      if (typeof value === "string") return [`${key}=${value}`]
      if (typeof value === "number") return [`${key}=${value}`]
      if (typeof value === "boolean") return [`${key}=${value}`]
      return []
    })
    .slice(0, 3)
}
