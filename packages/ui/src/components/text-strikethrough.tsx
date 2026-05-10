import type { JSX } from "react"
import { useEffect, useMemo, useRef, useState } from "react"
import { useSpring } from "./motion-spring"

export function TextStrikethrough(props: {
  active: boolean
  text: string
  visualDuration?: number
  class?: string
  style?: JSX.CSSProperties
}) {
  const progress = useSpring(
    props.active ? 1 : 0,
    { visualDuration: props.visualDuration ?? 0.35, bounce: 0 },
  )

  const baseRef = useRef<HTMLSpanElement>(null)
  const containerRef = useRef<HTMLSpanElement>(null)
  const [state, setState] = useState({
    textWidth: 0,
    containerWidth: 0,
  })

  const measure = () => {
    if (baseRef.current) setState(prev => ({ ...prev, textWidth: baseRef.current!.scrollWidth }))
    if (containerRef.current) setState(prev => ({ ...prev, containerWidth: containerRef.current!.offsetWidth }))
  }

  useEffect(() => {
    measure()
  }, [])

  useEffect(() => {
    const el = containerRef.current
    if (!el) return
    const observer = new ResizeObserver(measure)
    observer.observe(el)
    return () => observer.disconnect()
  }, [])

  const revealedPx = useMemo(() => {
    const tw = state.textWidth
    return tw > 0 ? progress * tw : 0
  }, [state.textWidth, progress])

  const overlayClip = useMemo(() => {
    const cw = state.containerWidth
    const tw = state.textWidth
    if (cw <= 0 || tw <= 0) return `inset(0 ${(1 - progress) * 100}% 0 0)`
    const remaining = Math.max(0, cw - revealedPx)
    return `inset(0 ${remaining}px 0 0)`
  }, [state.containerWidth, state.textWidth, progress, revealedPx])

  const baseClip = useMemo(() => {
    if (revealedPx <= 0.5) return "none"
    return `inset(0 0 0 ${revealedPx}px)`
  }, [revealedPx])

  return (
    <span
      data-component="text-strikethrough"
      className={props.class}
      style={{ display: "grid", ...props.style }}
      ref={containerRef}
    >
      <span ref={baseRef} style={{ gridArea: "1 / 1", clipPath: baseClip }}>
        {props.text}
      </span>
      <span
        aria-hidden="true"
        style={{
          gridArea: "1 / 1",
          textDecoration: "line-through",
          pointerEvents: "none",
          clipPath: overlayClip,
        }}
      >
        {props.text}
      </span>
    </span>
  )
}
