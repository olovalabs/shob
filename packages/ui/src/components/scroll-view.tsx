import { useEffect, useRef, useState, type ComponentPropsWithoutRef } from "react"
import { useI18n } from "../context/i18n"

export interface ScrollViewProps extends ComponentPropsWithoutRef<"div"> {
  viewportRef?: (el: HTMLDivElement) => void
  orientation?: "vertical" | "horizontal"
}

export const scrollKey = (event: Pick<KeyboardEvent, "key" | "altKey" | "ctrlKey" | "metaKey" | "shiftKey">) => {
  if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return

  switch (event.key) {
    case "PageDown":
      return "page-down"
    case "PageUp":
      return "page-up"
    case "Home":
      return "home"
    case "End":
      return "end"
    case "ArrowUp":
      return "up"
    case "ArrowDown":
      return "down"
  }
}

export function ScrollView(props: ScrollViewProps) {
  const i18n = useI18n()

  const {
    class: className,
    children,
    viewportRef: viewportRefProp,
    orientation = "vertical",
    style,
    onScroll,
    onWheel,
    onTouchStart,
    onTouchMove,
    onTouchEnd,
    onTouchCancel,
    onPointerDown: onPointerDownProp,
    onClick,
    onKeyDown: onKeyDownProp,
    ...rest
  } = props

  const rootRef = useRef<HTMLDivElement>(null)
  const viewportRef = useRef<HTMLDivElement>(null)
  const thumbRef = useRef<HTMLDivElement>(null)

  const [isHovered, setIsHovered] = useState(false)
  const [isDragging, setIsDragging] = useState(false)
  const [thumbHeight, setThumbHeight] = useState(0)
  const [thumbTop, setThumbTop] = useState(0)
  const [showThumb, setShowThumb] = useState(false)

  const updateThumb = () => {
    const vp = viewportRef.current
    if (!vp) return
    const { scrollTop, scrollHeight, clientHeight } = vp

    if (scrollHeight <= clientHeight || scrollHeight === 0) {
      setShowThumb(false)
      return
    }

    setShowThumb(true)
    const trackPadding = 8
    const trackHeight = clientHeight - trackPadding * 2

    const minThumbHeight = 32
    let height = (clientHeight / scrollHeight) * trackHeight
    height = Math.max(height, minThumbHeight)

    const maxScrollTop = scrollHeight - clientHeight
    const maxThumbTop = trackHeight - height

    const top = maxScrollTop > 0 ? (scrollTop / maxScrollTop) * maxThumbTop : 0

    const boundedTop = trackPadding + Math.max(0, Math.min(top, maxThumbTop))

    setThumbHeight(height)
    setThumbTop(boundedTop)
  }

  useEffect(() => {
    if (viewportRefProp && viewportRef.current) {
      viewportRefProp(viewportRef.current)
    }

    const vp = viewportRef.current
    if (!vp) return

    const observer = new ResizeObserver(updateThumb)
    observer.observe(vp)
    if (vp.firstElementChild) observer.observe(vp.firstElementChild)

    updateThumb()

    return () => observer.disconnect()
  }, [])

  let startY = 0
  let startScrollTop = 0

  const onThumbPointerDown = (e: React.PointerEvent) => {
    e.preventDefault()
    e.stopPropagation()
    setIsDragging(true)
    const vp = viewportRef.current
    const thumb = thumbRef.current
    if (!vp || !thumb) return

    startY = e.clientY
    startScrollTop = vp.scrollTop

    thumb.setPointerCapture(e.pointerId)

    const onPointerMove = (e: PointerEvent) => {
      const deltaY = e.clientY - startY
      const { scrollHeight, clientHeight } = vp
      const maxScrollTop = scrollHeight - clientHeight
      const maxThumbTop = clientHeight - thumbHeight

      if (maxThumbTop > 0) {
        const scrollDelta = deltaY * (maxScrollTop / maxThumbTop)
        vp.scrollTop = startScrollTop + scrollDelta
      }
    }

    const onPointerUp = (e: PointerEvent) => {
      setIsDragging(false)
      thumb.releasePointerCapture(e.pointerId)
      thumb.removeEventListener("pointermove", onPointerMove)
      thumb.removeEventListener("pointerup", onPointerUp)
    }

    thumb.addEventListener("pointermove", onPointerMove)
    thumb.addEventListener("pointerup", onPointerUp)
  }

  const handleKeyDown = (e: React.KeyboardEvent) => {
    const vp = viewportRef.current
    if (!vp) return

    if (document.activeElement && ["INPUT", "TEXTAREA", "SELECT"].includes(document.activeElement.tagName)) {
      return
    }

    const next = scrollKey(e)
    if (!next) return

    const scrollAmount = vp.clientHeight * 0.8
    const lineAmount = 40

    switch (next) {
      case "page-down":
        e.preventDefault()
        vp.scrollBy({ top: scrollAmount, behavior: "smooth" })
        break
      case "page-up":
        e.preventDefault()
        vp.scrollBy({ top: -scrollAmount, behavior: "smooth" })
        break
      case "home":
        e.preventDefault()
        vp.scrollTo({ top: 0, behavior: "smooth" })
        break
      case "end":
        e.preventDefault()
        vp.scrollTo({ top: vp.scrollHeight, behavior: "smooth" })
        break
      case "up":
        e.preventDefault()
        vp.scrollBy({ top: -lineAmount, behavior: "smooth" })
        break
      case "down":
        e.preventDefault()
        vp.scrollBy({ top: lineAmount, behavior: "smooth" })
        break
    }
  }

  return (
    <div
      ref={rootRef}
      className={`scroll-view ${className || ""}`}
      style={style}
      onPointerEnter={() => setIsHovered(true)}
      onPointerLeave={() => setIsHovered(false)}
      {...rest}
    >
      <div
        ref={viewportRef}
        className="scroll-view__viewport"
        onScroll={(e) => {
          updateThumb()
          if (typeof onScroll === "function") (onScroll as any)(e)
        }}
        onWheel={onWheel as any}
        onTouchStart={onTouchStart as any}
        onTouchMove={onTouchMove as any}
        onTouchEnd={onTouchEnd as any}
        onTouchCancel={onTouchCancel as any}
        onPointerDown={onPointerDownProp as any}
        onClick={onClick as any}
        tabIndex={0}
        role="region"
        aria-label={i18n.t("ui.scrollView.ariaLabel")}
        onKeyDown={(e) => {
          handleKeyDown(e)
          if (typeof onKeyDownProp === "function") (onKeyDownProp as any)(e)
        }}
      >
        {children}
      </div>

      {showThumb && (
        <div
          ref={thumbRef}
          onPointerDown={onThumbPointerDown}
          className="scroll-view__thumb"
          data-visible={isHovered || isDragging}
          data-dragging={isDragging}
          style={{
            height: `${thumbHeight}px`,
            transform: `translateY(${thumbTop}px)`,
            zIndex: 100,
          }}
        />
      )}
    </div>
  )
}
