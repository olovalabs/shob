import { useEffect, useState } from "react"

export const TextShimmer = <T extends React.ElementType = "span">(props: {
  text: string
  class?: string
  as?: T
  active?: boolean
  offset?: number
}) => {
  const active = props.active ?? true
  const offset = props.offset ?? 0
  const [run, setRun] = useState(active)
  const swap = 220

  useEffect(() => {
    if (active) {
      setRun(true)
      return
    }

    const timer = setTimeout(() => {
      setRun(false)
    }, swap)

    return () => clearTimeout(timer)
  }, [active, swap])

  const Tag = props.as ?? "span"

  return (
    <Tag
      data-component="text-shimmer"
      data-active={active ? "true" : "false"}
      className={props.class}
      aria-label={props.text}
      style={
        {
          "--text-shimmer-swap": `${swap}ms`,
          "--text-shimmer-index": `${offset}`,
        } as React.CSSProperties
      }
    >
      <span data-slot="text-shimmer-char">
        <span data-slot="text-shimmer-char-base" aria-hidden="true">
          {props.text}
        </span>
        <span data-slot="text-shimmer-char-shimmer" data-run={run ? "true" : "false"} aria-hidden="true">
          {props.text}
        </span>
      </span>
    </Tag>
  )
}
