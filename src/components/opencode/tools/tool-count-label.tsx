import { useMemo } from "react"

export function AnimatedCountLabel({
  count,
  one,
  other,
}: {
  count: number
  one: string
  other: string
}) {
  const label = useMemo(() => {
    const text = count === 1 ? one : other
    const withCount = text.replace("{{count}}", String(count))
    const stemPos = withCount.indexOf(String(count))
    return {
      before: stemPos > 0 ? withCount.slice(0, stemPos) : "",
      after: stemPos >= 0 ? withCount.slice(stemPos + String(count).length) : withCount,
    }
  }, [count, one, other])

  return (
    <span data-component="count-label">
      <span data-slot="count-label-before">{label.before}</span>
      <span data-slot="count-label-number">{count}</span>
      <span data-slot="count-label-after">{label.after}</span>
    </span>
  )
}
