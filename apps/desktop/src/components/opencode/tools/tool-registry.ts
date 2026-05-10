export interface ToolProps {
  input: Record<string, unknown>
  metadata: Record<string, unknown>
  tool: string
  output?: string
  status?: string
  hideDetails?: boolean
  defaultOpen?: boolean
}

export type ToolComponent = (props: ToolProps) => React.ReactNode

const registry: Record<string, { name: string; render?: ToolComponent }> = {}

export function registerTool(input: { name: string; render?: ToolComponent }) {
  registry[input.name] = input
  return input
}

export function getTool(name: string) {
  return registry[name]?.render
}

export const ToolRegistry = {
  register: registerTool,
  render: getTool,
}
