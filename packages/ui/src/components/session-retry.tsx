import { useEffect, useMemo, useState } from "react"
import type { SessionStatus } from "@opencode-ai/sdk/v2/client"
import { useI18n } from "../context/i18n"
import { Card } from "./card"
import { Tooltip } from "./tooltip"
import { Spinner } from "./spinner"

export function SessionRetry(props: { status: SessionStatus; show?: boolean }) {
  const i18n = useI18n()
  const retry = useMemo(() => {
    if (props.status.type !== "retry") return undefined
    return props.status
  }, [props.status])
  const [seconds, setSeconds] = useState(0)

  useEffect(() => {
    if (!retry) return
    const update = () => {
      if (!retry.next) return
      setSeconds(Math.round((retry.next - Date.now()) / 1000))
    }
    update()
    const timer = setInterval(update, 1000)
    return () => clearInterval(timer)
  }, [retry])

  const message = useMemo(() => {
    if (!retry) return ""
    if (retry.message.includes("exceeded your current quota") && retry.message.includes("gemini")) {
      return i18n.t("ui.sessionTurn.retry.geminiHot")
    }
    if (retry.message.length > 80) return retry.message.slice(0, 80) + "..."
    return retry.message
  }, [retry, i18n])

  const truncated = useMemo(() => {
    if (!retry) return false
    return retry.message.length > 80
  }, [retry])

  const info = useMemo(() => {
    if (!retry) return ""
    const count = Math.max(0, seconds)
    const delay = count > 0 ? i18n.t("ui.sessionTurn.retry.inSeconds", { seconds: count }) : ""
    const retrying = i18n.t("ui.sessionTurn.retry.retrying")
    const line = [retrying, delay].filter(Boolean).join(" ")
    if (!line) return i18n.t("ui.sessionTurn.retry.attempt", { attempt: retry.attempt })
    return i18n.t("ui.sessionTurn.retry.attemptLine", { line, attempt: retry.attempt })
  }, [retry, seconds, i18n])

  return (
    retry && (props.show ?? true) ? (
      <div data-slot="session-turn-retry">
        <Card variant="error" className="error-card">
          <div className="flex items-start gap-2">
            <Spinner className="size-4 mt-0.5" />
            <div className="min-w-0">
              {truncated ? (
                <Tooltip value={retry.message} placement="top">
                  <div data-slot="session-turn-retry-message" className="cursor-help truncate">
                    {message}
                  </div>
                </Tooltip>
              ) : (
                <div data-slot="session-turn-retry-message">{message}</div>
              )}
              {info && <div data-slot="session-turn-retry-info">{info}</div>}
            </div>
          </div>
        </Card>
      </div>
    ) : null
  )
}
