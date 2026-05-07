import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { Terminal as XTerm } from "@xterm/xterm"
import { FitAddon } from "@xterm/addon-fit"
import { WebLinksAddon } from "@xterm/addon-web-links"
import { Plus, SquareTerminal, X } from "lucide-react"
import { nativeApi } from "../services/native"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"

interface BottomTerminalPanelProps {
  projectPath?: string | null
  height: number
  onClose: () => void
  onResizeStart: (event: React.PointerEvent<HTMLDivElement>) => void
}

const getShellName = (shell: string) => {
  const name = shell.split(/[\\/]/).pop() ?? shell
  return name.replace(/\.(exe|cmd|bat|ps1)$/i, "")
}

const getTerminalTheme = () => ({
  background: "#0c0c0c",
  foreground: "#f4f4f5",
  cursor: "#f4f4f5",
  cursorAccent: "#0c0c0c",
  selectionBackground: "rgba(255, 255, 255, 0.16)",
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
})

export function BottomTerminalPanel({
  projectPath,
  height,
  onClose,
  onResizeStart,
}: BottomTerminalPanelProps) {
  const terminalRef = useRef<HTMLDivElement>(null)
  const xtermRef = useRef<XTerm | null>(null)
  const fitAddonRef = useRef<FitAddon | null>(null)
  const [terminalId, setTerminalId] = useState(() => `bottom-terminal-${crypto.randomUUID()}`)
  const preferredShell = useStore((state) => state.preferredShell)
  const availableShells = useStore((state) => state.availableShells)
  const shell = useMemo(
    () =>
      availableShells.find((item) => item === preferredShell) ??
      availableShells[0] ??
      preferredShell ??
      (nativeApi.platform() === "windows" ? "powershell.exe" : "bash"),
    [availableShells, preferredShell],
  )
  const title = getShellName(shell)

  const fitTerminal = useCallback(() => {
    requestAnimationFrame(() => {
      const term = xtermRef.current
      const fit = fitAddonRef.current
      if (!term || !fit) return

      fit.fit()
      void nativeApi.terminal().resize(terminalId, term.cols, term.rows)
    })
  }, [terminalId])

  useEffect(() => {
    const host = terminalRef.current
    if (!host) return

    let disposed = false
    let offData: (() => void) | null = null
    let offExit: (() => void) | null = null
    const terminal = nativeApi.terminal()
    const term = new XTerm({
      cursorBlink: true,
      cursorStyle: "bar",
      fontSize: 13,
      fontFamily: '"JetBrains Mono", "Cascadia Code", Consolas, monospace',
      lineHeight: 1.2,
      letterSpacing: 0,
      theme: getTerminalTheme(),
      scrollback: 6000,
      convertEol: false,
    })
    const fit = new FitAddon()

    xtermRef.current = term
    fitAddonRef.current = fit
    term.loadAddon(fit)
    term.loadAddon(new WebLinksAddon())
    term.open(host)
    fit.fit()

    const boot = async () => {
      try {
        await terminal.spawn({
          id: terminalId,
          shell,
          cwd: projectPath || undefined,
          cols: Math.max(80, term.cols || 80),
          rows: Math.max(10, term.rows || 10),
          env: {
            TERM: "xterm-256color",
            COLORTERM: "truecolor",
            TERM_PROGRAM: "shob",
          },
        })

        if (disposed) return

        offData = terminal.onData(terminalId, (data) => {
          term.write(data)
        })
        offExit = terminal.onExit(terminalId, () => {
          term.writeln("\r\n\x1b[2mProcess exited.\x1b[0m")
        })
        term.onData((data) => {
          void terminal.write(terminalId, data)
        })
        term.onBinary((data) => {
          void terminal.write(terminalId, data)
        })
        fitTerminal()
        term.focus()
      } catch (error) {
        term.writeln(`\x1b[31mFailed to start terminal: ${String(error)}\x1b[0m`)
      }
    }

    const resizeObserver = new ResizeObserver(fitTerminal)
    resizeObserver.observe(host)
    window.addEventListener("resize", fitTerminal)
    void boot()

    return () => {
      disposed = true
      resizeObserver.disconnect()
      window.removeEventListener("resize", fitTerminal)
      offData?.()
      offExit?.()
      void terminal.kill(terminalId)
      term.dispose()
      xtermRef.current = null
      fitAddonRef.current = null
    }
  }, [fitTerminal, projectPath, shell, terminalId])

  return (
    <section
      className="relative flex shrink-0 flex-col border-t border-border bg-[#0c0c0c]"
      style={{ height }}
    >
      <div
        role="separator"
        aria-orientation="horizontal"
        aria-label="Resize terminal panel"
        className="absolute -top-1 left-0 z-20 h-2 w-full cursor-row-resize bg-transparent"
        onPointerDown={onResizeStart}
      >
        <span className="mx-auto mt-[3px] block h-px w-full bg-border/80" />
      </div>

      <div className="flex h-9 shrink-0 items-center border-b border-border bg-[#111111] px-3">
        <div className="flex h-7 items-center gap-2 rounded-md bg-muted/60 px-2 text-xs font-medium text-foreground">
          <SquareTerminal className="h-3.5 w-3.5" />
          <span className="max-w-[180px] truncate">{projectPath ?? title}</span>
        </div>

        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          className="ml-2 h-7 w-7 text-muted-foreground hover:text-foreground"
          title="New terminal"
          onClick={() => {
            setTerminalId(`bottom-terminal-${crypto.randomUUID()}`)
          }}
        >
          <Plus className="h-3.5 w-3.5" />
        </Button>

        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          className="ml-auto h-7 w-7 text-muted-foreground hover:text-foreground"
          title="Close terminal panel"
          onClick={onClose}
        >
          <X className="h-3.5 w-3.5" />
        </Button>
      </div>

      <div ref={terminalRef} className="min-h-0 flex-1 overflow-hidden" />
    </section>
  )
}
