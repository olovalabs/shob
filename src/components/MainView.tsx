import { lazy, Suspense, useEffect, useMemo, useState } from "react"
import { FolderTree, Maximize2, MoreHorizontal, PanelRight, PanelTop, Plus } from "lucide-react"
import { nativeApi } from "../services/native"
import { Sidebar } from "./Sidebar"
import { Terminal } from "./Terminal"
import { WelcomeScreen } from "./WelcomeScreen"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"

const FileTree = lazy(async () => {
  const module = await import("./FileTree")
  return { default: module.FileTree }
})

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

type RightPanelMode = "review" | "file-tree" | "section"

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
  const [activeFilePath, setActiveFilePath] = useState<string | null>(null)
  const [isRightPanelVisible, setIsRightPanelVisible] = useState(true)
  const [rightPanelMode, setRightPanelMode] = useState<RightPanelMode>("file-tree")
  const [isAddPanelMenuOpen, setIsAddPanelMenuOpen] = useState(false)
  const [bootedSessionIds, setBootedSessionIds] = useState<Set<string>>(new Set())
  const projectSessions = useMemo(() => currentProject?.sessions ?? [], [currentProject])
  const allSessions = useMemo(() => projects.flatMap((p) => p.sessions), [projects])
  const activeSession = useMemo(
    () => allSessions.find((session) => session.id === activeSessionId) ?? null,
    [activeSessionId, allSessions],
  )
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
    const handleFileTreeToggleRequest = () => {
      setIsRightPanelVisible((current) => {
        if (current && rightPanelMode === "file-tree") return false
        return true
      })
      setRightPanelMode("file-tree")
    }

    window.addEventListener("gg-toggle-file-tree", handleFileTreeToggleRequest)
    return () => window.removeEventListener("gg-toggle-file-tree", handleFileTreeToggleRequest)
  }, [rightPanelMode])

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

  const handleToggleFileTree = () => {
    if (isRightPanelVisible && rightPanelMode === "file-tree") {
      setIsRightPanelVisible(false)
      return
    }

    setRightPanelMode("file-tree")
    setIsRightPanelVisible(true)
  }

  const handleShowReview = () => {
    setRightPanelMode("review")
    setIsRightPanelVisible(true)
  }

  const handleAddSection = () => {
    setRightPanelMode("section")
    setIsRightPanelVisible(true)
    setIsAddPanelMenuOpen(false)
  }

  const centerTitle = activeSession?.name || currentProject?.name || "Welcome"
  const centerSubtitle = currentProject?.path ?? "Open a project to start working"

  return (
    <div className="flex min-h-0 flex-1 bg-[#181818] text-foreground">
      <Sidebar />

      <div className="flex min-w-0 flex-1 overflow-hidden bg-[#151515]">
        <section
          className={`flex min-w-0 flex-col overflow-hidden rounded-tl-[14px] border border-b-0 border-r-0 border-white/[0.08] bg-[#101010] ${
            isRightPanelVisible ? "flex-[3_1_0%]" : "flex-1"
          }`}
        >
          <header className="flex h-12 shrink-0 items-center justify-between border-b border-white/[0.07] px-4">
            <div className="min-w-0">
              <div className="flex min-w-0 items-center gap-2">
                <p className="truncate text-[13px] font-semibold text-white">{centerTitle}</p>
                <Button type="button" variant="ghost" size="icon-xs" className="h-6 w-6 text-white/45 hover:text-white">
                  <MoreHorizontal className="h-3.5 w-3.5" />
                </Button>
              </div>
              <p className="mt-0.5 truncate text-[11px] text-white/45">{centerSubtitle}</p>
            </div>

            <div className="flex items-center gap-1 text-white/45">
              <Button type="button" variant="ghost" size="icon-xs" className="h-7 w-7 hover:text-white" title="Run">
                <PanelTop className="h-3.5 w-3.5" />
              </Button>
              <Button type="button" variant="ghost" size="icon-xs" className="h-7 w-7 hover:text-white" title="Expand">
                <Maximize2 className="h-3.5 w-3.5" />
              </Button>
            </div>
          </header>

          <div className="relative min-h-0 min-w-0 flex-1 overflow-hidden bg-[#111111]">
            <div className="relative h-full w-full min-h-0 min-w-0 overflow-hidden" style={{ display: projectSessions.length > 0 ? "block" : "none" }}>
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
          </div>
        </section>

        {isRightPanelVisible && (
          <aside className="flex min-w-[320px] flex-[1_1_0%] flex-col overflow-hidden border-l border-white/[0.08] bg-[#111111]">
            <header className="flex h-12 shrink-0 items-center justify-between border-b border-white/[0.07] px-3">
              <div className="relative flex min-w-0 items-center gap-1">
                <button
                  type="button"
                  onClick={handleShowReview}
                  className={`flex h-7 shrink-0 items-center gap-1.5 rounded-[8px] px-2.5 text-[12px] font-medium transition-colors ${
                    rightPanelMode === "review"
                      ? "bg-white/[0.08] text-white"
                      : "text-white/58 hover:bg-white/[0.05] hover:text-white"
                  }`}
                >
                  <PanelTop className="h-3.5 w-3.5 shrink-0" strokeWidth={1.8} />
                  <span>Review</span>
                </button>

                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  className="h-7 w-7 shrink-0 text-white/45 hover:bg-white/[0.05] hover:text-white"
                  onClick={() => setIsAddPanelMenuOpen((current) => !current)}
                  title="Open panel"
                >
                  <Plus className="h-4 w-4" />
                </Button>

                {isAddPanelMenuOpen && (
                  <div className="absolute left-0 top-9 z-30 w-[220px] rounded-[8px] border border-white/[0.08] bg-[#1d1d1d] p-1.5 shadow-2xl">
                    <button
                      type="button"
                      onClick={handleAddSection}
                      className="flex w-full flex-col items-start rounded-[6px] px-2.5 py-2 text-left hover:bg-white/[0.06]"
                    >
                      <span className="text-[12px] font-medium text-white">New Section</span>
                      <span className="mt-0.5 text-[11px] text-white/45">Open an empty right-side panel</span>
                    </button>
                  </div>
                )}
              </div>

              <div className="ml-2 flex items-center gap-1">
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  className={`h-7 w-7 ${
                    rightPanelMode === "file-tree"
                      ? "bg-white/[0.08] text-white"
                      : "text-white/45 hover:bg-white/[0.05] hover:text-white"
                  }`}
                  onClick={handleToggleFileTree}
                  title="File tree"
                  aria-pressed={rightPanelMode === "file-tree"}
                >
                  <FolderTree className="h-3.5 w-3.5" />
                </Button>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  className="h-7 w-7 text-white/45 hover:bg-white/[0.05] hover:text-white"
                  onClick={() => setIsRightPanelVisible(false)}
                  title="Hide right panel"
                >
                  <PanelRight className="h-3.5 w-3.5" />
                </Button>
              </div>
            </header>

            <div className="min-h-0 flex-1 overflow-hidden">
              {rightPanelMode === "file-tree" && (
                <Suspense fallback={null}>
                  <FileTree selectedFilePath={activeFilePath} onFileSelect={handleFileSelect} />
                </Suspense>
              )}

              {rightPanelMode === "review" && (
                <div className="flex h-full items-center justify-center px-6 text-center">
                  <div>
                    <p className="text-[13px] font-semibold text-white">No unstaged changes</p>
                    <p className="mt-2 text-[13px] text-white/45">Code changes will appear here</p>
                  </div>
                </div>
              )}

              {rightPanelMode === "section" && (
                <div className="flex h-full items-center justify-center px-6 text-center">
                  <div>
                    <p className="text-[13px] font-semibold text-white">New section</p>
                    <p className="mt-2 text-[13px] text-white/45">This right-side panel is ready for your next tool.</p>
                  </div>
                </div>
              )}
            </div>
          </aside>
        )}
      </div>
    </div>
  )
}
