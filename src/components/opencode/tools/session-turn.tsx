import { useMemo, useState } from "react"
import { UserMessageDisplay } from "./message-user"
import { AssistantMessageDisplay } from "./message-assistant"
import { DiffChanges } from "./diff-changes"
import { TextShimmer } from "./text-shimmer"
import { Accordion, AccordionContent, AccordionItem, AccordionTrigger } from "@/components/ui/accordion"

interface SnapshotFileDiff {
  file: string
  additions?: number
  deletions?: number
  contents?: string
}

interface Message {
  id: string
  role: string
  content?: string
  toolCalls?: Array<{
    id?: string | null
    callID?: string | null
    tool: string
    status: string
    title?: string | null
    input?: unknown
    output?: string | null
    error?: string | null
    raw?: string | null
    metadata?: Record<string, unknown> | null
    attachments?: unknown[] | null
    startedAt?: number | null
    endedAt?: number | null
    compactedAt?: number | null
  }>
  agent?: string
  model?: { providerID?: string; modelID?: string }
  time?: { created?: number; completed?: number }
  error?: { name?: string; data?: { message?: string } }
  summary?: { diffs?: SnapshotFileDiff[] }
  parts?: Array<{
    id: string
    type: string
    text?: string
    tool?: string
    state?: {
      status?: string
      input?: unknown
      output?: string
      error?: string
      metadata?: Record<string, unknown>
    }
  }>
}

interface SessionTurnProps {
  messages: Message[]
  userMessageIndex: number
  assistantMessages?: Message[]
  showReasoningSummaries?: boolean
  working?: boolean
  error?: string | null
}

const MAX_FILES = 10

function getDirectory(filePath: string): string {
  const idx = Math.max(filePath.lastIndexOf("/"), filePath.lastIndexOf("\\"))
  return idx >= 0 ? filePath.slice(0, idx) : ""
}

function getFilename(filePath: string): string {
  const idx = Math.max(filePath.lastIndexOf("/"), filePath.lastIndexOf("\\"))
  return idx >= 0 ? filePath.slice(idx + 1) : filePath
}

function formatTimestamp(ts?: number): string {
  if (!ts) return ""
  return new Date(ts).toLocaleTimeString()
}

export function SessionTurn({
  messages,
  userMessageIndex,
  assistantMessages = [],
  showReasoningSummaries = true,
  working = false,
  error = null,
}: SessionTurnProps) {
  const userMessage = messages[userMessageIndex]
  const [showAll, setShowAll] = useState(false)
  const [expanded, setExpanded] = useState<string[]>([])

  const diffs = useMemo(() => {
    const files = userMessage?.summary?.diffs
    if (!files?.length) return []
    const seen = new Set<string>()
    return files
      .reduceRight<SnapshotFileDiff[]>((result, diff) => {
        if (seen.has(diff.file)) return result
        seen.add(diff.file)
        result.push(diff)
        return result
      }, [])
      .reverse()
  }, [userMessage])

  // Combine all assistant parts
  const allAssistantParts = useMemo(() => {
    return assistantMessages.flatMap((am) =>
      (am.parts ?? []).map((p) => ({
        ...p,
        messageID: am.id,
      }))
    )
  }, [assistantMessages])

  const edited = diffs.length
  const overflow = Math.max(0, edited - MAX_FILES)
  const visible = showAll ? diffs : diffs.slice(0, MAX_FILES)

  if (!userMessage) return null

  const userParts = userMessage.parts?.map((p) => ({
    ...p,
    messageID: userMessage.id,
  })) ?? []

  // Check if there's a compaction divider
  const compaction = userParts.find((p) => p.type === "compaction")

  // Check if interrupted
  const interrupted = assistantMessages.some((m) => m.error?.name === "MessageAbortedError")

  const divider = compaction ? "Earlier messages compacted" : interrupted ? "Interrupted" : ""

  return (
    <div data-component="session-turn">
      <div data-slot="session-turn-content">
        {/* User message */}
        <div data-slot="session-turn-message-container">
          <div data-slot="session-turn-message-content" aria-live="off">
            <UserMessageDisplay
              text={userMessage.content ?? ""}
              parts={userParts}
              agent={userMessage.agent}
              model={userMessage.model?.modelID}
              timestamp={formatTimestamp(userMessage.time?.created)}
            />
          </div>

          {/* Divider (compaction or interruption) */}
          {divider && (
            <div data-slot="session-turn-compaction">
              <div data-component="compaction-part">
                <div data-slot="compaction-part-divider">
                  <span data-slot="compaction-part-line" />
                  <span data-slot="compaction-part-label">{divider}</span>
                  <span data-slot="compaction-part-line" />
                </div>
              </div>
            </div>
          )}

          {/* Assistant messages */}
          {assistantMessages.length > 0 && (
            <div data-slot="session-turn-assistant-content">
              <AssistantMessageDisplay
                parts={allAssistantParts}
                messageId={assistantMessages[0]?.id ?? ""}
                working={working}
                showReasoningSummaries={showReasoningSummaries}
              />
            </div>
          )}

          {/* Thinking indicator */}
          {working && allAssistantParts.length === 0 && (
            <div data-slot="session-turn-thinking">
              <TextShimmer text="Thinking" />
            </div>
          )}

          {/* Error */}
          {error && (
            <div className="error-card">
              {error}
            </div>
          )}

          {/* Diffs */}
          {edited > 0 && !working && (
            <div data-slot="session-turn-diffs" data-component="session-turn-diffs-group" data-show-all={showAll || undefined}>
              <div data-slot="session-turn-diffs-header">
                <span data-slot="session-turn-diffs-label">
                  {edited} changed {edited === 1 ? "file" : "files"}
                </span>
                <DiffChanges changes={diffs.map((d) => ({ additions: d.additions ?? 0, deletions: d.deletions ?? 0 }))} />
                {overflow > 0 && (
                  <span data-slot="session-turn-diffs-toggle" onClick={() => setShowAll(!showAll)}>
                    {showAll ? "Show less" : "Show all"}
                  </span>
                )}
              </div>
              <div data-component="session-turn-diffs-content">
                <Accordion type="multiple" value={expanded} onValueChange={setExpanded}>
                  {visible.map((diff) => (
                    <AccordionItem key={diff.file} value={diff.file}>
                      <AccordionTrigger className="px-3 py-2">
                        <div data-slot="session-turn-diff-trigger">
                          <span data-slot="session-turn-diff-path">
                            {diff.file.includes("/") && (
                              <span data-slot="session-turn-diff-directory">
                                {getDirectory(diff.file)}/
                              </span>
                            )}
                            <span data-slot="session-turn-diff-filename">{getFilename(diff.file)}</span>
                          </span>
                          <div data-slot="session-turn-diff-meta">
                            <span data-slot="session-turn-diff-changes">
                              <DiffChanges changes={{ additions: diff.additions ?? 0, deletions: diff.deletions ?? 0 }} variant="default" />
                            </span>
                          </div>
                        </div>
                      </AccordionTrigger>
                      <AccordionContent>
                        <div data-slot="session-turn-diff-view" data-scrollable>
                          {diff.contents && (
                            <pre className="p-3 text-[12px] leading-relaxed text-foreground/80 whitespace-pre-wrap">
                              {diff.contents}
                            </pre>
                          )}
                        </div>
                      </AccordionContent>
                    </AccordionItem>
                  ))}
                </Accordion>
                {!showAll && overflow > 0 && (
                  <div data-slot="session-turn-diffs-more" onClick={() => setShowAll(true)}>
                    Show {overflow} more {overflow === 1 ? "file" : "files"}
                  </div>
                )}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
