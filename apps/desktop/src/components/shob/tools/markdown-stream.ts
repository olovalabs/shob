import remend from "remend"

export type Block = {
  raw: string
  src: string
  mode: "full" | "live"
}

function heal(text: string) {
  return remend(text, { linkMode: "text-only" })
}

export function stream(text: string) {
  const src = heal(text)
  return [{ raw: text, src, mode: "full" }] satisfies Block[]
}
