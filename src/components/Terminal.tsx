import { useCallback, useEffect, useRef } from "react"
import { Terminal as XTerm } from "@xterm/xterm"
import { FitAddon } from "@xterm/addon-fit"
import { WebLinksAddon } from "@xterm/addon-web-links"
import { invoke } from "@tauri-apps/api/core"
import { spawn, type IPty } from "tauri-pty"
import { CLI_ALIAS_TO_ID } from "../config/check"
import { useStore } from "../store"
import { Card } from "@/components/ui/card"
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
  const ptyRef = useRef<IPty | null>(null)
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
    let writeFlushRaf: number | null = null
    let isWriteInFlight = false
    const pendingWriteChunks: string[] = []

    const scheduleTerminalFlush = () => {
      if (writeFlushRaf !== null || isWriteInFlight) return

      writeFlushRaf = requestAnimationFrame(() => {
        writeFlushRaf = null
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
        term.loadAddon(fitAddon)
        term.loadAddon(new WebLinksAddon())
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
        term.attachCustomWheelEventHandler((event) => {
          term?.focus()
          event.stopPropagation()
          return true
        })

        term.element?.addEventListener("pointerdown", handlePointerDown)
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
        .catch(() => {})
    }

    const handleWindowResize = () => fitTerminal()
    window.addEventListener("resize", handleWindowResize)

    const resizeObserver = new ResizeObserver(() => {
      fitTerminal()
    })
    resizeObserver.observe(terminalRef.current)
    
    const initPty = async () => {
      if (!term || spawnInFlightRef.current) return

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

      if (currentProjectId && currentSession) {
        const normalizedData = extractNamingInput(data)

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
    
    return () => {
      cancelled = true
      for (const timeoutId of fitTimeouts) {
        clearTimeout(timeoutId)
      }
      window.removeEventListener("resize", handleWindowResize)
      resizeObserver.disconnect()
      if (fitRafRef.current !== null) {
        cancelAnimationFrame(fitRafRef.current)
        fitRafRef.current = null
      }
      if (writeFlushRaf !== null) {
        cancelAnimationFrame(writeFlushRaf)
      }
      pendingWriteChunks.length = 0
      term?.element?.removeEventListener("pointerdown", handlePointerDown)
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
  
  return (
    <Card
      className="terminal-container absolute inset-0 h-full w-full min-h-0 min-w-0 overflow-hidden rounded-none border-0 bg-background p-0"
      data-active={isActive ? "true" : "false"}
      style={{
        visibility: isActive ? "visible" : "hidden",
        pointerEvents: isActive ? "auto" : "none",
      }}
    >
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
