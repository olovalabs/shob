import { useMemo } from "react"
import { AnimatedCountLabel } from "./tool-count-label"

export type CountItem = {
  key: string
  count: number
  one: string
  other: string
}

export function AnimatedCountList(props: { items: CountItem[]; fallback?: string; class?: string }) {
  const visible = useMemo(() => props.items.filter((item) => item.count > 0), [props.items])
  const showEmpty = visible.length === 0 && !!props.fallback

  return (
    <span data-component="tool-count-summary" className={props.class}>
      <span data-slot="tool-count-summary-empty" data-active={showEmpty ? "true" : "false"}>
        <span data-slot="tool-count-summary-empty-inner">{props.fallback ?? ""}</span>
      </span>

      {props.items.map((item, index) => {
        const active = item.count > 0
        const hasPrev = (() => {
          for (let i = index - 1; i >= 0; i--) {
            if (props.items[i].count > 0) return true
          }
          return false
        })()

        return (
          <span key={item.key}>
            <span data-slot="tool-count-summary-prefix" data-active={active && hasPrev ? "true" : "false"}>
              ,
            </span>
            <span data-slot="tool-count-summary-item" data-active={active ? "true" : "false"}>
              <span data-slot="tool-count-summary-item-inner">
                <AnimatedCountLabel
                  one={item.one}
                  other={item.other}
                  count={Math.max(0, Math.round(item.count))}
                />
              </span>
            </span>
          </span>
        )
      })}
    </span>
  )
}
