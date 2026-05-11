import { useMemo, useState } from "react"
import { UserMessageDisplay } from "./message-user"
import { AssistantMessageDisplay } from "./message-assistant"
import { DiffChanges } from "./diff-changes"
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

interface LivePart {
  id: string
  type: string
  text?: string
  tool?: string
  callID?: string
  state?: {
    status?: string
    title?: string
    input?: unknown
    output?: string
    error?: string
    metadata?: Record<string, unknown>
    attachments?: unknown[]
    time?: {
      start?: number
      end?: number
      compacted?: number
    }
  }
}

interface SessionTurnProps {
  messages: Message[]
  userMessageIndex: number
  assistantMessages?: Message[]
  showReasoningSummaries?: boolean
  working?: boolean
  error?: string | null
  liveParts?: LivePart[] | null
  liveError?: string | null
  classes?: { root?: string; content?: string; container?: string }
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
  liveParts = null,
  liveError = null,
  classes,
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

  const allAssistantParts = useMemo(() => {
    const persisted = assistantMessages.flatMap((am) =>
      (am.parts ?? []).map((p) => ({
        ...p,
        messageID: am.id,
      }))
    )
    if (liveParts) {
      const liveMessageId = assistantMessages.length > 0
        ? assistantMessages[assistantMessages.length - 1].id
        : "live-assistant"
      return [...persisted, ...liveParts.map((p) => ({ ...p, messageID: liveMessageId }))]
    }
    return persisted
  }, [assistantMessages, liveParts])

  const effectiveError = liveError ?? error

  const edited = diffs.length
  const overflow = Math.max(0, edited - MAX_FILES)
  const visible = showAll ? diffs : diffs.slice(0, MAX_FILES)

  if (!userMessage || !userMessage.id) return null

  const userParts = userMessage.parts ?? []
  const compaction = userParts.find((p) => p.type === "compaction")
  const interrupted = assistantMessages.some((m) => m.error?.name === "MessageAbortedError")
  const divider = compaction ? "Earlier messages compacted" : interrupted ? "Interrupted" : ""

  return (
    <div data-component="session-turn" className={classes?.root}>
      <div data-slot="session-turn-content" className={classes?.content}>
        <div data-slot="session-turn-message-container" className={classes?.container}>
          <div data-slot="session-turn-message-content" aria-live="off">
            <UserMessageDisplay
              text={userMessage.content ?? ""}
              parts={userParts}
              agent={userMessage.agent}
              model={userMessage.model?.modelID}
              timestamp={formatTimestamp(userMessage.time?.created)}
            />
          </div>

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

          {(assistantMessages.length > 0 || liveParts) && (
            <div data-slot="session-turn-assistant-content">
              <AssistantMessageDisplay
                parts={allAssistantParts}
                messageId={assistantMessages[0]?.id ?? "live-assistant"}
                working={working}
                showReasoningSummaries={showReasoningSummaries}
              />
            </div>
          )}

          {effectiveError && (
            <div className="error-card">
              {effectiveError}
            </div>
          )}

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
                      <AccordionTrigger>
                        <div data-slot="session-turn-diff-trigger">
                          <span data-slot="session-turn-diff-path">
                            {diff.file.includes("/") && (
                              <span data-slot="session-turn-diff-directory">
                                {`\u202A${getDirectory(diff.file)}\u202C`}
                              </span>
                            )}
                            <span data-slot="session-turn-diff-filename">{getFilename(diff.file)}</span>
                          </span>
                          <div data-slot="session-turn-diff-meta">
                            <span data-slot="session-turn-diff-changes">
                              <DiffChanges changes={{ additions: diff.additions ?? 0, deletions: diff.deletions ?? 0 }} />
                            </span>
                            <span data-slot="session-turn-diff-chevron">
                              <svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
                                <path d="M4 6l4 4 4-4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                              </svg>
                            </span>
                          </div>
                        </div>
                      </AccordionTrigger>
                      <AccordionContent>
                        <div data-slot="session-turn-diff-view" data-scrollable>
                          {diff.contents && (
                            <pre className="text-12-regular text-text-base whitespace-pre-wrap p-3">
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
