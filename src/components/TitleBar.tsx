import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { getCurrentWindow } from "@tauri-apps/api/window"
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import { platform } from "@tauri-apps/plugin-os"
import { ChevronDown, Folder, GitBranch, Play } from "lucide-react"
import { CliAvatar } from "./CliAvatar"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"

type OsPlatform = "windows" | "chromeos" | "macos" | "gnome" | "mobile"

interface GitBranchInfo {
  repoName?: string | null
  head: string
}

interface ProjectFsEvent {
  projectPath: string
  paths: string[]
}

function mapTauriPlatform(value: string): OsPlatform {
  switch (value) {
    case "macos":
      return "macos"
    case "linux":
      return "gnome"
    case "android":
    case "ios":
      return "mobile"
    case "windows":
    default:
      return "windows"
  }
}

function TitlebarButton({
  className,
  onClick,
  children,
  label,
}: {
  className: string
  onClick: () => void | Promise<void>
  children: React.ReactNode
  label: string
}) {
  return (
    <button
      type="button"
      onClick={(event) => {
        event.stopPropagation()
        void onClick()
      }}
      className={`relative z-20 pointer-events-auto ${className}`}
      style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      aria-label={label}
    >
      {children}
    </button>
  )
}

async function runWindowAction(action: string, operation: () => Promise<void>) {
  try {
    await operation()
  } catch (error) {
    console.error(`Title bar action failed: ${action}`, error)
  }
}

function currentWindow() {
  return getCurrentWindow()
}

function WindowsGlyph({
  glyph,
  className = "",
}: {
  glyph: string
  className?: string
}) {
  return (
    <span
      className={className}
      style={{
        fontFamily: '"Segoe Fluent Icons", "Segoe MDL2 Assets", "Segoe UI Symbol", sans-serif',
        fontSize: "10px",
        lineHeight: 1,
        fontWeight: 400,
      }}
      aria-hidden="true"
    >
      {glyph}
    </span>
  )
}

function WindowsControls({
  isMaximized,
  setIsMaximized,
}: {
  isMaximized: boolean
  setIsMaximized: React.Dispatch<React.SetStateAction<boolean>>
}) {
  const baseButtonClass = "bg-transparent text-foreground/90 hover:bg-white/[0.06] active:bg-white/[0.04]"
  const closeButtonClass = "bg-transparent text-foreground/90 hover:bg-[#c42b1c] hover:text-white active:bg-[#a1261b]"

  return (
    <div className="flex h-8" style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}>
      <TitlebarButton
        label="Minimize"
        onClick={() => runWindowAction("minimize", () => currentWindow().minimize())}
        className={`inline-flex h-8 w-[46px] items-center justify-center transition-colors ${baseButtonClass}`}
      >
        <WindowsGlyph glyph={"\uE921"} />
      </TitlebarButton>
      <TitlebarButton
        label={isMaximized ? "Restore" : "Maximize"}
        onClick={async () => {
          await runWindowAction("toggle maximize", async () => {
            const window = currentWindow()
            await window.toggleMaximize()
            const next = await window.isMaximized()
            setIsMaximized(next)
          })
        }}
        className={`inline-flex h-8 w-[46px] items-center justify-center transition-colors ${baseButtonClass}`}
      >
        <WindowsGlyph glyph={isMaximized ? "\uE923" : "\uE922"} />
      </TitlebarButton>
      <TitlebarButton
        label="Close"
        onClick={() => runWindowAction("close", () => currentWindow().close())}
        className={`inline-flex h-8 w-[46px] items-center justify-center transition-colors ${closeButtonClass}`}
      >
        <WindowsGlyph glyph={"\uE8BB"} />
      </TitlebarButton>
    </div>
  )
}

function MacControls() {
  return (
    <div className="flex items-center gap-2 px-3" style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}>
      <TitlebarButton
        label="Close"
        onClick={() => runWindowAction("close", () => currentWindow().close())}
        className="flex h-3 w-3 items-center justify-center rounded-full border border-black/[0.12] bg-[#ff544d] text-black/60"
      >
        <svg width="6" height="6" viewBox="0 0 16 18" fill="none" xmlns="http://www.w3.org/2000/svg" className="opacity-0 hover:opacity-100">
          <path d="M15.7522 4.44381L11.1543 9.04165L15.7494 13.6368C16.0898 13.9771 16.078 14.5407 15.724 14.8947L13.8907 16.728C13.5358 17.0829 12.9731 17.0938 12.6328 16.7534L8.03766 12.1583L3.44437 16.7507C3.10402 17.091 2.54132 17.0801 2.18645 16.7253L0.273257 14.8121C-0.0807018 14.4572 -0.0925004 13.8945 0.247845 13.5542L4.84024 8.96087L0.32499 4.44653C-0.0153555 4.10619 -0.00355681 3.54258 0.350402 3.18862L2.18373 1.35529C2.53859 1.00042 3.1013 0.989533 3.44164 1.32988L7.95689 5.84422L12.5556 1.24638C12.8951 0.906035 13.4587 0.917833 13.8126 1.27179L15.7267 3.18589C16.0807 3.53985 16.0925 4.10346 15.7522 4.44381Z" fill="currentColor" />
        </svg>
      </TitlebarButton>
      <TitlebarButton
        label="Minimize"
        onClick={() => runWindowAction("minimize", () => currentWindow().minimize())}
        className="flex h-3 w-3 items-center justify-center rounded-full border border-black/[0.12] bg-[#ffbd2e] text-black/60"
      >
        <svg width="8" height="8" viewBox="0 0 17 6" fill="none" xmlns="http://www.w3.org/2000/svg" className="opacity-0 hover:opacity-100">
          <path fillRule="evenodd" clipRule="evenodd" d="M1.47211 1.18042H15.4197C15.8052 1.18042 16.1179 1.50551 16.1179 1.90769V3.73242C16.1179 4.13387 15.8052 4.80006 15.4197 4.80006H1.47211C1.08665 4.80006 0.773926 4.47497 0.773926 4.07278V1.90769C0.773926 1.50551 1.08665 1.18042 1.47211 1.18042Z" fill="currentColor" />
        </svg>
      </TitlebarButton>
      <TitlebarButton
        label="Maximize"
        onClick={async () => {
          await runWindowAction("toggle fullscreen", async () => {
            const window = currentWindow()
            const fullscreen = await window.isFullscreen()
            await window.setFullscreen(!fullscreen)
          })
        }}
        className="flex h-3 w-3 items-center justify-center rounded-full border border-black/[0.12] bg-[#28c93f] text-black/60"
      >
        <svg width="8" height="8" viewBox="0 0 17 16" fill="none" xmlns="http://www.w3.org/2000/svg" className="opacity-0 hover:opacity-100">
          <path fillRule="evenodd" clipRule="evenodd" d="M15.5308 9.80147H10.3199V15.0095C10.3199 15.3949 9.9941 15.7076 9.59265 15.7076H7.51555C7.11337 15.7076 6.78828 15.3949 6.78828 15.0095V9.80147H1.58319C1.19774 9.80147 0.88501 9.47638 0.88501 9.07419V6.90619C0.88501 6.50401 1.19774 6.17892 1.58319 6.17892H6.78828V1.06183C6.78828 0.676375 7.11337 0.363647 7.51555 0.363647H9.59265C9.9941 0.363647 10.3199 0.676375 10.3199 1.06183V6.17892H15.5308C15.9163 6.17892 16.229 6.50401 16.229 6.90619V9.07419C16.229 9.47638 15.9163 9.80147 15.5308 9.80147Z" fill="currentColor" />
        </svg>
      </TitlebarButton>
    </div>
  )
}

function GnomeControls({
  isMaximized,
  setIsMaximized,
}: {
  isMaximized: boolean
  setIsMaximized: React.Dispatch<React.SetStateAction<boolean>>
}) {
  return (
    <div className="mr-[10px] flex items-center space-x-[13px]" style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}>
      <TitlebarButton
        label="Minimize"
        onClick={() => runWindowAction("minimize", () => currentWindow().minimize())}
        className="flex h-6 w-6 items-center justify-center rounded-full bg-[#373737] p-0 text-white hover:bg-[#424242] active:bg-[#565656]"
      >
        <svg width="10" height="1" viewBox="0 0 10 1" fill="none" xmlns="http://www.w3.org/2000/svg" className="h-[9px] w-[9px]">
          <path d="M0 0.5h10" stroke="currentColor" strokeWidth="1" />
        </svg>
      </TitlebarButton>
      <TitlebarButton
        label={isMaximized ? "Restore" : "Maximize"}
        onClick={async () => {
          await runWindowAction("toggle maximize", async () => {
            const window = currentWindow()
            await window.toggleMaximize()
            const next = await window.isMaximized()
            setIsMaximized(next)
          })
        }}
        className="flex h-6 w-6 items-center justify-center rounded-full bg-[#373737] p-0 text-white hover:bg-[#424242] active:bg-[#565656]"
      >
        {isMaximized ? (
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none" xmlns="http://www.w3.org/2000/svg" className="h-[9px] w-[9px]">
            <path d="M3 1h6v6H8V2H3V1ZM1 3h6v6H1V3Z" fill="currentColor" />
          </svg>
        ) : (
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none" xmlns="http://www.w3.org/2000/svg" className="h-2 w-2">
            <rect x="1" y="1" width="8" height="8" stroke="currentColor" />
          </svg>
        )}
      </TitlebarButton>
      <TitlebarButton
        label="Close"
        onClick={() => runWindowAction("close", () => currentWindow().close())}
        className="flex h-6 w-6 items-center justify-center rounded-full bg-[#373737] p-0 text-white hover:bg-[#424242] active:bg-[#565656]"
      >
        <svg width="10" height="10" viewBox="0 0 10 10" fill="none" xmlns="http://www.w3.org/2000/svg" className="h-2 w-2">
          <path d="M1 1l8 8M9 1 1 9" stroke="currentColor" strokeWidth="1.2" />
        </svg>
      </TitlebarButton>
    </div>
  )
}

export function TitleBar() {
  const [osPlatform, setOsPlatform] = useState<OsPlatform>("windows")
  const [isMaximized, setIsMaximized] = useState(false)
  const {
    projects,
    currentProjectId,
    activeSessionId,
    cliTools,
    cliLaunchMode,
    launchCliSession,
    getCurrentCliTool,
    setPreferredCliTool,
    updateSession,
  } = useStore()
  const [isLauncherOpen, setIsLauncherOpen] = useState(false)
  const [isSidebarVisible, setIsSidebarVisible] = useState(true)
  const [isFileTreeVisible, setIsFileTreeVisible] = useState(false)
  const [branchInfo, setBranchInfo] = useState<GitBranchInfo | null>(null)
  const launcherRef = useRef<HTMLDivElement>(null)
  const branchRefreshTimeoutRef = useRef<number | null>(null)
  const currentProject = useMemo(
    () => projects.find((project) => project.id === currentProjectId) ?? null,
    [projects, currentProjectId],
  )
  const currentCliTool = getCurrentCliTool()
  const installedCliTools = useMemo(() => cliTools.filter((tool) => tool.installed), [cliTools])

  const loadBranchInfo = useCallback(async (projectPath?: string | null) => {
    if (!projectPath) {
      setBranchInfo(null)
      return
    }

    try {
      const branch = await invoke<GitBranchInfo>("get_git_branch", {
        path: projectPath,
      })
      setBranchInfo(branch)
    } catch {
      setBranchInfo(null)
    }
  }, [])

  const scheduleBranchRefresh = useCallback((projectPath?: string | null, delay = 180) => {
    if (!projectPath) {
      setBranchInfo(null)
      return
    }

    if (branchRefreshTimeoutRef.current) {
      window.clearTimeout(branchRefreshTimeoutRef.current)
    }

    branchRefreshTimeoutRef.current = window.setTimeout(() => {
      void loadBranchInfo(projectPath)
    }, delay)
  }, [loadBranchInfo])

  useEffect(() => {
    let mounted = true
    const windowHandle = currentWindow()
    let resizeSyncTimer: number | null = null

    const sync = async () => {
      try {
        const currentPlatform = await platform()
        if (mounted) {
          setOsPlatform(mapTauriPlatform(currentPlatform))
        }

        const maximized = await windowHandle.isMaximized()
        if (mounted) {
          setIsMaximized(maximized)
        }
      } catch (error) {
        console.error("Failed to sync title bar platform:", error)
      }
    }

    void sync()

    const unlistenPromise = windowHandle.onResized(() => {
      if (resizeSyncTimer !== null) {
        window.clearTimeout(resizeSyncTimer)
      }

      resizeSyncTimer = window.setTimeout(async () => {
        const maximized = await windowHandle.isMaximized().catch(() => false)
        if (mounted) {
          setIsMaximized(maximized)
        }
      }, 120)
    })

    return () => {
      mounted = false
      if (resizeSyncTimer !== null) {
        window.clearTimeout(resizeSyncTimer)
      }
      unlistenPromise.then((unlisten) => unlisten())
    }
  }, [])

  useEffect(() => {
    const handleSidebarState = (event: Event) => {
      const detail = (event as CustomEvent<{ isSidebarVisible: boolean }>).detail
      if (!detail) return
      setIsSidebarVisible(Boolean(detail.isSidebarVisible))
    }

    window.addEventListener("gg-sidebar-state", handleSidebarState as EventListener)
    return () => window.removeEventListener("gg-sidebar-state", handleSidebarState as EventListener)
  }, [])

  useEffect(() => {
    const handleFileTreeState = (event: Event) => {
      const detail = (event as CustomEvent<{ isFileTreeVisible: boolean }>).detail
      if (!detail) return
      setIsFileTreeVisible(Boolean(detail.isFileTreeVisible))
    }

    window.addEventListener("gg-file-tree-state", handleFileTreeState as EventListener)
    return () => window.removeEventListener("gg-file-tree-state", handleFileTreeState as EventListener)
  }, [])

  useEffect(() => {
    void loadBranchInfo(currentProject?.path)
  }, [currentProject?.path, loadBranchInfo])

  useEffect(() => {
    if (!currentProject?.path) return

    const unlistenProjectPromise = listen<ProjectFsEvent>("project-fs-event", (event) => {
      if (event.payload.projectPath !== currentProject.path) return

      const touchesGitState = event.payload.paths.some((path) => {
        const normalized = path.replace(/\\/g, "/").toLowerCase()
        return normalized.includes("/.git/") || normalized.endsWith("/.git") || normalized.endsWith("/head")
      })

      if (!touchesGitState) return

      scheduleBranchRefresh(currentProject.path, 140)
    })

    return () => {
      if (branchRefreshTimeoutRef.current) {
        window.clearTimeout(branchRefreshTimeoutRef.current)
        branchRefreshTimeoutRef.current = null
      }
      unlistenProjectPromise.then((unlisten) => unlisten())
    }
  }, [currentProject?.path, scheduleBranchRefresh])

  const handleLaunchSession = async (cliId?: string | null) => {
    if (!currentProjectId) return

    const selectedCli = installedCliTools.find((tool) => tool.id === cliId) ?? null

    if (cliId) {
      setPreferredCliTool(cliId)
    }

    if (
      cliLaunchMode === "replace-current" &&
      activeSessionId
    ) {
      const launchCommand = selectedCli?.matchedCommand ?? cliId
      if (!launchCommand) {
        setIsLauncherOpen(false)
        return
      }

      window.dispatchEvent(
        new CustomEvent("gg-rerun-cli-current-session", {
          detail: {
            sessionId: activeSessionId,
            command: launchCommand,
          },
        }),
      )

      if (selectedCli) {
        await updateSession(currentProjectId, activeSessionId, { cliTool: selectedCli.id })
      }

      setIsLauncherOpen(false)
      return
    }

    await launchCliSession(currentProjectId, cliId)
    setIsLauncherOpen(false)
  }

  const handleToggleSidebar = () => {
    window.dispatchEvent(new Event("gg-toggle-sidebar"))
  }

  const handleToggleFileTree = () => {
    window.dispatchEvent(new Event("gg-toggle-file-tree"))
  }

  const headerClass = "border-white/[0.06] bg-[#121212] text-white"
  const navButtonClass = "text-[#6f6f6f] hover:bg-white/[0.05] hover:text-white"

  return (
    <header className={`relative z-50 flex h-[40px] shrink-0 select-none items-center border-b ${headerClass}`}>
      {osPlatform === "macos" && <MacControls />}

      <div className="flex min-w-0 items-center gap-3 px-3" style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}>
        <div className="flex items-center gap-2">
          <Button
            type="button"
            onClick={handleToggleSidebar}
            variant="ghost"
            size="icon-xs"
            className={`h-6 w-6 rounded-md ${navButtonClass}`}
            aria-label="Sidebar toggle"
            aria-pressed={isSidebarVisible}
            title={isSidebarVisible ? "Hide sidebar" : "Show sidebar"}
          >
            <span className="flex h-3.5 w-3.5 items-center justify-center rounded-[3px] border border-current">
              <span className="h-full w-[1px] bg-current opacity-80" />
            </span>
          </Button>

          {branchInfo?.head && (
            <div className="ml-1 hidden items-center gap-2 rounded-md bg-white/[0.03] px-2 py-1 text-xs text-current/65 lg:flex">
              <GitBranch className="h-3.5 w-3.5" strokeWidth={1.9} />
              <span className="max-w-[170px] truncate">{branchInfo.head}</span>
            </div>
          )}
        </div>
      </div>

      <div
        data-tauri-drag-region
        className="min-w-0 flex-1 self-stretch px-3 text-center text-[12px] font-medium text-current/60"
        style={{ WebkitAppRegion: "drag" } as React.CSSProperties}
      />

      <div className="flex items-center gap-2 px-2" style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}>
        <div ref={launcherRef} className="relative flex items-center">
          <DropdownMenu open={isLauncherOpen} onOpenChange={setIsLauncherOpen}>
            <DropdownMenuTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 min-w-[108px] items-center justify-between gap-2 rounded-md border border-white/10 bg-white/[0.03] px-2.5 text-xs font-medium leading-none text-current/80 hover:bg-white/[0.06]"
                title={currentCliTool ? `Current CLI: ${currentCliTool.label}` : "Choose CLI launcher"}
              >
                {currentCliTool ? (
                  <>
                    <CliAvatar cliId={currentCliTool.id} label={currentCliTool.label} size="sm" />
                    <span className="hidden sm:inline">{currentCliTool.label}</span>
                  </>
                ) : (
                  <Play className="h-3.5 w-3.5 fill-current" strokeWidth={1.8} />
                )}
                <ChevronDown className={`h-3.5 w-3.5 transition ${isLauncherOpen ? "rotate-180" : ""}`} strokeWidth={1.9} />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-[208px]">
              {installedCliTools.length > 0 ? (
                installedCliTools.map((tool) => (
                  <DropdownMenuItem
                    key={tool.id}
                    onClick={() => void handleLaunchSession(tool.id)}
                    className="gap-2.5 px-3 py-2"
                  >
                    <span className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-muted/50">
                      <CliAvatar cliId={tool.id} label={tool.label} size="sm" className="rounded-md" />
                    </span>
                    <span className="min-w-0 flex-1 truncate text-[13px] font-medium">{tool.label}</span>
                  </DropdownMenuItem>
                ))
              ) : (
                <div className="px-3 py-2 text-[13px] text-muted-foreground">No available CLI found</div>
              )}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>

        <Button
          type="button"
          onClick={handleToggleFileTree}
          variant="ghost"
          size="icon-sm"
          className={`h-7 w-7 ${
            isFileTreeVisible
              ? "bg-[#1e2a1f] text-[#9bdd9f]"
              : "text-current/65 hover:bg-white/[0.05] hover:text-current"
          }`}
          title={isFileTreeVisible ? "Hide file tree" : "Show file tree"}
          aria-pressed={isFileTreeVisible}
        >
          <Folder className="h-4 w-4" strokeWidth={1.9} />
        </Button>
      </div>

      {(osPlatform === "windows" || osPlatform === "chromeos") && (
        <WindowsControls
          isMaximized={isMaximized}
          setIsMaximized={setIsMaximized}
        />
      )}
      {osPlatform === "gnome" && (
        <GnomeControls isMaximized={isMaximized} setIsMaximized={setIsMaximized} />
      )}
    </header>
  )
}
