import { useEffect, useState } from "react"

export const Typewriter = <T extends React.ElementType = "p">(props: { text?: string; class?: string; as?: T }) => {
  const [typing, setTyping] = useState(false)
  const [displayed, setDisplayed] = useState("")
  const [cursor, setCursor] = useState(true)

  useEffect(() => {
    const text = props.text
    if (!text) return

    let i = 0
    const timeouts: ReturnType<typeof setTimeout>[] = []
    setTyping(true)
    setDisplayed("")
    setCursor(true)

    const getTypingDelay = () => {
      const random = Math.random()
      if (random < 0.05) return 150 + Math.random() * 100
      if (random < 0.15) return 80 + Math.random() * 60
      return 30 + Math.random() * 50
    }

    const type = () => {
      if (i < text.length) {
        setDisplayed(text.slice(0, i + 1))
        i++
        timeouts.push(setTimeout(type, getTypingDelay()))
      } else {
        setTyping(false)
        timeouts.push(setTimeout(() => setCursor(false), 2000))
      }
    }

    timeouts.push(setTimeout(type, 200))

    return () => {
      for (const timeout of timeouts) clearTimeout(timeout)
    }
  }, [props.text])

  const Tag = props.as || "p"

  return (
    <Tag className={props.class}>
      {displayed}
      {cursor && (
        <span className={!typing ? "blinking-cursor" : undefined}>│</span>
      )}
    </Tag>
  )
}
