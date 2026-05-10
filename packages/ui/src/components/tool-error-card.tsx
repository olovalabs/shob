import { type ComponentProps, useMemo, useState } from "react"
import { Card, CardDescription } from "./card"
import { Collapsible } from "./collapsible"
import { Icon } from "./icon"
import { IconButton } from "./icon-button"
import { Tooltip } from "./tooltip"
import { useI18n } from "../context/i18n"

export interface ToolErrorCardProps extends Omit<ComponentProps<typeof Card>, "children" | "variant"> {
  tool: string
  error: string
  title?: string
  defaultOpen?: boolean
  subtitle?: string
  href?: string
}

export function ToolErrorCard(props: ToolErrorCardProps) {
  const i18n = useI18n()
  const [state, setState] = useState({
    open: props.defaultOpen ?? false,
    copied: false,
  })
  const { tool, error, title: splitTitle, defaultOpen: _defaultOpen, subtitle: splitSubtitle, href: splitHref, ...rest } = props

  const name = useMemo(() => {
    if (splitTitle) return splitTitle
    const map: Record<string, string> = {
      read: "ui.tool.read",
      list: "ui.tool.list",
      glob: "ui.tool.glob",
      grep: "ui.tool.grep",
      task: "ui.tool.task",
      webfetch: "ui.tool.webfetch",
      websearch: "ui.tool.websearch",
      bash: "ui.tool.shell",
      apply_patch: "ui.tool.patch",
      question: "ui.tool.questions",
    }
    const key = map[tool]
    if (!key) return tool
    if (!key.includes(".")) return key
    return i18n.t(key)
  }, [splitTitle, tool, i18n])

  const cleaned = useMemo(() => error.replace(/^Error:\s*/, "").trim(), [error])

  const tail = useMemo(() => {
    const prefix = `${tool} `
    if (cleaned.startsWith(prefix)) return cleaned.slice(prefix.length)
    return cleaned
  }, [cleaned, tool])

  const subtitle = useMemo(() => {
    if (splitSubtitle) return splitSubtitle
    const parts = tail.split(": ")
    if (parts.length <= 1) return i18n.t("ui.toolErrorCard.failed")
    const head = (parts[0] ?? "").trim()
    if (!head) return i18n.t("ui.toolErrorCard.failed")
    return head[0] ? head[0].toUpperCase() + head.slice(1) : i18n.t("ui.toolErrorCard.failed")
  }, [splitSubtitle, tail, i18n])

  const body = useMemo(() => {
    const parts = tail.split(": ")
    if (parts.length <= 1) return cleaned
    return parts.slice(1).join(": ").trim() || cleaned
  }, [tail, cleaned])

  const copy = async () => {
    if (!cleaned) return
    await navigator.clipboard.writeText(cleaned)
    setState(prev => ({ ...prev, copied: true }))
    setTimeout(() => setState(prev => ({ ...prev, copied: false })), 2000)
  }

  return (
    <Card {...rest} data-kind="tool-error-card" data-open={state.open ? "true" : "false"} variant="error">
      <Collapsible
        className="tool-collapsible"
        data-open={state.open ? "true" : "false"}
        open={state.open}
        onOpenChange={(value) => setState(prev => ({ ...prev, open: value }))}
      >
        <Collapsible.Trigger>
          <div data-component="tool-trigger">
            <div data-slot="basic-tool-tool-trigger-content">
              <span data-slot="basic-tool-tool-indicator" data-component="tool-error-card-icon">
                <Icon name="circle-ban-sign" size="small" style={{ strokeWidth: 1.5 }} />
              </span>
              <div data-slot="basic-tool-tool-info">
                <div data-slot="basic-tool-tool-info-structured">
                  <div data-slot="basic-tool-tool-info-main">
                    <span data-slot="basic-tool-tool-title">{name}</span>
                    {splitHref && splitSubtitle ? (
                      <a
                        data-slot="basic-tool-tool-subtitle"
                        className="clickable subagent-link"
                        href={splitHref}
                        onClick={(e) => e.stopPropagation()}
                      >
                        {subtitle}
                      </a>
                    ) : (
                      <span data-slot="basic-tool-tool-subtitle">{subtitle}</span>
                    )}
                  </div>
                </div>
              </div>
            </div>
            <Collapsible.Arrow />
          </div>
        </Collapsible.Trigger>
        <Collapsible.Content>
          <div data-slot="tool-error-card-content">
            {state.open && (
              <div data-slot="tool-error-card-copy">
                <Tooltip
                  value={state.copied ? i18n.t("ui.message.copied") : i18n.t("ui.toolErrorCard.copyError")}
                  placement="top"
                  gutter={4}
                >
                  <IconButton
                    icon={state.copied ? "check" : "copy"}
                    size="normal"
                    variant="ghost"
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={(e) => {
                      e.stopPropagation()
                      void copy()
                    }}
                    aria-label={state.copied ? i18n.t("ui.message.copied") : i18n.t("ui.toolErrorCard.copyError")}
                  />
                </Tooltip>
              </div>
            )}
            {body && <CardDescription>{body}</CardDescription>}
          </div>
        </Collapsible.Content>
      </Collapsible>
    </Card>
  )
}
