import { useMemo, useRef } from "react"
import { SessionTurn } from "@/components/shob/tools"
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
  const livePartsRef = useRef<ReturnType<typeof buildAssistantParts> | null>(null)

  if (isThinking && liveAssistant) {
    const liveID = "live-assistant"
    livePartsRef.current = liveAssistant.parts.length > 0
      ? liveAssistant.parts
      : buildAssistantParts(liveID, liveAssistant.content, liveAssistant.toolCalls)
  } else {
    livePartsRef.current = null
  }

  return (
    <>
      {groups.map((group, index) => {
        const isLast = index === groups.length - 1
        const assistantMessages = group.assistantMessages.map(toTurnMessage)

        return (
          <div
            key={group.userMessage.id}
            data-message-id={group.userMessage.id}
            className="min-w-0 w-full max-w-full md:max-w-[800px] 2xl:max-w-[1000px]"
          >
            <SessionTurn
              messages={[toTurnMessage(group.userMessage), ...assistantMessages]}
              userMessageIndex={0}
              assistantMessages={assistantMessages}
              working={isThinking && isLast}
              liveParts={isLast && isThinking ? livePartsRef.current : null}
              liveError={isLast && isThinking && liveAssistant?.error ? describeOpenCodeError(liveAssistant.error) : null}
              classes={{
                root: "min-w-0 w-full relative",
                content: "flex flex-col justify-between !overflow-visible",
                container: "w-full px-4 md:px-5",
              }}
            />
          </div>
        )
      })}
    </>
  )
}
