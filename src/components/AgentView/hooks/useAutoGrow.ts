import { useEffect, useRef } from "react"

interface UseAutoGrowParams {
  input: string
}

export const useAutoGrow = ({ input }: UseAutoGrowParams) => {
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const autoGrow = () => {
    const el = textareaRef.current
    if (!el) return
    el.style.height = "0px"
    const next = Math.min(el.scrollHeight, 240)
    el.style.height = `${next}px`
  }

  useEffect(() => {
    autoGrow()
  }, [input])

  return { textareaRef, autoGrow }
}
