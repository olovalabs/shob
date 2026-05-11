import { useCallback, useEffect, useRef, useState, useMemo, type HTMLAttributes } from "react"
import { marked, type Tokens } from "marked"
import DOMPurify from "dompurify"
import "./markdown.css"
import { stream } from "./markdown-stream"

const renderer = new marked.Renderer()
renderer.link = ({ href, title, text }: Tokens.Link) => {
  const titleAttr = title ? ` title="${title}"` : ""
  return `<a href="${href}"${titleAttr} class="external-link" target="_blank" rel="noopener noreferrer">${text}</a>`
}
marked.use({
  renderer,
  breaks: false,
  gfm: true,
})

if (typeof window !== "undefined" && DOMPurify.isSupported) {
  DOMPurify.addHook("afterSanitizeAttributes", (node: Element) => {
    if (!(node instanceof HTMLAnchorElement)) return
    if (node.target !== "_blank") return

    const rel = node.getAttribute("rel") ?? ""
    const set = new Set(rel.split(/\s+/).filter(Boolean))
    set.add("noopener")
    set.add("noreferrer")
    node.setAttribute("rel", Array.from(set).join(" "))
  })
}

const iconPaths = {
  copy: '<path d="M6.2513 6.24935V2.91602H17.0846V13.7493H13.7513M13.7513 6.24935V17.0827H2.91797V6.24935H13.7513Z" stroke="currentColor" stroke-linecap="round"/>',
  check: '<path d="M5 11.9657L8.37838 14.7529L15 5.83398" stroke="currentColor" stroke-linecap="square"/>',
}

const config = {
  USE_PROFILES: { html: true, mathMl: true },
  SANITIZE_NAMED_PROPS: true,
  FORBID_TAGS: ["style"],
  FORBID_CONTENTS: ["style", "script"],
  ADD_TAGS: ["svg", "path"],
  ADD_ATTR: ["d", "viewBox", "preserveAspectRatio", "xmlns"],
}

function sanitize(html: string) {
  if (!DOMPurify.isSupported) return ""
  return DOMPurify.sanitize(html, config)
}

function urlPatternTest(text: string) {
  const urlPattern = /^https?:\/\/[^\s<>()`"']+$/
  const href = text.trim().replace(/[),.;!?]+$/, "")
  if (!urlPattern.test(href)) return null
  try {
    const url = new URL(href)
    return url.toString()
  } catch {
    return null
  }
}

function markCodeLinks(root: HTMLDivElement) {
  const codeNodes = Array.from(root.querySelectorAll(":not(pre) > code"))
  for (const code of codeNodes) {
    const href = urlPatternTest(code.textContent ?? "")
    const parentLink =
      code.parentElement instanceof HTMLAnchorElement && code.parentElement.classList.contains("external-link")
        ? code.parentElement
        : null

    if (!href) {
      if (parentLink) parentLink.replaceWith(code)
      continue
    }

    if (parentLink) {
      parentLink.href = href
      continue
    }

    const link = document.createElement("a")
    link.href = href
    link.className = "external-link"
    link.target = "_blank"
    link.rel = "noopener noreferrer"
    code.parentNode?.replaceChild(link, code)
    link.appendChild(code)
  }
}

function createCopyButton(labels: { copy: string; copied: string }, onClick: () => void) {
  const button = document.createElement("button")
  button.type = "button"
  button.setAttribute("data-component", "icon-button")
  button.setAttribute("data-variant", "secondary")
  button.setAttribute("data-size", "small")
  button.setAttribute("data-slot", "markdown-copy-button")
  button.setAttribute("aria-label", labels.copy)
  button.setAttribute("data-tooltip", labels.copy)

  const copyIcon = document.createElement("div")
  copyIcon.setAttribute("data-component", "icon")
  copyIcon.setAttribute("data-size", "small")
  copyIcon.setAttribute("data-slot", "copy-icon")
  const copySvg = document.createElementNS("http://www.w3.org/2000/svg", "svg")
  copySvg.setAttribute("data-slot", "icon-svg")
  copySvg.setAttribute("fill", "none")
  copySvg.setAttribute("viewBox", "0 0 20 20")
  copySvg.setAttribute("aria-hidden", "true")
  copySvg.innerHTML = iconPaths.copy
  copyIcon.appendChild(copySvg)

  const checkIcon = document.createElement("div")
  checkIcon.setAttribute("data-component", "icon")
  checkIcon.setAttribute("data-size", "small")
  checkIcon.setAttribute("data-slot", "check-icon")
  const checkSvg = document.createElementNS("http://www.w3.org/2000/svg", "svg")
  checkSvg.setAttribute("data-slot", "icon-svg")
  checkSvg.setAttribute("fill", "none")
  checkSvg.setAttribute("viewBox", "0 0 20 20")
  checkSvg.setAttribute("aria-hidden", "true")
  checkSvg.innerHTML = iconPaths.check
  checkIcon.appendChild(checkSvg)

  button.appendChild(copyIcon)
  button.appendChild(checkIcon)
  button.addEventListener("click", onClick)

  return button
}

function ensureCodeWrapper(block: HTMLPreElement, labels: { copy: string; copied: string }, onClick: () => void) {
  const parent = block.parentElement
  if (!parent) return
  const wrapped = parent.getAttribute("data-component") === "markdown-code"
  if (!wrapped) {
    const wrapper = document.createElement("div")
    wrapper.setAttribute("data-component", "markdown-code")
    parent.replaceChild(wrapper, block)
    wrapper.appendChild(block)
    wrapper.appendChild(createCopyButton(labels, onClick))
    return
  }

  const buttons = Array.from(parent.querySelectorAll('[data-slot="markdown-copy-button"]')).filter(
    (el): el is HTMLButtonElement => el instanceof HTMLButtonElement,
  )

  if (buttons.length === 0) {
    parent.appendChild(createCopyButton(labels, onClick))
    return
  }

  for (const button of buttons.slice(1)) {
    button.remove()
  }
}

function decorate(root: HTMLDivElement, labels: { copy: string; copied: string }, onClick: () => void) {
  const blocks = Array.from(root.querySelectorAll("pre"))
  for (const block of blocks) {
    ensureCodeWrapper(block, labels, onClick)
  }
  markCodeLinks(root)
}

function syncAttributes(existingEl: Element, newEl: Element) {
  const newAttrs = new Map(Array.from(newEl.attributes).map((a) => [a.name, a.value]))
  for (const attr of Array.from(existingEl.attributes)) {
    if (!newAttrs.has(attr.name)) {
      existingEl.removeAttribute(attr.name)
    }
  }
  for (const [name, value] of newAttrs) {
    if (existingEl.getAttribute(name) !== value) {
      existingEl.setAttribute(name, value)
    }
  }
}

function nodesMatch(a: Node, b: Node): boolean {
  if (a.nodeType !== b.nodeType) return false
  if (a.nodeType === Node.TEXT_NODE) return true
  if (a.nodeType === Node.ELEMENT_NODE) {
    return (a as Element).tagName === (b as Element).tagName
  }
  return false
}

function updateNodeInPlace(existing: Node, incoming: Node, labels: { copy: string; copied: string }) {
  if (existing.nodeType === Node.TEXT_NODE) {
    if (existing.textContent !== incoming.textContent) {
      existing.textContent = incoming.textContent
    }
    return
  }
  if (existing.nodeType === Node.ELEMENT_NODE) {
    const existingEl = existing as Element
    const incomingEl = incoming as Element
    syncAttributes(existingEl, incomingEl)
    updateDomIncrementally(existingEl, incomingEl.innerHTML, labels)
  }
}

function updateDomIncrementally(container: HTMLDivElement | Element, newHtml: string, labels: { copy: string; copied: string }) {
  const temp = document.createElement("div")
  temp.innerHTML = newHtml
  decorate(temp as HTMLDivElement, labels, () => {})

  const existingChildren = Array.from(container.childNodes)
  const newChildren = Array.from(temp.childNodes)

  // Greedy front+back matching to preserve DOM identity
  let prefixLen = 0
  while (prefixLen < existingChildren.length && prefixLen < newChildren.length) {
    if (nodesMatch(existingChildren[prefixLen], newChildren[prefixLen])) {
      updateNodeInPlace(existingChildren[prefixLen], newChildren[prefixLen], labels)
      prefixLen++
    } else {
      break
    }
  }

  let suffixLen = 0
  while (
    suffixLen < existingChildren.length - prefixLen &&
    suffixLen < newChildren.length - prefixLen
  ) {
    const eIdx = existingChildren.length - 1 - suffixLen
    const nIdx = newChildren.length - 1 - suffixLen
    if (nodesMatch(existingChildren[eIdx], newChildren[nIdx])) {
      updateNodeInPlace(existingChildren[eIdx], newChildren[nIdx], labels)
      suffixLen++
    } else {
      break
    }
  }

  // Remove unmatched nodes from the middle
  for (let i = existingChildren.length - suffixLen - 1; i >= prefixLen; i--) {
    container.removeChild(existingChildren[i])
  }

  // Insert new unmatched nodes in the middle
  const insertBefore = container.childNodes[prefixLen] ?? null
  for (let i = prefixLen; i < newChildren.length - suffixLen; i++) {
    container.insertBefore(newChildren[i].cloneNode(true), insertBefore)
  }
}

type MarkdownProps = Omit<HTMLAttributes<HTMLDivElement>, "children"> & {
  text: string
  streaming?: boolean
}

export function Markdown({ text, streaming = false, className, ...props }: MarkdownProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const [copiedIndex, setCopiedIndex] = useState<number | null>(null)

  const labels = useMemo(() => ({ copy: "Copy", copied: "Copied" }), [])

  const handleCopy = useCallback(async (code: string, index: number) => {
    if (!code) return
    await navigator.clipboard.writeText(code)
    setCopiedIndex(index)
    setTimeout(() => setCopiedIndex(null), 2000)
  }, [])

  useEffect(() => {
    if (!containerRef.current) return

    let sanitized = ""
    try {
      const blocks = stream(text, streaming)
      for (const block of blocks) {
        const rawHtml = marked.parse(block.src, { async: false }) as string
        const safe = sanitize(rawHtml)
        if (safe) sanitized += safe
      }
    } catch (error) {
      console.warn("Markdown parse error:", error)
      sanitized = text
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;")
        .replace(/'/g, "&#39;")
        .replace(/\r\n?/g, "\n")
        .replace(/\n/g, "<br>")
    }

    if (!sanitized) {
      containerRef.current.innerHTML = ""
      return
    }

    if (containerRef.current.childNodes.length === 0) {
      containerRef.current.innerHTML = sanitized
      decorate(containerRef.current, labels, () => {})
    } else {
      updateDomIncrementally(containerRef.current, sanitized, labels)
    }

    if (streaming) {
      containerRef.current.setAttribute("data-streaming", "true")
    } else {
      containerRef.current.removeAttribute("data-streaming")
    }

    const codeBlocks = Array.from(containerRef.current.querySelectorAll('[data-component="markdown-code"]'))
    codeBlocks.forEach((block, index) => {
      const code = block.querySelector("code")
      const button = block.querySelector('[data-slot="markdown-copy-button"]') as HTMLButtonElement | null

      if (!code || !button) return

      button.onclick = () => {
        void handleCopy(code.textContent ?? "", index)
      }

      button.removeAttribute("data-copied")
      button.setAttribute("aria-label", labels.copy)
      button.setAttribute("data-tooltip", labels.copy)
    })
  }, [text, streaming, labels, handleCopy])

  useEffect(() => {
    if (!containerRef.current) return
    const codeBlocks = Array.from(containerRef.current.querySelectorAll('[data-component="markdown-code"]'))
    codeBlocks.forEach((block, index) => {
      const button = block.querySelector('[data-slot="markdown-copy-button"]') as HTMLButtonElement | null
      if (!button) return
      if (copiedIndex === index) {
        button.setAttribute("data-copied", "true")
        button.setAttribute("aria-label", labels.copied)
        button.setAttribute("data-tooltip", labels.copied)
      } else {
        button.removeAttribute("data-copied")
        button.setAttribute("aria-label", labels.copy)
        button.setAttribute("data-tooltip", labels.copy)
      }
    })
  }, [copiedIndex, labels])

  return <div {...props} ref={containerRef} data-component="markdown" className={className} />
}
