import React, { useEffect, useRef, useState } from "react"

export function TextShimmer({
  text,
  active = true,
  offset = 0,
  className,
}: {
  text: string
  active?: boolean
  offset?: number
  className?: string
}) {
  const swap = 220
  const [run, setRun] = useState(active)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  if (active && !run) {
    setRun(true)
  }

  useEffect(() => {
    if (!active && run) {
      timerRef.current = setTimeout(() => setRun(false), swap)
      return () => {
        if (timerRef.current) clearTimeout(timerRef.current)
        timerRef.current = null
      }
    }
  }, [active, run])

  return (
    <span
      data-component="text-shimmer"
      data-active={active ? "true" : "false"}
      className={className}
      aria-label={text}
      style={{
        "--text-shimmer-swap": `${swap}ms`,
        "--text-shimmer-index": `${offset}`,
      } as React.CSSProperties}
    >
      <span data-slot="text-shimmer-char">
        <span data-slot="text-shimmer-char-base" aria-hidden="true">
          {text}
        </span>
        <span data-slot="text-shimmer-char-shimmer" data-run={run ? "true" : "false"} aria-hidden="true">
          {text}
        </span>
      </span>
    </span>
  )
}
