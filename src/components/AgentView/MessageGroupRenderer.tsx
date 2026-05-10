import { useMemo } from "react"
import { SessionTurn } from "@/components/opencode/tools"
import type { AgentMessage } from "@/types"
import type { LiveAssistantState, AgentMsg } from "./types"
import {
  convertToSessionFormat,
  toTurnMessage,
  buildAssistantParts,
  describeOpenCodeError,
} from "./utils"

interface MessageGroupRendererProps {
  messages: AgentMessage[]
  isThinking: boolean
  liveAssistant: LiveAssistantState | null
}

export function MessageGroupRenderer({
  messages: msgs,
  isThinking,
  liveAssistant,
}: MessageGroupRendererProps) {
  const groups = useMemo(() => convertToSessionFormat(msgs as AgentMsg[]), [msgs])

  return (
    <>
      {groups.map((group, index) => {
        const isLast = index === groups.length - 1
        const assistantMessages = group.assistantMessages.map(toTurnMessage)

        if (isLast && isThinking && liveAssistant && assistantMessages.length === 0) {
          const liveID = "live-assistant"
          assistantMessages.push(toTurnMessage({
            id: liveID,
            role: "assistant",
            content: liveAssistant.content,
            createdAt: liveAssistant.createdAt,
            toolCalls: liveAssistant.toolCalls,
            parts: liveAssistant.parts.length > 0
              ? liveAssistant.parts
              : buildAssistantParts(liveID, liveAssistant.content, liveAssistant.toolCalls),
            error: liveAssistant.error
              ? { name: "OpenCodeError", data: { message: describeOpenCodeError(liveAssistant.error) } }
              : null,
          }))
        }

        return (
          <SessionTurn
            key={group.userMessage.id}
            messages={[toTurnMessage(group.userMessage), ...assistantMessages]}
            userMessageIndex={0}
            assistantMessages={assistantMessages}
            working={isThinking && isLast}
            error={isThinking && isLast && liveAssistant?.error ? describeOpenCodeError(liveAssistant.error) : null}
          />
        )
      })}
    </>
  )
}
