import { useMemo } from "react"
import { AnimatedNumber } from "./animated-number"

function split(text: string) {
  const match = /{{\s*count\s*}}/.exec(text)
  if (!match) return { before: "", after: text }
  return {
    before: text.slice(0, match.index),
    after: text.slice(match.index + match[0].length),
  }
}

function common(one: string, other: string) {
  const a = Array.from(one)
  const b = Array.from(other)
  let i = 0
  while (i < a.length && i < b.length && b[i] === a[i]) i += 1
  return {
    stem: a.slice(0, i).join(""),
    one: a.slice(i).join(""),
    other: b.slice(i).join(""),
  }
}

export function AnimatedCountLabel({
  count,
  one,
  other,
  className,
}: {
  count: number
  one: string
  other: string
  className?: string
}) {
  const oneSplit = useMemo(() => split(one), [one])
  const otherSplit = useMemo(() => split(other), [other])
  const singular = Math.round(count) === 1
  const active = singular ? oneSplit : otherSplit
  const suffix = useMemo(() => common(oneSplit.after, otherSplit.after), [oneSplit.after, otherSplit.after])
  const splitSuffix =
    oneSplit.before === otherSplit.before &&
    (oneSplit.after.startsWith(otherSplit.after) || otherSplit.after.startsWith(oneSplit.after))
  const before = splitSuffix ? oneSplit.before : active.before
  const stem = splitSuffix ? suffix.stem : active.after
  const tail = splitSuffix ? (singular ? suffix.one : suffix.other) : ""
  const showTail = splitSuffix && tail.length > 0

  return (
    <span data-component="tool-count-label" className={className}>
      <span data-slot="tool-count-label-before">{before}</span>
      <AnimatedNumber value={count} />
      <span data-slot="tool-count-label-word">
        <span data-slot="tool-count-label-stem">{stem}</span>
        <span data-slot="tool-count-label-suffix" data-active={showTail ? "true" : "false"}>
          <span data-slot="tool-count-label-suffix-inner">{tail}</span>
        </span>
      </span>
    </span>
  )
}
