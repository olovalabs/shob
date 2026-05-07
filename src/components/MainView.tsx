import { lazy, Suspense, useEffect, useMemo, useState } from "react"
import { FolderTree, Plus, X } from "lucide-react"
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

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

type RightPanelTabType = "file-tree" | "section"

interface RightPanelTab {
  id: string
  title: string
  type: RightPanelTabType
}

const FILE_TREE_TAB_ID = "file-tree"

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
  const [isFileTreeVisible, setIsFileTreeVisible] = useState(false)
  const [rightPanelTabs, setRightPanelTabs] = useState<RightPanelTab[]>([
    { id: FILE_TREE_TAB_ID, title: "File Tree", type: "file-tree" },
  ])
  const [activeRightPanelTabId, setActiveRightPanelTabId] = useState(FILE_TREE_TAB_ID)
  const [bootedSessionIds, setBootedSessionIds] = useState<Set<string>>(new Set())
  const projectSessions = useMemo(() => currentProject?.sessions ?? [], [currentProject])
  const allSessions = useMemo(() => projects.flatMap((p) => p.sessions), [projects])
  const activeRightPanelTab = useMemo(
    () => rightPanelTabs.find((tab) => tab.id === activeRightPanelTabId) ?? rightPanelTabs[0] ?? null,
    [rightPanelTabs, activeRightPanelTabId],
  )

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
      setIsFileTreeVisible((current) => !current)
    }

    window.addEventListener("gg-toggle-file-tree", handleFileTreeToggleRequest)
    return () => window.removeEventListener("gg-toggle-file-tree", handleFileTreeToggleRequest)
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

  const handleToggleFileTree = () => {
    if (!currentProject) return
    setIsFileTreeVisible((current) => !current)
  }

  const handleAddRightPanelSection = () => {
    const nextCount = rightPanelTabs.filter((tab) => tab.type === "section").length + 1
    const id = `section-${Date.now()}`
    const nextTab: RightPanelTab = {
      id,
      title: `Section ${nextCount}`,
      type: "section",
    }
    setRightPanelTabs((current) => [...current, nextTab])
    setActiveRightPanelTabId(id)
    setIsFileTreeVisible(true)
  }

  const handleActivateRightPanelTab = (tabId: string) => {
    setActiveRightPanelTabId(tabId)
    setIsFileTreeVisible(true)
  }

  const handleCloseRightPanelTab = (tabId: string) => {
    setRightPanelTabs((current) => {
      const next = current.filter((tab) => tab.id !== tabId)
      if (next.length === 0) {
        setIsFileTreeVisible(false)
        return [{ id: FILE_TREE_TAB_ID, title: "File Tree", type: "file-tree" }]
      }

      if (activeRightPanelTabId === tabId) {
        const fallback = next[next.length - 1]
        if (fallback) setActiveRightPanelTabId(fallback.id)
      }

      return next
    })
  }

  useEffect(() => {
    if (!isFileTreeVisible) return
    if (rightPanelTabs.some((tab) => tab.id === FILE_TREE_TAB_ID)) return

    setRightPanelTabs((current) => [{ id: FILE_TREE_TAB_ID, title: "File Tree", type: "file-tree" }, ...current])
    setActiveRightPanelTabId(FILE_TREE_TAB_ID)
  }, [isFileTreeVisible, rightPanelTabs])
  
  return (
    <div className="flex min-h-0 flex-1 bg-background text-foreground">
      <Sidebar />
      <div className="min-w-0 flex-1 flex flex-col bg-background">
        <TabBar />
        <div className="min-h-0 min-w-0 flex-1 overflow-hidden bg-background">
          <div className="flex h-full min-h-0 min-w-0">
            <div className="min-h-0 min-w-0 flex-1 overflow-hidden">
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

            {isFileTreeVisible && (
              <aside className="flex h-full w-[360px] flex-col border-l border-border bg-background">
                <div className="flex items-center gap-1 border-b border-border px-2 py-1.5">
                  <div className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto thin-scrollbar">
                    {rightPanelTabs.map((tab) => {
                      const isActive = tab.id === activeRightPanelTab?.id
                      const showClose = tab.type !== "file-tree"

                      return (
                        <button
                          key={tab.id}
                          type="button"
                          onClick={() => handleActivateRightPanelTab(tab.id)}
                          className={`group flex h-7 shrink-0 items-center gap-1 rounded-md border px-2 text-[12px] font-medium transition-colors ${
                            isActive
                              ? "border-border bg-accent/75 text-accent-foreground"
                              : "border-transparent text-foreground/70 hover:bg-muted/45 hover:text-foreground"
                          }`}
                        >
                          {tab.type === "file-tree" && <FolderTree className="h-3.5 w-3.5 shrink-0" />}
                          <span className="truncate">{tab.title}</span>
                          {showClose && (
                            <span
                              role="button"
                              tabIndex={0}
                              onClick={(e) => {
                                e.stopPropagation()
                                handleCloseRightPanelTab(tab.id)
                              }}
                              onKeyDown={(e) => {
                                if (e.key !== "Enter" && e.key !== " ") return
                                e.preventDefault()
                                e.stopPropagation()
                                handleCloseRightPanelTab(tab.id)
                              }}
                              className="inline-flex h-4 w-4 items-center justify-center rounded-sm text-foreground/55 hover:bg-muted hover:text-foreground"
                            >
                              <X className="h-3 w-3" />
                            </span>
                          )}
                        </button>
                      )
                    })}
                  </div>

                  <Button
                    type="button"
                    variant="ghost"
                    size="icon-xs"
                    className="h-6 w-6 shrink-0"
                    onClick={handleAddRightPanelSection}
                    title="New section"
                  >
                    <Plus className="h-3.5 w-3.5" />
                  </Button>
                </div>

                <div className="min-h-0 flex-1 overflow-hidden">
                  {activeRightPanelTab?.type === "file-tree" ? (
                    <Suspense fallback={null}>
                      <FileTree selectedFilePath={activeFilePath} onFileSelect={handleFileSelect} />
                    </Suspense>
                  ) : (
                    <div className="flex h-full items-center justify-center px-6 text-center">
                      <div>
                        <p className="text-sm font-medium text-foreground">Empty section</p>
                        <p className="mt-1 text-sm text-muted-foreground">You can use this split pane for additional tools next.</p>
                      </div>
                    </div>
                  )}
                </div>
              </aside>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
