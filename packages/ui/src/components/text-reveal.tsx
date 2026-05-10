import { useEffect, useRef, useState } from "react"

const px = (value: number | string | undefined, fallback: number) => {
  if (typeof value === "number") return `${value}px`
  if (typeof value === "string") return value
  return `${fallback}px`
}

const ms = (value: number | string | undefined, fallback: number) => {
  if (typeof value === "number") return `${value}ms`
  if (typeof value === "string") return value
  return `${fallback}ms`
}

const pct = (value: number | undefined, fallback: number) => {
  const v = value ?? fallback
  return `${v}%`
}

export function TextReveal(props: {
  text?: string
  class?: string
  duration?: number | string
  edge?: number
  travel?: number | string
  spring?: string
  springSoft?: string
  growOnly?: boolean
  truncate?: boolean
}) {
  const [state, setState] = useState({
    cur: props.text,
    old: undefined as string | undefined,
    width: "auto",
    ready: false,
    swapping: false,
  })
  const inRef = useRef<HTMLSpanElement>(null)
  const outRef = useRef<HTMLSpanElement>(null)
  const rootRef = useRef<HTMLSpanElement>(null)
  const frameRef = useRef<number | undefined>(undefined)
  const prevTextRef = useRef(props.text)

  const widen = (next: number) => {
    if (next <= 0) return
    if (props.growOnly ?? true) {
      const prev = Number.parseFloat(state.width)
      if (Number.isFinite(prev) && next <= prev) return
    }
    setState(prev => ({ ...prev, width: `${next}px` }))
  }

  useEffect(() => {
    const next = props.text
    const prev = prevTextRef.current
    prevTextRef.current = next
    if (next === prev) return
    if (typeof next === "string" && typeof prev === "string" && next.startsWith(prev)) {
      setState(prevState => ({ ...prevState, cur: next }))
      widen(inRef.current?.scrollWidth ?? 0)
      return
    }
    setState(prevState => ({ ...prevState, swapping: true, old: prev, cur: next }))
    if (typeof requestAnimationFrame !== "function") {
      widen(Math.max(inRef.current?.scrollWidth ?? 0, outRef.current?.scrollWidth ?? 0))
      rootRef.current?.offsetHeight
      setState(prevState => ({ ...prevState, swapping: false }))
      return
    }
    if (frameRef.current !== undefined && typeof cancelAnimationFrame === "function") cancelAnimationFrame(frameRef.current)
    frameRef.current = requestAnimationFrame(() => {
      widen(Math.max(inRef.current?.scrollWidth ?? 0, outRef.current?.scrollWidth ?? 0))
      rootRef.current?.offsetHeight
      setState(prevState => ({ ...prevState, swapping: false }))
      frameRef.current = undefined
    })
  }, [props.text])

  useEffect(() => {
    widen(inRef.current?.scrollWidth ?? 0)
    const fonts = typeof document !== "undefined" ? document.fonts : undefined
    if (typeof requestAnimationFrame !== "function") {
      setState(prev => ({ ...prev, ready: true }))
      return
    }
    if (!fonts) {
      requestAnimationFrame(() => setState(prev => ({ ...prev, ready: true })))
      return
    }
    void fonts.ready.finally(() => {
      widen(inRef.current?.scrollWidth ?? 0)
      requestAnimationFrame(() => setState(prev => ({ ...prev, ready: true })))
    })
  }, [])

  useEffect(() => {
    return () => {
      if (frameRef.current !== undefined && typeof cancelAnimationFrame === "function") {
        cancelAnimationFrame(frameRef.current)
      }
    }
  }, [])

  return (
    <span
      ref={rootRef}
      data-component="text-reveal"
      data-ready={state.ready ? "true" : "false"}
      data-swapping={state.swapping ? "true" : "false"}
      data-truncate={props.truncate ? "true" : "false"}
      className={props.class}
      aria-label={props.text ?? ""}
      style={{
        "--text-reveal-duration": ms(props.duration, 450),
        "--text-reveal-edge": pct(props.edge, 17),
        "--text-reveal-travel": px(props.travel, 0),
        "--text-reveal-spring": props.spring ?? "cubic-bezier(0.34, 1.08, 0.64, 1)",
        "--text-reveal-spring-soft": props.springSoft ?? "cubic-bezier(0.34, 1, 0.64, 1)",
      } as React.CSSProperties}
    >
      <span data-slot="text-reveal-track" style={{ width: props.truncate ? "100%" : state.width }}>
        <span data-slot="text-reveal-entering" ref={inRef}>
          {state.cur ?? "\u00A0"}
        </span>
        <span data-slot="text-reveal-leaving" ref={outRef}>
          {state.old ?? "\u00A0"}
        </span>
      </span>
    </span>
  )
}
