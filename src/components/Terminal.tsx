import { useCallback, useEffect, useRef, useState } from "react"
import { Terminal as XTerm } from "@xterm/xterm"
import { FitAddon } from "@xterm/addon-fit"
import { WebLinksAddon } from "@xterm/addon-web-links"
import { SearchAddon } from "@xterm/addon-search"
import { SerializeAddon } from "@xterm/addon-serialize"
import { invoke } from "@tauri-apps/api/core"
import { spawn, type IPty } from "tauri-pty"
import { Search, X, ArrowUp, ArrowDown, Save, Trash2 } from "lucide-react"
import { CLI_ALIAS_TO_ID } from "../config/check"
import { useStore } from "../store"
import { api } from "../services/api"
import { Card } from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import "@xterm/xterm/css/xterm.css"

interface TerminalProps {
  sessionId: string
  isActive?: boolean
  shouldBoot?: boolean
}

interface RerunCliEventDetail {
  sessionId: string
  command: string
}

interface TerminalHostInfo {
  os: string
  windowsBuildNumber?: number | null
}

type TerminalOs = TerminalHostInfo["os"]

const launchedPendingCommandKeys = new Set<string>()
const ACTIVITY_THROTTLE_MS = 15_000
const FIT_SETTLE_DELAYS_MS = [0, 50, 150] as const
const ALLOWED_SHELLS = new Set(["pwsh", "powershell", "cmd", "bash", "zsh", "fish", "sh"])

const DEFAULT_SESSION_NAME_PATTERN = /^Terminal \d+$/
const ANSI_ESCAPE_SEQUENCE = /\x1b(?:\[[0-?]*[ -/]*[@-~]|[@-_]|\].*?(?:\x07|\x1b\\)|P.*?\x1b\\|X.*?\x1b\\|\^.*?\x1b\\|_.*?\x1b\\)/g
const OTHER_CONTROL_CHARS = /[\u0000-\u0008\u000b-\u001f\u007f]/g
const BRACKETED_PASTE_WRAPPER = /\x1b\[(?:200|201)~/g
const OSC_COLOR_FRAGMENT = /(?:^|\s)(?:\d+;(?:rgb:[0-9a-f]+\/[0-9a-f]+\/[0-9a-f]+|\d+;))+/gi
const LEADING_GARBAGE_FRAGMENT = /^(?:\s|;|(?:\d+;)+(?:rgb:[0-9a-f]+\/[0-9a-f]+\/[0-9a-f]+)?)+/i
type CaptureMode = "text" | "escape" | "csi" | "osc" | "dcs" | "string"
const MAX_NAMING_CAPTURE_CHARS = 2048

function stripOptionalWrappingQuotes(value: string): string {
  const trimmed = value.trim()
  if (trimmed.length < 2) return trimmed
  const first = trimmed[0]
  const last = trimmed[trimmed.length - 1]
  if ((first === '"' && last === '"') || (first === "'" && last === "'")) {
    return trimmed.slice(1, -1).trim()
  }

  return trimmed
}

function getShellBasename(shell: string): string {
  const unquotedShell = stripOptionalWrappingQuotes(shell)
  const baseName = unquotedShell.split(/[\\/]/).pop() ?? unquotedShell
  return baseName.toLowerCase().replace(/\.(exe|cmd|bat|ps1)$/, "")
}

function resolveAllowlistedShell(shell: string): string | null {
  const candidate = stripOptionalWrappingQuotes(shell)
  if (!candidate) return null

  const baseName = getShellBasename(candidate)
  if (!ALLOWED_SHELLS.has(baseName)) return null

  return candidate
}

function decodePtyChunk(chunk: unknown, decoder: TextDecoder): string {
  if (typeof chunk === "string") return chunk
  if (chunk instanceof Uint8Array) return decoder.decode(chunk, { stream: true })
  if (Array.isArray(chunk)) return decoder.decode(Uint8Array.from(chunk), { stream: true })
  return ""
}

function supportsClipboardRead() {
  return typeof navigator !== "undefined" && typeof navigator.clipboard?.readText === "function"
}

function isPasteShortcut(event: KeyboardEvent, os: TerminalOs): boolean {
  if (event.repeat) return false
  if (event.altKey) return false

  if (event.key === "Insert") {
    return event.shiftKey && !event.ctrlKey && !event.metaKey
  }

  if (event.key.toLowerCase() !== "v") return false

  if (os === "macos") {
    return event.metaKey && !event.ctrlKey
  }

  if (os === "linux") {
    return event.ctrlKey && event.shiftKey && !event.metaKey
  }

  return event.ctrlKey && !event.metaKey
}

function sanitizeClipboardPasteText(input: string): string {
  return input
    .replace(BRACKETED_PASTE_WRAPPER, "")
    .replace(/\r\n/g, "\n")
    .replace(/[\r\n]+$/g, "")
}

function getShadcnTerminalTheme() {
  return {
    background: "#09090b",
    foreground: "#fafafa",
    cursor: "#fafafa",
    cursorAccent: "#09090b",
    selectionBackground: "rgba(255, 255, 255, 0.15)",
    selectionForeground: "#fafafa",
    black: "#09090b",
    brightBlack: "#71717a",
    red: "#ef4444",
    brightRed: "#f87171",
    green: "#22c55e",
    brightGreen: "#4ade80",
    yellow: "#eab308",
    brightYellow: "#facc15",
    blue: "#3b82f6",
    brightBlue: "#60a5fa",
    magenta: "#a855f7",
    brightMagenta: "#c084fc",
    cyan: "#06b6d4",
    brightCyan: "#22d3ee",
    white: "#fafafa",
    brightWhite: "#ffffff",
  }
}

function toSessionTitle(input: string) {
  return input
    .replace(OSC_COLOR_FRAGMENT, " ")
    .replace(LEADING_GARBAGE_FRAGMENT, "")
    .trim()
    .replace(/\s+/g, " ")
    .slice(0, 80)
}

function normalizeInputForSessionTitle(input: string) {
  return input
    .replace(BRACKETED_PASTE_WRAPPER, "")
    .replace(ANSI_ESCAPE_SEQUENCE, "")
    .replace(OSC_COLOR_FRAGMENT, " ")
    .replace(OTHER_CONTROL_CHARS, "")
}

function parseCliInvocation(input: string): { cliTool: string; promptText: string | null } | null {
  const normalizedInput = input.trim().replace(/\s+/g, " ")
  if (!normalizedInput) return null

  const tokens = normalizedInput.match(/"[^"]*"|'[^']*'|\S+/g) ?? []
  if (tokens.length === 0) return null

  const unwrapToken = (token: string) => token.replace(/^['"]|['"]$/g, "")
  const normalizedTokens = tokens.map((token) => unwrapToken(token))
  const cliIndex = normalizedTokens.findIndex((token) => {
    const baseName = token
      .split(/[\\/]/)
      .pop()
      ?.replace(/\.(cmd|exe|bat|ps1)$/i, "")
      .toLowerCase()

    return baseName ? Boolean(CLI_ALIAS_TO_ID[baseName]) : false
  })

  if (cliIndex === -1) return null

  const baseName = normalizedTokens[cliIndex]
    .split(/[\\/]/)
    .pop()
    ?.replace(/\.(cmd|exe|bat|ps1)$/i, "")
    .toLowerCase()
  const cliTool = baseName ? CLI_ALIAS_TO_ID[baseName] : null
  if (!cliTool) return null

  const promptTokens = normalizedTokens.slice(cliIndex + 1)
  const promptText = promptTokens.length > 0 ? promptTokens.join(" ").trim() : null

  return {
    cliTool,
    promptText: promptText || null,
  }
}

export function Terminal({ sessionId, isActive = true, shouldBoot = true }: TerminalProps) {
  const terminalRef = useRef<HTMLDivElement>(null)
  const xtermRef = useRef<XTerm | null>(null)
  const fitAddonRef = useRef<FitAddon | null>(null)
  const searchAddonRef = useRef<SearchAddon | null>(null)
  const serializeAddonRef = useRef<SerializeAddon | null>(null)
  const ptyRef = useRef<IPty | null>(null)
  const searchInputRef = useRef<HTMLInputElement>(null)
  const [showSearch, setShowSearch] = useState(false)
  const [searchQuery, setSearchQuery] = useState("")
  const decoderRef = useRef(new TextDecoder())
  const inputBufferRef = useRef("")
  const awaitingPromptTitleRef = useRef(false)
  const hasNamedFromPromptRef = useRef(false)
  const hasFlushedPendingLaunchRef = useRef(false)
  const hasRecordedStartupMetricRef = useRef(false)
  const captureModeRef = useRef<CaptureMode>("text")
  const captureEscapePendingRef = useRef(false)
  const fitRafRef = useRef<number | null>(null)
  const spawnInFlightRef = useRef(false)
  const spawnStartedAtRef = useRef<number | null>(null)
  const lastPtySizeRef = useRef<{ rows: number; cols: number } | null>(null)
  const lastPersistedActivityAtRef = useRef(0)
  const startupDurationMsRef = useRef<number | null>(null)
  const sessionProjectId = useStore((state) => {
    const project = state.projects.find((item) => item.sessions.some((session) => session.id === sessionId))
    return project?.id ?? null
  })
  const sessionProjectPath = useStore((state) => {
    const project = state.projects.find((item) => item.sessions.some((session) => session.id === sessionId))
    return project?.path ?? null
  })
  const session = useStore((state) => {
    for (const project of state.projects) {
      const match = project.sessions.find((item) => item.id === sessionId)
      if (match) return match
    }

    return null
  })
  const renameSession = useStore((state) => state.renameSession)
  const updateSession = useStore((state) => state.updateSession)
  const recordSessionActivity = useStore((state) => state.recordSessionActivity)
  const recordSessionCommand = useStore((state) => state.recordSessionCommand)
  const recordSessionStartup = useStore((state) => state.recordSessionStartup)
  const latestSessionRef = useRef(session)
  const latestSessionProjectIdRef = useRef(sessionProjectId)
  const latestSessionProjectPathRef = useRef(sessionProjectPath)

  useEffect(() => {
    latestSessionRef.current = session
  }, [session])

  useEffect(() => {
    latestSessionProjectIdRef.current = sessionProjectId
  }, [sessionProjectId])

  useEffect(() => {
    latestSessionProjectPathRef.current = sessionProjectPath
  }, [sessionProjectPath])

  const fitTerminal = useCallback(() => {
    if (fitRafRef.current !== null) return
    fitRafRef.current = requestAnimationFrame(() => {
      fitRafRef.current = null
      const fit = fitAddonRef.current
      const term = xtermRef.current
      if (fit && term) {
        fit.fit()
        const nextSize = { rows: term.rows, cols: term.cols }
        const lastSize = lastPtySizeRef.current
        if (lastSize && lastSize.rows === nextSize.rows && lastSize.cols === nextSize.cols) {
          return
        }

        lastPtySizeRef.current = nextSize
        ptyRef.current?.resize(nextSize.cols, nextSize.rows)
      }
    })
  }, [])

  useEffect(() => {
    awaitingPromptTitleRef.current = false
    hasNamedFromPromptRef.current = !session || !DEFAULT_SESSION_NAME_PATTERN.test(session.name)
    hasFlushedPendingLaunchRef.current = false
    hasRecordedStartupMetricRef.current = false
    inputBufferRef.current = ""
    captureModeRef.current = "text"
    captureEscapePendingRef.current = false
    spawnStartedAtRef.current = null
    lastPtySizeRef.current = null
    lastPersistedActivityAtRef.current = session?.lastActiveAt ?? 0
    startupDurationMsRef.current = typeof session?.startupDurationMs === "number" ? session.startupDurationMs : null
  }, [sessionId])

  useEffect(() => {
    if (!session || DEFAULT_SESSION_NAME_PATTERN.test(session.name)) return
    hasNamedFromPromptRef.current = true
  }, [session?.name])

  useEffect(() => {
    startupDurationMsRef.current = typeof session?.startupDurationMs === "number" ? session.startupDurationMs : null
  }, [session?.startupDurationMs])

  useEffect(() => {
    if (!terminalRef.current || !session || !shouldBoot) return

    const bootSession = session
    const bootProjectId = sessionProjectId
    const bootProjectPath = sessionProjectPath

    let cancelled = false
    const fitTimeouts: number[] = []
    let term: XTerm | null = null
    let fitAddon: FitAddon | null = null
    let writeFlushScheduled = false
    let isWriteInFlight = false
    const pendingWriteChunks: string[] = []

    const scheduleTerminalFlush = () => {
      if (writeFlushScheduled || isWriteInFlight) return
      writeFlushScheduled = true

      queueMicrotask(() => {
        writeFlushScheduled = false
        if (!term || isWriteInFlight || pendingWriteChunks.length === 0) return

        isWriteInFlight = true
        const chunk = pendingWriteChunks.join("")
        pendingWriteChunks.length = 0

        term.write(chunk, () => {
          isWriteInFlight = false
          if (!cancelled && pendingWriteChunks.length > 0) {
            scheduleTerminalFlush()
          }
        })
      })
    }

    const queueTerminalWrite = (data: string) => {
      if (!data) return
      pendingWriteChunks.push(data)
      scheduleTerminalFlush()
    }

    const pasteFromClipboard = async () => {
      if (!term || !supportsClipboardRead()) return

      try {
        const text = await navigator.clipboard.readText()
        if (!text) return
        const safePasteText = sanitizeClipboardPasteText(text)
        if (!safePasteText) return
        term.focus()
        const pty = ptyRef.current
        if (pty) {
          pty.write(safePasteText)
        } else {
          term.paste(safePasteText)
        }
        term.scrollToBottom()
      } catch (error) {
        console.error("Failed to paste clipboard text into terminal", error)
      }
    }

    const bootTerminal = async () => {
      try {
        if (cancelled || !terminalRef.current) return

        const hostInfo = await invoke<TerminalHostInfo>("get_terminal_host_info")
        const isWindows = hostInfo.os === "windows"
        const windowsBuildNumber =
          typeof hostInfo.windowsBuildNumber === "number" && hostInfo.windowsBuildNumber > 0
            ? hostInfo.windowsBuildNumber
            : null
        const windowsPtyOptions =
          isWindows && windowsBuildNumber
            ? {
              backend: "conpty" as const,
              buildNumber: windowsBuildNumber,
            }
            : undefined

        term = new XTerm({
          cursorBlink: true,
          cursorStyle: "bar",
          cursorInactiveStyle: "outline",
          altClickMovesCursor: false,
          macOptionIsMeta: hostInfo.os === "macos",
          rightClickSelectsWord: false,
          fontSize: 14,
          fontFamily: '"JetBrains Mono", "Cascadia Code", Consolas, monospace',
          fontWeight: "400",
          fontWeightBold: "700",
          lineHeight: 1.22,
          letterSpacing: 0,
          customGlyphs: true,
          rescaleOverlappingGlyphs: true,
          theme: getShadcnTerminalTheme(),
          scrollback: 10000,
          smoothScrollDuration: 0,
          convertEol: false,
          drawBoldTextInBrightColors: true,
          fastScrollSensitivity: 5,
          scrollSensitivity: 1.15,
          windowsPty: windowsPtyOptions,
          documentOverride: terminalRef.current.ownerDocument,
        })

        fitAddon = new FitAddon()
        const searchAddon = new SearchAddon()
        const serializeAddon = new SerializeAddon()

        term.loadAddon(fitAddon)
        term.loadAddon(new WebLinksAddon())
        term.loadAddon(searchAddon)
        term.loadAddon(serializeAddon)
        term.open(terminalRef.current)

        const helperTextarea = terminalRef.current.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea")
        helperTextarea?.setAttribute("autocomplete", "off")
        helperTextarea?.setAttribute("autocorrect", "off")
        helperTextarea?.setAttribute("autocapitalize", "off")
        helperTextarea?.setAttribute("spellcheck", "false")
        helperTextarea?.setAttribute("data-gramm", "false")

        if (isActive) {
          term.focus()
        }

        xtermRef.current = term
        fitAddonRef.current = fitAddon
        searchAddonRef.current = searchAddon
        serializeAddonRef.current = serializeAddon

        fitTerminal()
        fitTimeouts.push(
          ...FIT_SETTLE_DELAYS_MS.map((delay) =>
            window.setTimeout(() => {
              fitTerminal()
            }, delay),
          ),
        )

        initPty()

        term.onData(handleData)
        term.onBinary(handleBinaryData)
        term.onKey(() => {
          term?.scrollToBottom()
        })
        term.attachCustomKeyEventHandler((event) => {
          if (event.type !== "keydown") return true

          // Custom shortcuts
          if (event.key === "f" && (hostInfo.os === "macos" ? event.metaKey : event.ctrlKey) && !event.shiftKey && !event.altKey) {
            event.preventDefault()
            event.stopPropagation()
            setShowSearch(true)
            setTimeout(() => searchInputRef.current?.focus(), 50)
            return false
          }

          if (!supportsClipboardRead()) return true
          if (!isPasteShortcut(event, hostInfo.os)) return true

          event.preventDefault()
          event.stopPropagation()
          void pasteFromClipboard()
          return false
        })
        term.attachCustomWheelEventHandler((event) => {
          term?.focus()
          event.stopPropagation()
          return true
        })

        term.element?.addEventListener("pointerdown", handlePointerDown)
        term.element?.addEventListener("contextmenu", handleContextMenu, true)
      } catch (err) {
        if (!cancelled) {
          if (terminalRef.current) {
            terminalRef.current.textContent = `Error: ${String(err)}`
          }
        }
      }
    }

    if ("fonts" in document) {
      document.fonts.ready
        .then(() => {
          if (!cancelled) fitTerminal()
        })
        .catch(() => { })
    }

    const handleWindowResize = () => fitTerminal()
    window.addEventListener("resize", handleWindowResize)

    const resizeObserver = new ResizeObserver(() => {
      fitTerminal()
    })
    resizeObserver.observe(terminalRef.current)

    const initPty = async () => {
      if (!term || spawnInFlightRef.current) return

      try {
        const savedOutput = await api.loadSessionOutput(sessionId)
        if (savedOutput && !cancelled) {
          term.write(savedOutput)
        }
      } catch (err) {
        console.error('Failed to load session output', err)
      }

      if (cancelled) return


      const resolvedShell = resolveAllowlistedShell(bootSession.shell)
      if (!resolvedShell) {
        term.writeln("\x1b[31mTerminal launch blocked: unsupported shell.\x1b[0m")
        term.writeln(
          "\x1b[33mAllowed shells: pwsh, powershell, cmd, bash, zsh, fish, sh. Update your shell in Settings.\x1b[0m",
        )
        return
      }

      spawnInFlightRef.current = true
      try {
        ptyRef.current?.kill()
        fitAddonRef.current?.fit()
        const spawnRows = Math.max(2, term.rows)
        const spawnCols = Math.max(2, term.cols)
        spawnStartedAtRef.current = Date.now()
        const pty = spawn(resolvedShell, [], {
          cwd: bootProjectPath || undefined,
          rows: spawnRows,
          cols: spawnCols,
          useConpty: true,
          env: {
            TERM: "xterm-256color",
            COLORTERM: "truecolor",
            TERM_PROGRAM: "shob",
          },
        })
        const initPromise = (pty as IPty & { _init?: Promise<unknown> })._init
        if (initPromise) {
          await initPromise
        }

        ptyRef.current = pty
        lastPtySizeRef.current = { rows: spawnRows, cols: spawnCols }
        fitTerminal()
        fitTimeouts.push(
          ...FIT_SETTLE_DELAYS_MS.map((delay) =>
            window.setTimeout(() => {
              fitTerminal()
            }, delay + 40),
          ),
        )
        pty.onData((chunk) => {
          const data = decodePtyChunk(chunk, decoderRef.current)
          if (!data) return

          if (
            latestSessionProjectIdRef.current &&
            spawnStartedAtRef.current &&
            !hasRecordedStartupMetricRef.current &&
            startupDurationMsRef.current === null
          ) {
            hasRecordedStartupMetricRef.current = true
            const startupDurationMs = Math.max(0, Date.now() - spawnStartedAtRef.current)
            recordSessionStartup(latestSessionProjectIdRef.current, sessionId, startupDurationMs, Date.now()).catch(console.error)
          }

          if (latestSessionProjectIdRef.current) {
            const now = Date.now()
            if (now - lastPersistedActivityAtRef.current >= ACTIVITY_THROTTLE_MS) {
              recordSessionActivity(latestSessionProjectIdRef.current, sessionId, now).catch(console.error)
              lastPersistedActivityAtRef.current = now
            }
          }

          window.dispatchEvent(
            new CustomEvent("gg-pty-data", {
              detail: { sessionId, data },
            }),
          )
          queueTerminalWrite(data)
        })

        if (bootSession.pendingLaunchCommand && !hasFlushedPendingLaunchRef.current) {
          const pendingLaunchKey = `${sessionId}:${bootSession.pendingLaunchCommand}`
          if (launchedPendingCommandKeys.has(pendingLaunchKey)) {
            hasFlushedPendingLaunchRef.current = true
            if (bootProjectId) {
              await updateSession(bootProjectId, sessionId, { pendingLaunchCommand: null })
            }
            return
          }

          launchedPendingCommandKeys.add(pendingLaunchKey)
          hasFlushedPendingLaunchRef.current = true
          awaitingPromptTitleRef.current = true
          if (bootProjectId) {
            await updateSession(bootProjectId, sessionId, { pendingLaunchCommand: null })
          }
          pty.write(`${bootSession.pendingLaunchCommand}\r`)
        }
      } catch (err) {
        term?.writeln(`\x1b[31mError: ${err}\x1b[0m`)
      } finally {
        spawnInFlightRef.current = false
      }
    }

    const handlePointerDown = (event: PointerEvent) => {
      if (event.button !== 0) return
      if ((event.target as HTMLElement | null)?.closest("a")) return
      term?.focus()
    }

    const handleContextMenu = (event: MouseEvent) => {
      if ((event.target as HTMLElement | null)?.closest("a")) return
      if (!supportsClipboardRead()) return

      event.preventDefault()
      event.stopPropagation()
      term?.focus()
      void pasteFromClipboard()
    }

    const extractNamingInput = (rawInput: string) => {
      let output = ""

      for (let index = 0; index < rawInput.length; index += 1) {
        const char = rawInput[index]
        const code = char.charCodeAt(0)

        if (captureModeRef.current === "text") {
          if (char === "\x1b") {
            captureModeRef.current = "escape"
            continue
          }

          if (char === "\r" || char === "\n" || char === "\b" || char === "\u007f") {
            output += char
            continue
          }

          if (code >= 0x20) {
            output += char
          }
          continue
        }

        if (captureModeRef.current === "escape") {
          if (char === "[") {
            captureModeRef.current = "csi"
          } else if (char === "]") {
            captureModeRef.current = "osc"
          } else if (char === "P") {
            captureModeRef.current = "dcs"
          } else if (char === "_" || char === "^" || char === "X") {
            captureModeRef.current = "string"
          } else {
            captureModeRef.current = "text"
          }
          continue
        }

        if (captureModeRef.current === "csi") {
          if (code >= 0x40 && code <= 0x7e) {
            captureModeRef.current = "text"
          }
          continue
        }

        if (
          captureModeRef.current === "osc" ||
          captureModeRef.current === "dcs" ||
          captureModeRef.current === "string"
        ) {
          if (captureEscapePendingRef.current) {
            captureEscapePendingRef.current = false
            if (char === "\\") {
              captureModeRef.current = "text"
            } else if (char === "\x1b") {
              captureEscapePendingRef.current = true
            }
            continue
          }

          if (char === "\x07") {
            captureModeRef.current = "text"
            continue
          }

          if (char === "\x1b") {
            captureEscapePendingRef.current = true
          }
        }
      }

      return output
    }

    const commitBufferedInput = (rawInput: string) => {
      const currentSession = latestSessionRef.current
      const currentProjectId = latestSessionProjectIdRef.current
      if (!currentProjectId || !currentSession) return

      const submittedText = normalizeInputForSessionTitle(rawInput).trim()
      if (!submittedText) return
      const now = Date.now()

      const normalizedText = toSessionTitle(submittedText)
      if (!normalizedText) return
      recordSessionCommand(currentProjectId, sessionId, now).catch(console.error)
      lastPersistedActivityAtRef.current = now

      const cliInvocation = parseCliInvocation(submittedText)

      if (cliInvocation) {
        awaitingPromptTitleRef.current = true

        if (currentSession.cliTool !== cliInvocation.cliTool) {
          updateSession(currentProjectId, sessionId, { cliTool: cliInvocation.cliTool }).catch(console.error)
        }

        if (cliInvocation.promptText && !hasNamedFromPromptRef.current) {
          awaitingPromptTitleRef.current = false
          hasNamedFromPromptRef.current = true
          renameSession(currentProjectId, sessionId, toSessionTitle(cliInvocation.promptText)).catch(console.error)
        }
      } else if (!hasNamedFromPromptRef.current && (awaitingPromptTitleRef.current || Boolean(currentSession.cliTool))) {
        awaitingPromptTitleRef.current = false
        hasNamedFromPromptRef.current = true
        renameSession(currentProjectId, sessionId, normalizedText).catch(console.error)
      }
    }

    const handleData = (data: string) => {
      const currentProjectId = latestSessionProjectIdRef.current
      const currentSession = latestSessionRef.current

      if (currentProjectId) {
        const now = Date.now()
        if (now - lastPersistedActivityAtRef.current >= ACTIVITY_THROTTLE_MS) {
          recordSessionActivity(currentProjectId, sessionId, now).catch(console.error)
          lastPersistedActivityAtRef.current = now
        }
      }

      const shouldCaptureForNaming =
        Boolean(currentProjectId) &&
        Boolean(currentSession) &&
        (!hasNamedFromPromptRef.current || awaitingPromptTitleRef.current || Boolean(currentSession?.cliTool))

      if (shouldCaptureForNaming) {
        const cappedData = data.length > MAX_NAMING_CAPTURE_CHARS ? data.slice(0, MAX_NAMING_CAPTURE_CHARS) : data
        const normalizedData = extractNamingInput(cappedData)

        for (let index = 0; index < normalizedData.length; index += 1) {
          const char = normalizedData[index]

          if (char === "\r" || char === "\n") {
            if (inputBufferRef.current) {
              commitBufferedInput(inputBufferRef.current)
              inputBufferRef.current = ""
            }
            continue
          }

          if (char === "\u007f" || char === "\b") {
            inputBufferRef.current = inputBufferRef.current.slice(0, -1)
            continue
          }

          if (char >= " ") {
            inputBufferRef.current += char
          }
        }
      }

      ptyRef.current?.write(data)
    }

    const handleBinaryData = (data: string) => {
      if (!data) return
      ptyRef.current?.write(data)
    }

    void bootTerminal()

    const handleBeforeUnload = () => {
      if (serializeAddonRef.current) {
        const currentOutput = serializeAddonRef.current.serialize()
        if (currentOutput) {
          api.saveSessionOutput(sessionId, currentOutput).catch(console.error)
        }
      }
    }

    window.addEventListener("beforeunload", handleBeforeUnload)

    return () => {
      cancelled = true

      // Save session output on unmount
      handleBeforeUnload()
      window.removeEventListener("beforeunload", handleBeforeUnload)

      for (const timeoutId of fitTimeouts) {
        clearTimeout(timeoutId)
      }
      window.removeEventListener("resize", handleWindowResize)
      resizeObserver.disconnect()
      if (fitRafRef.current !== null) {
        cancelAnimationFrame(fitRafRef.current)
        fitRafRef.current = null
      }
      writeFlushScheduled = false
      pendingWriteChunks.length = 0
      term?.element?.removeEventListener("pointerdown", handlePointerDown)
      term?.element?.removeEventListener("contextmenu", handleContextMenu, true)
      spawnInFlightRef.current = false
      ptyRef.current?.kill()
      ptyRef.current = null
      term?.dispose()
    }
  }, [
    sessionId,
    session?.shell,
    sessionProjectId,
    sessionProjectPath,
    renameSession,
    updateSession,
    recordSessionActivity,
    recordSessionCommand,
    recordSessionStartup,
    shouldBoot,
  ])

  useEffect(() => {
    if (!isActive) return

    const timer = window.setTimeout(() => {
      requestAnimationFrame(() => {
        const term = xtermRef.current
        if (!term) return

        fitTerminal()
        term.scrollToBottom()
        term.focus()
      })
    }, 50)

    return () => window.clearTimeout(timer)
  }, [isActive, fitTerminal])

  useEffect(() => {
    const handleRerunCurrentCli = (event: Event) => {
      const detail = (event as CustomEvent<RerunCliEventDetail>).detail
      if (!detail || detail.sessionId !== sessionId) return
      const currentSession = latestSessionRef.current
      const currentProjectPath = latestSessionProjectPathRef.current
      if (!currentSession) return

      const term = xtermRef.current
      if (!term) return

      const run = async () => {
        if (spawnInFlightRef.current) return

        const resolvedShell = resolveAllowlistedShell(currentSession.shell)
        if (!resolvedShell) {
          term.writeln("\x1b[31mTerminal relaunch blocked: unsupported shell.\x1b[0m")
          term.writeln(
            "\x1b[33mAllowed shells: pwsh, powershell, cmd, bash, zsh, fish, sh. Update your shell in Settings.\x1b[0m",
          )
          return
        }

        spawnInFlightRef.current = true
        try {
          ptyRef.current?.kill()
          fitAddonRef.current?.fit()
          const spawnRows = Math.max(2, term.rows)
          const spawnCols = Math.max(2, term.cols)
          const pty = spawn(resolvedShell, [], {
            cwd: currentProjectPath || undefined,
            rows: spawnRows,
            cols: spawnCols,
            useConpty: true,
            env: {
              TERM: "xterm-256color",
              COLORTERM: "truecolor",
              TERM_PROGRAM: "shob",
            },
          })
          const initPromise = (pty as IPty & { _init?: Promise<unknown> })._init
          if (initPromise) {
            await initPromise
          }
          ptyRef.current = pty
          lastPtySizeRef.current = { rows: spawnRows, cols: spawnCols }
          fitTerminal()
          pty.onData((chunk) => {
            const data = decodePtyChunk(chunk, decoderRef.current)
            if (!data) return
            window.dispatchEvent(
              new CustomEvent("gg-pty-data", {
                detail: { sessionId, data },
              }),
            )
            xtermRef.current?.write(data)
          })
          pty.write(`${detail.command}\r`)
          awaitingPromptTitleRef.current = true
        } catch (error) {
          term.writeln(`\x1b[31mError: ${error}\x1b[0m`)
        } finally {
          spawnInFlightRef.current = false
        }
      }

      void run()
    }

    window.addEventListener("gg-rerun-cli-current-session", handleRerunCurrentCli as EventListener)
    return () => window.removeEventListener("gg-rerun-cli-current-session", handleRerunCurrentCli as EventListener)
  }, [sessionId])

  const handleSearchNext = () => {
    if (searchAddonRef.current && searchQuery) {
      searchAddonRef.current.findNext(searchQuery)
    }
  }

  const handleSearchPrev = () => {
    if (searchAddonRef.current && searchQuery) {
      searchAddonRef.current.findPrevious(searchQuery)
    }
  }

  const handleSaveOutput = () => {
    if (serializeAddonRef.current) {
      const output = serializeAddonRef.current.serialize()
      const blob = new Blob([output], { type: "text/plain" })
      const url = URL.createObjectURL(blob)
      const a = document.createElement("a")
      a.href = url
      a.download = `terminal-output-${sessionId}-${Date.now()}.txt`
      a.click()
      URL.revokeObjectURL(url)
    }
  }

  const handleClearTerminal = () => {
    if (xtermRef.current) {
      xtermRef.current.clear()
    }
  }

  return (
    <Card
      className="terminal-container absolute inset-0 h-full w-full min-h-0 min-w-0 overflow-hidden rounded-none border-0 bg-background p-0"
      data-active={isActive ? "true" : "false"}
      style={{
        visibility: isActive ? "visible" : "hidden",
        pointerEvents: isActive ? "auto" : "none",
      }}
    >
      <div className="absolute right-4 top-2 z-10 flex items-center gap-1 opacity-0 transition-opacity hover:opacity-100 group-hover:opacity-100 terminal-toolbar">
        <Button
          type="button"
          onClick={() => {
            setShowSearch(true)
            setTimeout(() => searchInputRef.current?.focus(), 50)
          }}
          variant="secondary"
          size="icon-xs"
          className="h-6 w-6 bg-background/80 backdrop-blur"
          title="Search (Ctrl+F)"
        >
          <Search className="h-3.5 w-3.5" />
        </Button>
        <Button
          type="button"
          onClick={handleClearTerminal}
          variant="secondary"
          size="icon-xs"
          className="h-6 w-6 bg-background/80 backdrop-blur"
          title="Clear Terminal"
        >
          <Trash2 className="h-3.5 w-3.5 text-muted-foreground" />
        </Button>
        <Button
          type="button"
          onClick={handleSaveOutput}
          variant="secondary"
          size="icon-xs"
          className="h-6 w-6 bg-background/80 backdrop-blur"
          title="Save Output"
        >
          <Save className="h-3.5 w-3.5 text-muted-foreground" />
        </Button>
      </div>

      {showSearch && (
        <div className="absolute right-4 top-10 z-20 flex items-center gap-1 rounded-md border bg-popover px-2 py-1.5 shadow-md">
          <input
            ref={searchInputRef}
            className="w-40 bg-transparent px-1 text-sm outline-none placeholder:text-muted-foreground"
            placeholder="Find..."
            value={searchQuery}
            onChange={(e) => {
              setSearchQuery(e.target.value)
              if (searchAddonRef.current && e.target.value) {
                searchAddonRef.current.findNext(e.target.value)
              }
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                if (e.shiftKey) handleSearchPrev()
                else handleSearchNext()
              } else if (e.key === "Escape") {
                setShowSearch(false)
                xtermRef.current?.focus()
              }
            }}
          />
          <div className="flex items-center gap-0.5 border-l pl-1">
            <Button
              type="button"
              onClick={handleSearchPrev}
              variant="ghost"
              size="icon-xs"
              className="h-6 w-6"
            >
              <ArrowUp className="h-3.5 w-3.5" />
            </Button>
            <Button
              type="button"
              onClick={handleSearchNext}
              variant="ghost"
              size="icon-xs"
              className="h-6 w-6"
            >
              <ArrowDown className="h-3.5 w-3.5" />
            </Button>
            <Button
              type="button"
              onClick={() => {
                setShowSearch(false)
                xtermRef.current?.focus()
              }}
              variant="ghost"
              size="icon-xs"
              className="h-6 w-6"
            >
              <X className="h-3.5 w-3.5" />
            </Button>
          </div>
        </div>
      )}

      <div
        ref={terminalRef}
        className="terminal-wrapper h-full w-full min-h-0 min-w-0 overflow-hidden"
        style={{
          backgroundColor: "#09090b",
        }}
      />
    </Card>
  )
}
