import { useState } from "react"
import { Copy, Check } from "lucide-react"

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

  if (!text && attachments.length === 0) return null

  return (
    <div data-component="user-message">
      {attachments.length > 0 && (
        <div data-slot="user-message-attachments">
          {attachments.map((file, i) => (
            <div key={i} data-slot="user-message-attachment" data-type={file.url ? "image" : "file"}>
              <div data-slot="user-message-attachment-file">
                <span data-slot="user-message-attachment-name">{file.filename}</span>
              </div>
            </div>
          ))}
        </div>
      )}
      {text && (
        <>
          <div data-slot="user-message-body">
            <div data-slot="user-message-text">{text}</div>
          </div>
          <div data-slot="user-message-copy-wrapper">
            {(metaHead || timestamp) && (
              <span data-slot="user-message-meta-wrap">
                {metaHead && <span data-slot="user-message-meta">{metaHead}</span>}
                {metaHead && timestamp && <span data-slot="user-message-meta-sep">\u00A0\u00B7\u00A0</span>}
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
        </>
      )}
    </div>
  )
}
