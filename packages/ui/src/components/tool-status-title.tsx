import { useEffect, useMemo, useRef, useState } from "react"
import { TextShimmer } from "./text-shimmer"

function common(active: string, done: string) {
  const a = Array.from(active)
  const b = Array.from(done)
  let i = 0
  while (i < a.length && i < b.length && a[i] === b[i]) i++
  return {
    prefix: a.slice(0, i).join(""),
    active: a.slice(i).join(""),
    done: b.slice(i).join(""),
  }
}

function contentWidth(el: HTMLSpanElement | null) {
  if (!el) return
  return `${Math.ceil(el.getBoundingClientRect().width)}px`
}

export function ToolStatusTitle(props: {
  active: boolean
  activeText: string
  doneText: string
  class?: string
  split?: boolean
}) {
  const split = useMemo(() => common(props.activeText, props.doneText), [props.activeText, props.doneText])
  const suffix = useMemo(
    () => (props.split ?? true) && split.prefix.length >= 2 && split.active.length > 0 && split.done.length > 0,
    [props.split, split],
  )
  const prefixLen = useMemo(() => Array.from(split.prefix).length, [split])
  const activeTail = useMemo(() => (suffix ? split.active : props.activeText), [suffix, split, props.activeText])
  const doneTail = useMemo(() => (suffix ? split.done : props.doneText), [suffix, split, props.doneText])

  const [state, setState] = useState({
    active: props.active,
    animating: false,
    width: undefined as string | undefined,
  })

  const activeRef = useRef<HTMLSpanElement>(null)
  const doneRef = useRef<HTMLSpanElement>(null)
  const widthRef = useRef<HTMLSpanElement>(null)
  const frameRef = useRef<number | undefined>(undefined)
  const finishTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)

  const finish = () => {
    if (frameRef.current !== undefined) cancelAnimationFrame(frameRef.current)
    if (finishTimerRef.current !== undefined) clearTimeout(finishTimerRef.current)
    frameRef.current = undefined
    finishTimerRef.current = undefined
    setState(prev => ({ ...prev, animating: false, width: undefined }))
  }

  const animate = () => {
    const first = contentWidth(widthRef.current)
    finish()
    setState(prev => ({ ...prev, animating: true, active: props.active }))
    const last = contentWidth(props.active ? activeRef.current : doneRef.current)
    if (!first || !last) {
      finish()
      return
    }

    setState(prev => ({ ...prev, width: first }))
    if (first === last) {
      finishTimerRef.current = setTimeout(finish, 600)
      return
    }

    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = undefined
      setState(prev => ({ ...prev, width: last }))
      finishTimerRef.current = setTimeout(finish, 600)
    })
  }

  useEffect(() => {
    animate()
    return () => {
      if (frameRef.current !== undefined) cancelAnimationFrame(frameRef.current)
      if (finishTimerRef.current !== undefined) clearTimeout(finishTimerRef.current)
      frameRef.current = undefined
      finishTimerRef.current = undefined
    }
  }, [props.active, activeTail, doneTail])

  return (
    <span
      data-component="tool-status-title"
      data-active={state.active ? "true" : "false"}
      data-ready={state.animating ? "true" : "false"}
      data-mode={suffix ? "suffix" : "swap"}
      className={props.class}
      aria-label={state.active ? props.activeText : props.doneText}
    >
      {suffix ? (
        <span data-slot="tool-status-suffix">
          <span data-slot="tool-status-prefix">
            <TextShimmer text={split.prefix} active={state.active} offset={0} />
          </span>
          <span data-slot="tool-status-tail" ref={widthRef} style={{ width: state.width }}>
            {(state.animating || state.active) && (
              <span data-slot="tool-status-active" ref={activeRef}>
                <TextShimmer text={activeTail} active={state.active} offset={prefixLen} />
              </span>
            )}
            {(state.animating || !state.active) && (
              <span data-slot="tool-status-done" ref={doneRef}>
                <TextShimmer text={doneTail} active={false} offset={prefixLen} />
              </span>
            )}
          </span>
        </span>
      ) : (
        <span data-slot="tool-status-swap" ref={widthRef} style={{ width: state.width }}>
          {(state.animating || state.active) && (
            <span data-slot="tool-status-active" ref={activeRef}>
              <TextShimmer text={activeTail} active={state.active} offset={0} />
            </span>
          )}
          {(state.animating || !state.active) && (
            <span data-slot="tool-status-done" ref={doneRef}>
              <TextShimmer text={doneTail} active={false} offset={0} />
            </span>
          )}
        </span>
      )}
    </span>
  )
}
