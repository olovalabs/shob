import { useCallback, useEffect, useRef, useState, useMemo } from "react"
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

function elementsEqual(a: Element, b: Element): boolean {
  if (a.tagName !== b.tagName) return false
  if (a.childNodes.length !== b.childNodes.length) return false
  for (let i = 0; i < a.childNodes.length; i++) {
    const ca = a.childNodes[i]
    const cb = b.childNodes[i]
    if (ca.nodeType !== cb.nodeType) return false
    if (ca.nodeType === Node.TEXT_NODE) {
      if (ca.textContent !== cb.textContent) return false
    } else if (ca.nodeType === Node.ELEMENT_NODE) {
      if (!elementsEqual(ca as Element, cb as Element)) return false
    }
  }
  return true
}

function updateDomIncrementally(container: HTMLDivElement, newHtml: string, labels: { copy: string; copied: string }) {
  const temp = document.createElement("div")
  temp.innerHTML = newHtml
  decorate(temp, labels, () => {})

  const existingChildren = Array.from(container.childNodes)
  const newChildren = Array.from(temp.childNodes)

  const maxLen = Math.max(existingChildren.length, newChildren.length)

  for (let i = 0; i < maxLen; i++) {
    if (i >= newChildren.length) {
      container.removeChild(existingChildren[i])
    } else if (i >= existingChildren.length) {
      container.appendChild(newChildren[i])
    } else {
      const existing = existingChildren[i]
      const newNode = newChildren[i]
      if (existing.nodeType === Node.TEXT_NODE && newNode.nodeType === Node.TEXT_NODE) {
        if (existing.textContent !== newNode.textContent) {
          existing.textContent = newNode.textContent
        }
      } else if (existing.nodeType === Node.ELEMENT_NODE && newNode.nodeType === Node.ELEMENT_NODE) {
        const existingEl = existing as Element
        const newEl = newNode as Element
        if (!elementsEqual(existingEl, newEl)) {
          container.replaceChild(newEl, existing)
        }
      } else {
        container.replaceChild(newNode, existing)
      }
    }
  }
}

export function Markdown({ text, streaming = false }: { text: string; streaming?: boolean }) {
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

    const blocks = stream(text, streaming)
    let sanitized = ""
    for (const block of blocks) {
      const rawHtml = marked.parse(block.src, { async: false }) as string
      const safe = sanitize(rawHtml)
      if (safe) sanitized += safe
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

    const codeBlocks = Array.from(containerRef.current.querySelectorAll('[data-component="markdown-code"]'))
    codeBlocks.forEach((block, index) => {
      const code = block.querySelector("code")
      const button = block.querySelector('[data-slot="markdown-copy-button"]') as HTMLButtonElement | null

      if (!code || !button) return

      const handleClick = async () => {
        await handleCopy(code.textContent ?? "", index)
      }

      button.onclick = null
      button.addEventListener("click", handleClick)

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
  }, [text, copiedIndex, labels, handleCopy])

  return <div ref={containerRef} data-component="markdown" />
}
