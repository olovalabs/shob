import { useMemo } from "react"
import { AnimatedCountLabel } from "./tool-count-label"

interface CountItem {
  key: string
  count: number
  one: string
  other: string
}

export function AnimatedCountList({
  items,
  fallback,
}: {
  items: CountItem[]
  fallback: string
}) {
  const visible = useMemo(() => items.filter((item) => item.count > 0), [items])

  if (visible.length === 0) {
    return <>{fallback}</>
  }

  return (
    <span data-component="count-list">
      {visible.map((item, i) => (
        <span key={item.key} data-component="count-list-entry">
          {i > 0 && <span data-slot="count-list-comma">, </span>}
          <AnimatedCountLabel count={item.count} one={item.one} other={item.other} />
        </span>
      ))}
    </span>
  )
}
