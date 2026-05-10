import { UserMessage } from "@opencode-ai/sdk/v2"
import { ComponentProps } from "react"
import { DiffChanges } from "./diff-changes"
import { Tooltip } from "./tooltip"
import { useI18n } from "../context/i18n"

export function MessageNav(
  props: ComponentProps<"ul"> & {
    messages: UserMessage[]
    current?: UserMessage
    size: "normal" | "compact"
    onMessageSelect: (message: UserMessage) => void
    getLabel?: (message: UserMessage) => string | undefined
  },
) {
  const i18n = useI18n()
  const { messages, current, size, onMessageSelect, getLabel, ...others } = props

  const content = (
    <ul role="list" data-component="message-nav" data-size={size} {...others}>
      {messages.map((message) => {
        const handleClick = () => onMessageSelect(message)

        const handleKeyPress = (event: React.KeyboardEvent) => {
          if (event.key !== "Enter" && event.key !== " ") return
          event.preventDefault()
          onMessageSelect(message)
        }

        return (
          <li key={message.id} data-slot="message-nav-item">
            {size === "compact" ? (
              <div
                data-slot="message-nav-tick-button"
                data-active={message.id === current?.id || undefined}
                role="button"
                tabIndex={0}
                onClick={handleClick}
                onKeyDown={handleKeyPress}
              >
                <div data-slot="message-nav-tick-line" />
              </div>
            ) : size === "normal" ? (
              <button data-slot="message-nav-message-button" onClick={handleClick} onKeyDown={handleKeyPress}>
                <DiffChanges changes={message.summary?.diffs ?? []} variant="bars" />
                <div
                  data-slot="message-nav-title-preview"
                  data-active={message.id === current?.id || undefined}
                >
                  {getLabel?.(message) ?? message.summary?.title ?? i18n.t("ui.messageNav.newMessage")}
                </div>
              </button>
            ) : null}
          </li>
        )
      })}
    </ul>
  )

  return (
    size === "compact" ? (
      <Tooltip
        openDelay={0}
        placement="right-start"
        gutter={-40}
        shift={-10}
        overlap
        contentClass="message-nav-tooltip"
        value={
          <div data-slot="message-nav-tooltip-content">
            <MessageNav {...props} size="normal" className="" />
          </div>
        }
      >
        {content}
      </Tooltip>
    ) : size === "normal" ? content : null
  )
}
