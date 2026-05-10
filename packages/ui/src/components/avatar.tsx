import { type ComponentProps } from "react"

const segmenter =
  typeof Intl !== "undefined" && "Segmenter" in Intl
    ? new Intl.Segmenter(undefined, { granularity: "grapheme" })
    : undefined

function first(value: string) {
  if (!value) return ""
  if (!segmenter) return Array.from(value)[0] ?? ""
  return segmenter.segment(value)[Symbol.iterator]().next().value?.segment ?? Array.from(value)[0] ?? ""
}

function buildClassName(base: string | undefined, classList: Record<string, boolean | undefined> | undefined): string | undefined {
  const classes = [...(base ? [base] : [])]
  if (classList) {
    for (const [key, val] of Object.entries(classList)) {
      if (val) classes.push(key)
    }
  }
  return classes.filter(Boolean).join(" ") || undefined
}

export interface AvatarProps extends ComponentProps<"div"> {
  fallback: string
  src?: string
  background?: string
  foreground?: string
  size?: "small" | "normal" | "large"
}

export function Avatar({ fallback, src, background, foreground, size, class: _class, classList, style, ...rest }: AvatarProps) {
  return (
    <div
      {...rest}
      data-component="avatar"
      data-size={size || "normal"}
      data-has-image={src ? "" : undefined}
      className={buildClassName(_class, classList)}
      style={{
        ...(typeof style === "object" ? style : {}),
        ...(!src && background ? ({ "--avatar-bg": background } as React.CSSProperties) : {}),
        ...(!src && foreground ? ({ "--avatar-fg": foreground } as React.CSSProperties) : {}),
      }}
    >
      {src ? <img src={src} draggable={false} data-slot="avatar-image" /> : first(fallback)}
    </div>
  )
}
