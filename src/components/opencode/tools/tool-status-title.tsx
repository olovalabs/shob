import { useCallback, useEffect, useMemo, useRef, useState } from "react"
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
  if (!el) return undefined
  return `${Math.ceil(el.getBoundingClientRect().width)}px`
}

export function ToolStatusTitle({
  active,
  activeText,
  doneText,
  className,
  split: enableSplit = true,
}: {
  active: boolean
  activeText: string
  doneText: string
  className?: string
  split?: boolean
}) {
  const splitInfo = useMemo(() => common(activeText, doneText), [activeText, doneText])
  const useSuffix = useMemo(
    () => enableSplit && splitInfo.prefix.length >= 2 && splitInfo.active.length > 0 && splitInfo.done.length > 0,
    [enableSplit, splitInfo],
  )
  const prefixLen = useMemo(() => Array.from(splitInfo.prefix).length, [splitInfo.prefix])
  const activeTail = useMemo(() => (useSuffix ? splitInfo.active : activeText), [useSuffix, splitInfo.active, activeText])
  const doneTail = useMemo(() => (useSuffix ? splitInfo.done : doneText), [useSuffix, splitInfo.done, doneText])

  const [state, setState] = useState({
    active,
    animating: false,
    width: undefined as string | undefined,
  })
  const activeRef = useRef<HTMLSpanElement>(null)
  const doneRef = useRef<HTMLSpanElement>(null)
  const widthRef = useRef<HTMLSpanElement>(null)
  const frameRef = useRef<number | undefined>(undefined)
  const finishTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)

  const finish = useCallback(() => {
    if (frameRef.current !== undefined) cancelAnimationFrame(frameRef.current)
    if (finishTimerRef.current !== undefined) clearTimeout(finishTimerRef.current)
    frameRef.current = undefined
    finishTimerRef.current = undefined
    setState((s) => ({ ...s, animating: false, width: undefined }))
  }, [])

  useEffect(() => {
    const first = contentWidth(widthRef.current)
    finish()
    setState((s) => ({ ...s, animating: true, active }))
    const last = contentWidth(active ? activeRef.current : doneRef.current)
    if (!first || !last) {
      finish()
      return
    }
    setState((s) => ({ ...s, width: first }))
    if (first === last) {
      finishTimerRef.current = setTimeout(finish, 600)
      return
    }
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = undefined
      setState((s) => ({ ...s, width: last }))
      finishTimerRef.current = setTimeout(finish, 600)
    })
    return () => {
      if (frameRef.current) cancelAnimationFrame(frameRef.current)
      if (finishTimerRef.current) clearTimeout(finishTimerRef.current)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, activeTail, doneTail])

  return (
    <span
      data-component="tool-status-title"
      data-active={state.active ? "true" : "false"}
      data-ready={state.animating ? "true" : "false"}
      data-mode={useSuffix ? "suffix" : "swap"}
      className={className}
      aria-label={state.active ? activeText : doneText}
    >
      {useSuffix ? (
        <span data-slot="tool-status-suffix">
          <span data-slot="tool-status-prefix">
            <TextShimmer text={splitInfo.prefix} active={state.active} offset={0} />
          </span>
          <span data-slot="tool-status-tail" ref={widthRef} style={{ width: state.width }}>
            {(state.animating || state.active) && (
              <span data-slot="tool-status-active" ref={activeRef}>
                <TextShimmer text={activeTail} active={state.active} offset={prefixLen} />
              </span>
            )}
            {(state.animating || !state.active) && (
              <span data-slot="tool-status-done" ref={doneRef}>
                <TextShimmer text={doneTail} active={false} offset={0} />
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
