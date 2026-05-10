import { useEffect, useRef, useState } from "react"

export interface AutoScrollOptions {
  working: () => boolean
  onUserInteracted?: () => void
  overflowAnchor?: "none" | "auto" | "dynamic"
  bottomThreshold?: number
}

export function createAutoScroll(options: AutoScrollOptions) {
  let settling = false
  let settleTimer: ReturnType<typeof setTimeout> | undefined
  let autoTimer: ReturnType<typeof setTimeout> | undefined
  let auto: { top: number; time: number } | undefined

  const storeRef = useRef({
    contentRef: undefined as HTMLElement | undefined,
    scrollRef: undefined as HTMLElement | undefined,
    userScrolled: false,
  })
  const [, forceUpdate] = useState(0)

  const setStore = (partial: Partial<typeof storeRef.current>) => {
    Object.assign(storeRef.current, partial)
    forceUpdate((n) => n + 1)
  }

  const threshold = options.bottomThreshold ?? 10

  const active = () => options.working() || settling

  const distanceFromBottom = (el: HTMLElement) => {
    return el.scrollHeight - el.clientHeight - el.scrollTop
  }

  const canScroll = (el: HTMLElement) => {
    return el.scrollHeight - el.clientHeight > 1
  }

  const markAuto = (el: HTMLElement) => {
    auto = {
      top: Math.max(0, el.scrollHeight - el.clientHeight),
      time: Date.now(),
    }
    if (autoTimer) clearTimeout(autoTimer)
    autoTimer = setTimeout(() => {
      auto = undefined
      autoTimer = undefined
    }, 1500)
  }

  const isAuto = (el: HTMLElement) => {
    const a = auto
    if (!a) return false
    if (Date.now() - a.time > 1500) {
      auto = undefined
      return false
    }
    return Math.abs(el.scrollTop - a.top) < 2
  }

  const scrollToBottomNow = (behavior: ScrollBehavior) => {
    const el = storeRef.current.scrollRef
    if (!el) return
    markAuto(el)
    if (behavior === "smooth") {
      el.scrollTo({ top: el.scrollHeight, behavior })
      return
    }
    el.scrollTop = el.scrollHeight
  }

  const scrollToBottom = (force: boolean) => {
    if (!force && !active()) return

    if (force && storeRef.current.userScrolled) setStore({ userScrolled: false })

    const el = storeRef.current.scrollRef
    if (!el) return

    if (!force && storeRef.current.userScrolled) return

    const distance = distanceFromBottom(el)
    if (distance < 2) {
      markAuto(el)
      return
    }

    scrollToBottomNow("auto")
  }

  const stop = () => {
    const el = storeRef.current.scrollRef
    if (!el) return
    if (!canScroll(el)) {
      if (storeRef.current.userScrolled) setStore({ userScrolled: false })
      return
    }
    if (storeRef.current.userScrolled) return
    setStore({ userScrolled: true })
    options.onUserInteracted?.()
  }

  const handleWheel = (e: WheelEvent) => {
    if (e.deltaY >= 0) return
    const el = storeRef.current.scrollRef
    const target = e.target instanceof Element ? e.target : undefined
    const nested = target?.closest("[data-scrollable]")
    if (el && nested && nested !== el) return
    stop()
  }

  const handleScroll = () => {
    const el = storeRef.current.scrollRef
    if (!el) return

    if (!canScroll(el)) {
      if (storeRef.current.userScrolled) setStore({ userScrolled: false })
      return
    }

    if (distanceFromBottom(el) < threshold) {
      if (storeRef.current.userScrolled) setStore({ userScrolled: false })
      return
    }

    if (!storeRef.current.userScrolled && isAuto(el)) {
      scrollToBottom(false)
      return
    }

    stop()
  }

  const handleInteraction = () => {
    if (!active()) return
    const selection = window.getSelection()
    if (selection && selection.toString().length > 0) {
      stop()
    }
  }

  const updateOverflowAnchor = (el: HTMLElement) => {
    const mode = options.overflowAnchor ?? "dynamic"

    if (mode === "none") {
      el.style.overflowAnchor = "none"
      return
    }

    if (mode === "auto") {
      el.style.overflowAnchor = "auto"
      return
    }

    el.style.overflowAnchor = storeRef.current.userScrolled ? "auto" : "none"
  }

  useEffect(() => {
    const el = storeRef.current.contentRef
    if (!el) return

    const observer = new ResizeObserver(() => {
      const scrollEl = storeRef.current.scrollRef
      if (scrollEl && !canScroll(scrollEl)) {
        if (storeRef.current.userScrolled) setStore({ userScrolled: false })
        return
      }
      if (!active()) return
      if (storeRef.current.userScrolled) return
      scrollToBottom(false)
    })

    observer.observe(el)
    return () => observer.disconnect()
  }, [storeRef.current.contentRef])

  useEffect(() => {
    settling = false
    if (settleTimer) clearTimeout(settleTimer)
    settleTimer = undefined

    if (options.working()) {
      if (!storeRef.current.userScrolled) scrollToBottom(true)
      return
    }

    settling = true
    settleTimer = setTimeout(() => {
      settling = false
    }, 300)

    return () => {
      if (settleTimer) clearTimeout(settleTimer)
    }
  }, [options.working()])

  useEffect(() => {
    const el = storeRef.current.scrollRef
    if (!el) return
    updateOverflowAnchor(el)
  })

  useEffect(() => {
    const el = storeRef.current.scrollRef
    if (!el) return

    const handler = (e: WheelEvent) => handleWheel(e)
    el.addEventListener("wheel", handler, { passive: true })
    return () => el.removeEventListener("wheel", handler)
  }, [storeRef.current.scrollRef])

  useEffect(() => {
    return () => {
      if (settleTimer) clearTimeout(settleTimer)
      if (autoTimer) clearTimeout(autoTimer)
    }
  }, [])

  return {
    scrollRef: (el: HTMLElement | undefined) => setStore({ scrollRef: el }),
    contentRef: (el: HTMLElement | undefined) => setStore({ contentRef: el }),
    handleScroll,
    handleInteraction,
    pause: stop,
    resume: () => {
      if (storeRef.current.userScrolled) setStore({ userScrolled: false })
      scrollToBottom(true)
    },
    scrollToBottom: () => scrollToBottom(false),
    forceScrollToBottom: () => scrollToBottom(true),
    userScrolled: () => storeRef.current.userScrolled,
  }
}
