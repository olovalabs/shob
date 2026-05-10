import { useEffect, useRef, useState } from "react"

export type FindHost = {
  element: () => HTMLElement | undefined
  open: () => void
  close: () => void
  next: (dir: 1 | -1) => void
  isOpen: () => boolean
}

const hosts = new Set<FindHost>()
let target: FindHost | undefined
let current: FindHost | undefined
let installed = false

function isEditable(node: unknown): boolean {
  if (!(node instanceof HTMLElement)) return false
  if (node.closest("[data-prevent-autofocus]")) return true
  if (node.isContentEditable) return true
  return /^(INPUT|TEXTAREA|SELECT|BUTTON)$/.test(node.tagName)
}

function hostForNode(node: unknown) {
  if (!(node instanceof Node)) return
  for (const host of hosts) {
    const el = host.element()
    if (el && el.isConnected && el.contains(node)) return host
  }
}

function installShortcuts() {
  if (installed) return
  if (typeof window === "undefined") return
  installed = true

  window.addEventListener(
    "keydown",
    (event) => {
      if (event.defaultPrevented) return
      if (isEditable(event.target)) return

      const mod = event.metaKey || event.ctrlKey
      if (!mod) return

      const key = event.key.toLowerCase()
      if (key === "g") {
        const host = current
        if (!host || !host.isOpen()) return
        event.preventDefault()
        event.stopPropagation()
        host.next(event.shiftKey ? -1 : 1)
        return
      }

      if (key !== "f") return

      const active = current
      if (active && active.isOpen()) {
        event.preventDefault()
        event.stopPropagation()
        active.open()
        return
      }

      const host = hostForNode(document.activeElement) ?? hostForNode(event.target) ?? target ?? Array.from(hosts)[0]
      if (!host) return

      event.preventDefault()
      event.stopPropagation()
      host.open()
    },
    { capture: true },
  )
}

function clearHighlightFind() {
  const api = (globalThis as { CSS?: { highlights?: { delete: (name: string) => void } } }).CSS?.highlights
  if (!api) return
  api.delete("opencode-find")
  api.delete("opencode-find-current")
}

function supportsHighlights() {
  const g = globalThis as unknown as { CSS?: { highlights?: unknown }; Highlight?: unknown }
  return typeof g.Highlight === "function" && g.CSS?.highlights != null
}

function scrollParent(el: HTMLElement): HTMLElement | undefined {
  let parent = el.parentElement
  while (parent) {
    const style = getComputedStyle(parent)
    if (style.overflowY === "auto" || style.overflowY === "scroll") return parent
    parent = parent.parentElement
  }
}

type CreateFileFindOptions = {
  wrapper: () => HTMLElement | undefined
  overlay: () => HTMLDivElement | undefined
  getRoot: () => ShadowRoot | undefined
}

export function createFileFind(opts: CreateFileFindOptions) {
  let input: HTMLInputElement | undefined
  let overlayFrame: number | undefined
  let mode: "highlights" | "overlay" = "overlay"
  let hits: Range[] = []
  const [overlayScroll, setOverlayScroll] = useState<HTMLElement[]>([])

  const [state, setState] = useState({
    open: false,
    query: "",
    index: 0,
    count: 0,
    pos: { top: 8, right: 8 },
  })

  const clearOverlayScroll = () => {
    setOverlayScroll([])
  }

  const clearOverlay = () => {
    const el = opts.overlay()
    if (!el) return
    if (overlayFrame !== undefined) {
      cancelAnimationFrame(overlayFrame)
      overlayFrame = undefined
    }
    el.innerHTML = ""
  }

  const renderOverlay = () => {
    if (mode !== "overlay") {
      clearOverlay()
      return
    }

    const wrapper = opts.wrapper()
    const overlay = opts.overlay()
    if (!wrapper || !overlay) return

    clearOverlay()
    if (hits.length === 0) return

    const base = wrapper.getBoundingClientRect()
    const currentIndex = state.index
    const frag = document.createDocumentFragment()

    for (let i = 0; i < hits.length; i++) {
      const range = hits[i]
      const active = i === currentIndex
      for (const rect of Array.from(range.getClientRects())) {
        if (!rect.width || !rect.height) continue

        const mark = document.createElement("div")
        mark.style.position = "absolute"
        mark.style.left = `${Math.round(rect.left - base.left)}px`
        mark.style.top = `${Math.round(rect.top - base.top)}px`
        mark.style.width = `${Math.round(rect.width)}px`
        mark.style.height = `${Math.round(rect.height)}px`
        mark.style.borderRadius = "2px"
        mark.style.backgroundColor = active ? "var(--surface-warning-strong)" : "var(--surface-warning-base)"
        mark.style.opacity = active ? "0.55" : "0.35"
        if (active) mark.style.boxShadow = "inset 0 0 0 1px var(--border-warning-base)"
        frag.appendChild(mark)
      }
    }

    overlay.appendChild(frag)
  }

  function scheduleOverlay() {
    if (mode !== "overlay") return
    if (!state.open) return
    if (overlayFrame !== undefined) return

    overlayFrame = requestAnimationFrame(() => {
      overlayFrame = undefined
      renderOverlay()
    })
  }

  const syncOverlayScroll = () => {
    if (mode !== "overlay") return
    const root = opts.getRoot()

    const next = root
      ? Array.from(root.querySelectorAll("[data-code]")).filter(
          (node): node is HTMLElement => node instanceof HTMLElement,
        )
      : []
    const current = overlayScroll
    if (next.length === current.length && next.every((el, i) => el === current[i])) return

    clearOverlayScroll()
    setOverlayScroll(next)
  }

  const clearFind = () => {
    clearHighlightFind()
    clearOverlay()
    clearOverlayScroll()
    hits = []
    setState((prev) => ({ ...prev, count: 0, index: 0 }))
  }

  const positionBar = () => {
    if (typeof window === "undefined") return
    const wrapper = opts.wrapper()
    if (!wrapper) return

    const root = scrollParent(wrapper) ?? wrapper
    const rect = root.getBoundingClientRect()
    const title = parseFloat(getComputedStyle(root).getPropertyValue("--session-title-height"))
    const header = Number.isNaN(title) ? 0 : title

    setState((prev) => ({
      ...prev,
      pos: {
        top: Math.round(rect.top) + header - 4,
        right: Math.round(window.innerWidth - rect.right) + 8,
      },
    }))
  }

  const scan = (root: ShadowRoot, value: string) => {
    const needle = value.toLowerCase()
    const ranges: Range[] = []
    const cols = Array.from(root.querySelectorAll("[data-content] [data-line], [data-column-content]")).filter(
      (node): node is HTMLElement => node instanceof HTMLElement,
    )

    for (const col of cols) {
      const text = col.textContent
      if (!text) continue

      const hay = text.toLowerCase()
      let at = hay.indexOf(needle)
      if (at === -1) continue

      const nodes: Text[] = []
      const ends: number[] = []
      const walker = document.createTreeWalker(col, NodeFilter.SHOW_TEXT)
      let node = walker.nextNode()
      let pos = 0
      while (node) {
        if (node instanceof Text) {
          pos += node.data.length
          nodes.push(node)
          ends.push(pos)
        }
        node = walker.nextNode()
      }
      if (nodes.length === 0) continue

      const locate = (offset: number) => {
        let lo = 0
        let hi = ends.length - 1
        while (lo < hi) {
          const mid = (lo + hi) >> 1
          if (ends[mid] >= offset) hi = mid
          else lo = mid + 1
        }
        const prev = lo === 0 ? 0 : ends[lo - 1]
        return { node: nodes[lo], offset: offset - prev }
      }

      while (at !== -1) {
        const start = locate(at)
        const end = locate(at + value.length)
        const range = document.createRange()
        range.setStart(start.node, start.offset)
        range.setEnd(end.node, end.offset)
        ranges.push(range)
        at = hay.indexOf(needle, at + value.length)
      }
    }

    return ranges
  }

  const scrollToRange = (range: Range) => {
    const start = range.startContainer
    const el = start instanceof Element ? start : start.parentElement
    el?.scrollIntoView({ block: "center", inline: "center" })
  }

  const setHighlights = (ranges: Range[], currentIndex: number) => {
    const api = (globalThis as unknown as { CSS?: { highlights?: any }; Highlight?: any }).CSS?.highlights
    const Highlight = (globalThis as unknown as { Highlight?: any }).Highlight
    if (!api || typeof Highlight !== "function") return false

    api.delete("opencode-find")
    api.delete("opencode-find-current")

    const active = ranges[currentIndex]
    if (active) api.set("opencode-find-current", new Highlight(active))

    const rest = ranges.filter((_, i) => i !== currentIndex)
    if (rest.length > 0) api.set("opencode-find", new Highlight(...rest))
    return true
  }

  const apply = (args?: { reset?: boolean; scroll?: boolean }) => {
    if (!state.open) return

    const value = state.query.trim()
    if (!value) {
      clearFind()
      return
    }

    const root = opts.getRoot()
    if (!root) return

    mode = supportsHighlights() ? "highlights" : "overlay"

    const ranges = scan(root, value)
    const total = ranges.length
    const desired = args?.reset ? 0 : state.index
    const currentIndex = total ? Math.min(desired, total - 1) : 0

    hits = ranges
    setState((prev) => ({ ...prev, count: total, index: currentIndex }))

    const active = ranges[currentIndex]
    if (mode === "highlights") {
      clearOverlay()
      clearOverlayScroll()
      if (!setHighlights(ranges, currentIndex)) {
        mode = "overlay"
        clearHighlightFind()
        syncOverlayScroll()
        scheduleOverlay()
      }
      if (args?.scroll && active) scrollToRange(active)
      return
    }

    clearHighlightFind()
    syncOverlayScroll()
    if (args?.scroll && active) scrollToRange(active)
    scheduleOverlay()
  }

  const close = () => {
    setState({ open: false, query: "", index: 0, count: 0, pos: state.pos })
    clearFind()
    if (current === host) current = undefined
  }

  const focus = () => {
    if (current && current !== host) current.close()
    current = host
    target = host
    if (!state.open) setState((prev) => ({ ...prev, open: true }))
    requestAnimationFrame(() => {
      apply({ scroll: true })
      input?.focus()
      input?.select()
    })
  }

  const next = (dir: 1 | -1) => {
    if (!state.open) return
    const total = state.count
    if (total <= 0) return

    const currentIndex = (state.index + dir + total) % total
    setState((prev) => ({ ...prev, index: currentIndex }))

    const active = hits[currentIndex]
    if (!active) return

    if (mode === "highlights") {
      if (!setHighlights(hits, currentIndex)) {
        mode = "overlay"
        apply({ reset: true, scroll: true })
        return
      }
      scrollToRange(active)
      return
    }

    clearHighlightFind()
    syncOverlayScroll()
    scrollToRange(active)
    scheduleOverlay()
  }

  const host: FindHost = {
    element: opts.wrapper,
    isOpen: () => state.open,
    next,
    open: focus,
    close,
  }

  const overlayScrollRef = useRef(overlayScroll)
  overlayScrollRef.current = overlayScroll

  useEffect(() => {
    const els = overlayScrollRef.current
    const handlers: (() => void)[] = []
    for (const el of els) {
      const handler = () => scheduleOverlay()
      el.addEventListener("scroll", handler, { passive: true })
      handlers.push(() => el.removeEventListener("scroll", handler))
    }
    return () => handlers.forEach((h) => h())
  })

  useEffect(() => {
    mode = supportsHighlights() ? "highlights" : "overlay"
    installShortcuts()
    hosts.add(host)
    if (!target) target = host

    return () => {
      hosts.delete(host)
      if (current === host) {
        current = undefined
        clearHighlightFind()
      }
      if (target === host) target = undefined
    }
  }, [])

  useEffect(() => {
    if (!state.open) return

    const update = () => positionBar()
    requestAnimationFrame(update)

    window.addEventListener("resize", update, { passive: true })

    const wrapper = opts.wrapper()
    if (!wrapper) return
    const root = scrollParent(wrapper) ?? wrapper
    const observer = new ResizeObserver(update)
    observer.observe(root)

    return () => {
      window.removeEventListener("resize", update)
      observer.disconnect()
    }
  }, [state.open])

  return {
    open: () => state.open,
    query: () => state.query,
    count: () => state.count,
    index: () => state.index,
    pos: () => state.pos,
    setInput: (el: HTMLInputElement) => {
      input = el
    },
    setQuery: (value: string) => {
      setState((prev) => ({ ...prev, query: value, index: 0 }))
      apply({ reset: true, scroll: true })
    },
    focus,
    close,
    next,
    refresh: (args?: { reset?: boolean; scroll?: boolean }) => apply(args),
    onPointerDown: () => {
      target = host
      opts.wrapper()?.focus({ preventScroll: true })
    },
    onFocus: () => {
      target = host
    },
    onInputKeyDown: (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault()
        close()
        return
      }
      if (event.key !== "Enter") return
      event.preventDefault()
      next(event.shiftKey ? -1 : 1)
    },
  }
}
