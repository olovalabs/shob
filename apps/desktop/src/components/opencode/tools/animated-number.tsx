import { useEffect, useMemo, useRef, useState } from "react"

const TRACK = Array.from({ length: 30 }, (_, index) => index % 10)
const DURATION = 600

function normalize(value: number) {
  return ((value % 10) + 10) % 10
}

function spin(from: number, to: number, direction: 1 | -1) {
  if (from === to) return 0
  if (direction > 0) return (to - from + 10) % 10
  return -((from - to + 10) % 10)
}

function Digit({ value, direction }: { value: number; direction: 1 | -1 }) {
  const [state, setState] = useState({
    step: value + 10,
    animating: false,
  })
  const lastRef = useRef(value)

  useEffect(() => {
    const delta = spin(lastRef.current, value, direction)
    lastRef.current = value

    const frame = requestAnimationFrame(() => {
      setState((current) => {
        if (!delta) {
          return {
            animating: false,
            step: value + 10,
          }
        }

        return {
          animating: true,
          step: current.step + delta,
        }
      })
    })

    return () => cancelAnimationFrame(frame)
  }, [direction, value])

  return (
    <span data-slot="animated-number-digit">
      <span
        data-slot="animated-number-strip"
        data-animating={state.animating ? "true" : "false"}
        onTransitionEnd={() => {
          setState((current) => ({
            animating: false,
            step: normalize(current.step) + 10,
          }))
        }}
        style={{
          "--animated-number-offset": `${state.step}`,
          "--animated-number-duration": `var(--tool-motion-odometer-ms, ${DURATION}ms)`,
        } as React.CSSProperties}
      >
        {TRACK.map((item, index) => (
          <span key={`${index}-${item}`} data-slot="animated-number-cell">
            {item}
          </span>
        ))}
      </span>
    </span>
  )
}

export function AnimatedNumber({ value, className }: { value: number; className?: string }) {
  const target = useMemo(() => {
    if (!Number.isFinite(value)) return 0
    return Math.max(0, Math.round(value))
  }, [value])
  const [state, setState] = useState({
    value: target,
    direction: 1 as 1 | -1,
  })

  useEffect(() => {
    const frame = requestAnimationFrame(() => {
      setState((current) => {
        if (target === current.value) return current
        return {
          value: target,
          direction: target > current.value ? 1 : -1,
        }
      })
    })

    return () => cancelAnimationFrame(frame)
  }, [target])

  const label = state.value.toString()
  const digits = Array.from(label, (char) => {
    const code = char.charCodeAt(0) - 48
    if (code < 0 || code > 9) return 0
    return code
  }).reverse()
  const width = `${digits.length}ch`

  return (
    <span data-component="animated-number" className={className} aria-label={label}>
      <span data-slot="animated-number-value" style={{ "--animated-number-width": width } as React.CSSProperties}>
        {digits.map((digit, index) => (
          <Digit key={`${index}-${digits.length}`} value={digit} direction={state.direction} />
        ))}
      </span>
    </span>
  )
}
