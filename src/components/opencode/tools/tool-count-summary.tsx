import { Fragment, useMemo } from "react"
import { AnimatedCountLabel } from "./tool-count-label"

interface CountItem {
  key: string
  count: number
  one: string
  other: string
}

export function AnimatedCountList({
  items,
  fallback = "",
  className,
}: {
  items: CountItem[]
  fallback?: string
  className?: string
}) {
  const visible = useMemo(() => items.filter((item) => item.count > 0), [items])
  const showEmpty = visible.length === 0 && fallback.length > 0

  return (
    <span data-component="tool-count-summary" className={className}>
      <span data-slot="tool-count-summary-empty" data-active={showEmpty ? "true" : "false"}>
        <span data-slot="tool-count-summary-empty-inner">{fallback}</span>
      </span>

      {items.map((item, index) => {
        const active = item.count > 0
        const hasPrev = items.slice(0, index).some((previous) => previous.count > 0)

        return (
          <Fragment key={item.key}>
            <span data-slot="tool-count-summary-prefix" data-active={active && hasPrev ? "true" : "false"}>
              ,
            </span>
            <span data-slot="tool-count-summary-item" data-active={active ? "true" : "false"}>
              <span data-slot="tool-count-summary-item-inner">
                <AnimatedCountLabel
                  count={Math.max(0, Math.round(item.count))}
                  one={item.one}
                  other={item.other}
                />
              </span>
            </span>
          </Fragment>
        )
      })}
    </span>
  )
}
