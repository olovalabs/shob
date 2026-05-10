import { useState } from "react"
import { Copy, Check } from "lucide-react"
import { Message, MessageContent } from "@/components/ai-elements/message"

interface UserMessageProps {
  text: string
  parts?: Array<{ type: string; filename?: string; url?: string }>
  agent?: string
  model?: string
  timestamp?: string
  onCopy?: () => void
}

export function UserMessageDisplay({ text, parts, agent, model, timestamp, onCopy }: UserMessageProps) {
  const [copied, setCopied] = useState(false)

  const attachments = parts?.filter((p) => p.type === "file" && p.filename) ?? []
  const metaHead = [agent ? agent[0].toUpperCase() + agent.slice(1) : "", model ?? ""]
    .filter(Boolean)
    .join("\u00A0\u00B7\u00A0")

  const handleCopy = async () => {
    if (!text) return
    await navigator.clipboard.writeText(text)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
    onCopy?.()
  }

  if (!text && attachments.length === 0 && parts?.some((p) => p.type === "tool") === false) return null

  return (
    <Message from="user" className="py-2">
      {attachments.length > 0 && (
        <div data-slot="user-message-attachments" className="flex flex-wrap gap-1.5 justify-end">
          {attachments.map((file, i) => (
            <div
              key={i}
              data-slot="user-message-attachment"
              data-type={file.url ? "image" : "file"}
              className="inline-flex items-center gap-1 rounded-md border bg-secondary px-1.5 py-0.5 text-xs text-foreground"
            >
              <span data-slot="user-message-attachment-name">{file.filename}</span>
            </div>
          ))}
        </div>
      )}
      {text && (
        <MessageContent>
          <div data-slot="user-message-text" className="whitespace-pre-wrap">{text}</div>
        </MessageContent>
      )}
      {text && (
        <div data-slot="user-message-copy-wrapper" className="flex items-center gap-2 justify-end px-1">
          {(metaHead || timestamp) && (
            <span data-slot="user-message-meta-wrap" className="text-xs text-muted-foreground">
              {metaHead && <span data-slot="user-message-meta">{metaHead}</span>}
              {metaHead && timestamp && <span data-slot="user-message-meta-sep">&nbsp;·&nbsp;</span>}
              {timestamp && <span data-slot="user-message-meta-tail">{timestamp}</span>}
            </span>
          )}
          <button
            onClick={handleCopy}
            className="text-muted-foreground hover:text-foreground transition-colors"
            title={copied ? "Copied" : "Copy message"}
          >
            {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
          </button>
        </div>
      )}
    </Message>
  )
}
