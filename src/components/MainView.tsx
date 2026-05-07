import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from "react"
import { FolderTree, PanelRight, PanelTop, Plus } from "lucide-react"
import { nativeApi } from "../services/native"
import { Sidebar } from "./Sidebar"
import { TabBar } from "./TabBar"
import { Terminal } from "./Terminal"
import { WelcomeScreen } from "./WelcomeScreen"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"

const FileTree = lazy(async () => {
  const module = await import("./FileTree")
  return { default: module.FileTree }
})

const RIGHT_PANEL_MIN_WIDTH = 320
const RIGHT_PANEL_DEFAULT_WIDTH = 520
const RIGHT_PANEL_MAX_WIDTH = 760
const MAIN_CONTENT_MIN_WIDTH = 520

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

const clampRightPanelWidth = (width: number, workspaceWidth?: number) => {
  const dynamicMax = workspaceWidth
    ? Math.max(RIGHT_PANEL_MIN_WIDTH, Math.min(RIGHT_PANEL_MAX_WIDTH, workspaceWidth - MAIN_CONTENT_MIN_WIDTH))
    : RIGHT_PANEL_MAX_WIDTH

  return Math.round(Math.min(Math.max(width, RIGHT_PANEL_MIN_WIDTH), dynamicMax))
}

const getInitialRightPanelWidth = () => {
  if (typeof window === "undefined") return RIGHT_PANEL_DEFAULT_WIDTH
  return clampRightPanelWidth(Math.round(window.innerWidth * 0.34), window.innerWidth)
}

type RightPanelMode = "review" | "file-tree"

function ReviewEmptyState() {
  return (
    <div className="flex h-full w-full items-center justify-center px-6">
      <div className="text-center">
        <p className="text-sm font-semibold text-foreground">No unstaged changes</p>
        <p className="mt-2 text-sm text-muted-foreground">Code changes will appear here</p>
      </div>
    </div>
  )
}

export function MainView() {
  const projects = useStore((state) => state.projects)
  const currentProject = useStore((state) =>
    state.projects.find((project) => project.id === state.currentProjectId) ?? null,
  )
  const activeSessionId = useStore((state) => state.activeSessionId)
  const currentProjectId = useStore((state) => state.currentProjectId)
  const addProject = useStore((state) => state.addProject)
  const setCurrentProject = useStore((state) => state.setCurrentProject)
  const launchCliSession = useStore((state) => state.launchCliSession)
  const workspaceRef = useRef<HTMLDivElement>(null)
  const [activeFilePath, setActiveFilePath] = useState<string | null>(null)
  const [rightPanelMode, setRightPanelMode] = useState<RightPanelMode>("file-tree")
  const [isRightPanelVisible, setIsRightPanelVisible] = useState(true)
  const [rightPanelWidth, setRightPanelWidth] = useState(getInitialRightPanelWidth)
  const [isAddPanelMenuOpen, setIsAddPanelMenuOpen] = useState(false)
  const [bootedSessionIds, setBootedSessionIds] = useState<Set<string>>(new Set())
  const projectSessions = useMemo(() => currentProject?.sessions ?? [], [currentProject])
  const allSessions = useMemo(() => projects.flatMap((p) => p.sessions), [projects])
  const isFileTreeVisible = isRightPanelVisible && rightPanelMode === "file-tree"

  useEffect(() => {
    setActiveFilePath(null)
  }, [currentProjectId])

  useEffect(() => {
    if (!activeSessionId) return
    setBootedSessionIds((current) => {
      if (current.has(activeSessionId)) return current
      const next = new Set(current)
      next.add(activeSessionId)
      return next
    })
  }, [activeSessionId])

  useEffect(() => {
    window.dispatchEvent(
      new CustomEvent("gg-file-tree-state", {
        detail: {
          isFileTreeVisible,
        },
      }),
    )
  }, [isFileTreeVisible])

  useEffect(() => {
    const handleResize = () => {
      const workspaceWidth = workspaceRef.current?.getBoundingClientRect().width
      setRightPanelWidth((current) => clampRightPanelWidth(current, workspaceWidth))
    }

    window.addEventListener("resize", handleResize)
    return () => window.removeEventListener("resize", handleResize)
  }, [])

  const showFileTree = useCallback(() => {
    setRightPanelMode("file-tree")
    setIsRightPanelVisible(true)
    setIsAddPanelMenuOpen(false)
  }, [])

  const showReviewPanel = useCallback(() => {
    setRightPanelMode("review")
    setIsRightPanelVisible(true)
    setIsAddPanelMenuOpen(false)
  }, [])

  const handleToggleFileTree = useCallback(() => {
    if (!isRightPanelVisible || rightPanelMode !== "file-tree") {
      showFileTree()
      return
    }

    showReviewPanel()
  }, [isRightPanelVisible, rightPanelMode, showFileTree, showReviewPanel])

  useEffect(() => {
    window.addEventListener("gg-toggle-file-tree", handleToggleFileTree)
    return () => window.removeEventListener("gg-toggle-file-tree", handleToggleFileTree)
  }, [handleToggleFileTree])

  const handleRightPanelResizeStart = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)

    const workspaceRect = workspaceRef.current?.getBoundingClientRect()
    if (!workspaceRect) return

    const previousCursor = document.body.style.cursor
    const previousUserSelect = document.body.style.userSelect
    document.body.style.cursor = "col-resize"
    document.body.style.userSelect = "none"

    const handlePointerMove = (moveEvent: PointerEvent) => {
      const nextWidth = workspaceRect.right - moveEvent.clientX
      setRightPanelWidth(clampRightPanelWidth(nextWidth, workspaceRect.width))
    }

    const cleanup = () => {
      window.removeEventListener("pointermove", handlePointerMove)
      window.removeEventListener("pointerup", cleanup)
      window.removeEventListener("pointercancel", cleanup)
      document.body.style.cursor = previousCursor
      document.body.style.userSelect = previousUserSelect
    }

    window.addEventListener("pointermove", handlePointerMove)
    window.addEventListener("pointerup", cleanup)
    window.addEventListener("pointercancel", cleanup)
  }, [])

  const handleFileSelect = (filePath: string | null) => {
    setActiveFilePath(filePath)
  }

  const handleOpenFolder = async () => {
    const selected = await nativeApi.open({
      directory: true,
      multiple: false,
      title: "Select Project Folder",
    })

    if (typeof selected !== "string" || !selected) return

    const existing = projects.find((project) => project.path === selected)
    if (existing) {
      setCurrentProject(existing.id)
      return
    }

    const created = await addProject(folderNameFromPath(selected), selected)
    setCurrentProject(created.id)
  }

  const handleCreateSession = async () => {
    if (!currentProjectId) return
    await launchCliSession(currentProjectId)
  }

  return (
    <div className="flex min-h-0 flex-1 bg-background text-foreground">
      <Sidebar />
      <div className="flex min-w-0 flex-1 flex-col bg-background">
        <TabBar />
        <div ref={workspaceRef} className="flex min-h-0 min-w-0 flex-1 overflow-hidden bg-background">
          <main className="min-h-0 min-w-0 flex-1 overflow-hidden">
            <div
              className="relative h-full min-h-0 w-full min-w-0 overflow-hidden"
              style={{ display: projectSessions.length > 0 ? "block" : "none" }}
            >
              {allSessions.map((session) => {
                const shouldBoot = bootedSessionIds.has(session.id)
                if (!shouldBoot) return null

                return (
                  <Terminal
                    key={session.id}
                    sessionId={session.id}
                    isActive={session.id === activeSessionId}
                    shouldBoot={shouldBoot}
                  />
                )
              })}
            </div>

            {projectSessions.length === 0 && (
              <WelcomeScreen
                projects={projects}
                currentProject={currentProject}
                onOpenFolder={handleOpenFolder}
                onCreateSession={handleCreateSession}
                onSelectProject={setCurrentProject}
                onToggleFileTree={handleToggleFileTree}
              />
            )}
          </main>

          {isRightPanelVisible && (
            <>
              <div
                className="workspace-divider h-full w-[5px] shrink-0"
                role="separator"
                aria-orientation="vertical"
                aria-label="Resize review panel"
                onPointerDown={handleRightPanelResizeStart}
              />

              <aside
                className="flex h-full shrink-0 flex-col border-l border-border bg-background"
                style={{ width: rightPanelWidth }}
              >
                <div className="flex h-10 shrink-0 items-center border-b border-border px-3">
                  <div className="relative flex min-w-0 items-center gap-1">
                    <button
                      type="button"
                      onClick={showReviewPanel}
                      className={`flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px] font-medium transition-colors ${
                        rightPanelMode === "review"
                          ? "bg-accent/75 text-foreground"
                          : "text-foreground/70 hover:bg-muted/45 hover:text-foreground"
                      }`}
                    >
                      <PanelTop className="h-3.5 w-3.5 shrink-0" />
                      <span>Review</span>
                    </button>

                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-xs"
                      className="h-7 w-7 text-foreground/55 hover:text-foreground"
                      onClick={() => setIsAddPanelMenuOpen((current) => !current)}
                      title="Open in panel"
                    >
                      <Plus className="h-3.5 w-3.5" />
                    </Button>

                    {isAddPanelMenuOpen && (
                      <div className="absolute left-0 top-8 z-20 w-[190px] rounded-md border border-border bg-popover p-1 shadow-lg">
                        <button
                          type="button"
                          onClick={showFileTree}
                          className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[12px] text-popover-foreground hover:bg-accent"
                        >
                          <FolderTree className="h-3.5 w-3.5" />
                          <span>File Tree</span>
                        </button>
                      </div>
                    )}
                  </div>

                  <div className="ml-auto flex items-center gap-1">
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-xs"
                      className={`h-7 w-7 ${
                        rightPanelMode === "file-tree"
                          ? "bg-accent/75 text-foreground"
                          : "text-foreground/60 hover:text-foreground"
                      }`}
                      onClick={handleToggleFileTree}
                      title={isFileTreeVisible ? "Show review" : "Show file tree"}
                      aria-pressed={isFileTreeVisible}
                    >
                      <FolderTree className="h-3.5 w-3.5" />
                    </Button>

                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-xs"
                      className="h-7 w-7 text-foreground/60 hover:text-foreground"
                      onClick={() => setIsRightPanelVisible(false)}
                      title="Hide review panel"
                    >
                      <PanelRight className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                </div>

                <div className="min-h-0 flex-1 overflow-hidden">
                  {rightPanelMode === "review" ? (
                    <ReviewEmptyState />
                  ) : (
                    <Suspense fallback={null}>
                      <FileTree selectedFilePath={activeFilePath} onFileSelect={handleFileSelect} />
                    </Suspense>
                  )}
                </div>
              </aside>
            </>
          )}
        </div>
      </div>
    </div>
  )
}
