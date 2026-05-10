import { ComponentProps } from "react"

const outerIndices = new Set([1, 2, 4, 7, 8, 11, 13, 14])
const cornerIndices = new Set([0, 3, 12, 15])
const squares = Array.from({ length: 16 }, (_, i) => ({
  id: i,
  x: (i % 4) * 4,
  y: Math.floor(i / 4) * 4,
  delay: Math.random() * 1.5,
  duration: 1 + Math.random() * 1,
  outer: outerIndices.has(i),
  corner: cornerIndices.has(i),
}))

function buildClassName(base: string | undefined, classList: Record<string, boolean | undefined> | undefined): string | undefined {
  const classes = [...(base ? [base] : [])]
  if (classList) {
    for (const [key, val] of Object.entries(classList)) {
      if (val) classes.push(key)
    }
  }
  return classes.filter(Boolean).join(" ") || undefined
}

export function Spinner(props: {
  class?: string
  classList?: ComponentProps<"div">["classList"]
  style?: ComponentProps<"div">["style"]
}) {
  return (
    <svg
      viewBox="0 0 15 15"
      data-component="spinner"
      className={buildClassName(props.class, props.classList)}
      fill="currentColor"
      style={props.style}
    >
      {squares.map((square) => (
        <rect
          key={square.id}
          x={square.x}
          y={square.y}
          width="3"
          height="3"
          rx="1"
          style={{
            opacity: square.corner ? 0 : undefined,
            animation: square.corner
              ? undefined
              : `${square.outer ? "pulse-opacity-dim" : "pulse-opacity"} ${square.duration}s ease-in-out infinite`,
            animationFillMode: square.corner ? undefined : "both",
            animationDelay: square.corner ? undefined : `${square.delay}s`,
          }}
        />
      ))}
    </svg>
  )
}
