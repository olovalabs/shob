import { useState, useEffect, useMemo, useRef } from "react"

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
  const [step, setStep] = useState(value + 10)
  const [animating, setAnimating] = useState(false)
  const last = useRef(value)

  useEffect(() => {
    const delta = spin(last.current, value, direction)
    last.current = value
    if (!delta) {
      setAnimating(false)
      setStep(value + 10)
      return
    }
    setAnimating(true)
    setStep((s) => s + delta)
  }, [value, direction])

  return (
    <span data-slot="animated-number-digit">
      <span
        data-slot="animated-number-strip"
        data-animating={animating ? "true" : "false"}
        onTransitionEnd={() => {
          setAnimating(false)
          setStep((s) => normalize(s) + 10)
        }}
        style={{
          "--animated-number-offset": `${step}`,
          "--animated-number-duration": `var(--tool-motion-odometer-ms, ${DURATION}ms)`,
        } as React.CSSProperties}
      >
        {TRACK.map((v) => <span key={v} data-slot="animated-number-cell">{v}</span>)}
      </span>
    </span>
  )
}

export function AnimatedNumber({ value, class: className }: { value: number; class?: string }) {
  const target = useMemo(() => {
    if (!Number.isFinite(value)) return 0
    return Math.max(0, Math.round(value))
  }, [value])

  const [direction, setDirection] = useState<1 | -1>(1)
  const prevRef = useRef(target)

  useEffect(() => {
    const prev = prevRef.current
    if (target === prev) return
    setDirection(target > prev ? 1 : -1)
    prevRef.current = target
  }, [target])

  const label = target.toString()
  const digits = useMemo(
    () =>
      Array.from(label, (char) => {
        const code = char.charCodeAt(0) - 48
        if (code < 0 || code > 9) return 0
        return code
      }).reverse(),
    [label],
  )

  const width = `${digits.length}ch`

  return (
    <span data-component="animated-number" className={className} aria-label={label}>
      <span data-slot="animated-number-value" style={{ "--animated-number-width": width } as React.CSSProperties}>
        {digits.map((digit, i) => <Digit key={i} value={digit} direction={direction} />)}
      </span>
    </span>
  )
}
