import { useMemo } from "react"
import { AnimatedNumber } from "./animated-number"

function split(text: string) {
  const match = /{{\s*count\s*}}/.exec(text)
  if (!match) return { before: "", after: text }
  if (match.index === undefined) return { before: "", after: text }
  return {
    before: text.slice(0, match.index),
    after: text.slice(match.index + match[0].length),
  }
}

function common(one: string, other: string) {
  const a = Array.from(one)
  const b = Array.from(other)
  let i = 0
  while (i < a.length && i < b.length && a[i] === b[i]) i++
  return {
    stem: a.slice(0, i).join(""),
    one: a.slice(i).join(""),
    other: b.slice(i).join(""),
  }
}

export function AnimatedCountLabel(props: { count: number; one: string; other: string; class?: string }) {
  const one = useMemo(() => split(props.one))
  const other = useMemo(() => split(props.other))
  const singular = useMemo(() => Math.round(props.count) === 1)
  const active = useMemo(() => (singular() ? one() : other()))
  const suffix = useMemo(() => common(one().after, other().after))
  const splitSuffix = useMemo(
    () =>
      one().before === other().before &&
      (one().after.startsWith(other().after) || other().after.startsWith(one().after)),
  )
  const before = useMemo(() => (splitSuffix() ? one().before : active().before))
  const stem = useMemo(() => (splitSuffix() ? suffix().stem : active().after))
  const tail = useMemo(() => {
    if (!splitSuffix()) return ""
    if (singular()) return suffix().one
    return suffix().other
  })
  const showTail = useMemo(() => splitSuffix() && tail().length > 0)

  return (
    <span data-component="tool-count-label" className={props.class}>
      <span data-slot="tool-count-label-before">{before()}</span>
      <AnimatedNumber value={props.count} />
      <span data-slot="tool-count-label-word">
        <span data-slot="tool-count-label-stem">{stem()}</span>
        <span data-slot="tool-count-label-suffix" data-active={showTail() ? "true" : "false"}>
          <span data-slot="tool-count-label-suffix-inner">{tail()}</span>
        </span>
      </span>
    </span>
  )
}
