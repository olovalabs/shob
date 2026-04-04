import { Bot } from "lucide-react"
import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar"
import { getCliDisplayLabel, getCliIconAsset, getCliFallbackText } from "../config/cli-ui"

interface CliAvatarProps {
  cliId?: string | null
  label?: string | null
  size?: "sm" | "md"
  className?: string
}

const sizeClasses = {
  sm: "h-4 w-4",
  md: "h-7 w-7",
} as const

export function CliAvatar({ cliId, label, size = "md", className = "" }: CliAvatarProps) {
  const logo = getCliIconAsset(cliId)
  const resolvedLabel = label ?? getCliDisplayLabel(cliId) ?? "CLI"
  const sizeClass = sizeClasses[size]

  return (
    <Avatar className={`${sizeClass} ${className}`} data-size={size}>
      {logo && <AvatarImage src={logo} alt={resolvedLabel} />}
      <AvatarFallback className="text-[8px] bg-muted/50">
        {cliId ? (
          <Bot className={size === "sm" ? "h-3 w-3" : "h-4 w-4"} strokeWidth={1.9} />
        ) : (
          getCliFallbackText(cliId, resolvedLabel)
        )}
      </AvatarFallback>
    </Avatar>
  )
}
