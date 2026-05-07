import { lazy, Suspense, useEffect, useMemo, useState } from "react"
import { FolderTree, Globe, PanelTop, Plus, X } from "lucide-react"
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

type WorkspaceTabType = "review" | "browser" | "section"

interface WorkspaceTab {
  id: string
  title: string
  type: WorkspaceTabType
}
interface AddTabOption {
  type: Exclude<WorkspaceTabType, "review">
  title: string
  description: string
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
  const [activeFilePath, setActiveFilePath] = useState<string | null>(null)
  const [isFileTreeVisible, setIsFileTreeVisible] = useState(false)
  const [workspaceTabs, setWorkspaceTabs] = useState<WorkspaceTab[]>([
    { id: "review", title: "Review", type: "review" },
  ])
  const [activeWorkspaceTabId, setActiveWorkspaceTabId] = useState("review")
  const [isAddTabMenuOpen, setIsAddTabMenuOpen] = useState(false)
  const [bootedSessionIds, setBootedSessionIds] = useState<Set<string>>(new Set())
  const projectSessions = useMemo(() => currentProject?.sessions ?? [], [currentProject])
  const allSessions = useMemo(() => projects.flatMap((p) => p.sessions), [projects])
  const activeWorkspaceTab = useMemo(
    () => workspaceTabs.find((tab) => tab.id === activeWorkspaceTabId) ?? workspaceTabs[0] ?? null,
    [workspaceTabs, activeWorkspaceTabId],
  )
  const addTabOptions = useMemo<AddTabOption[]>(
    () => [
      { type: "browser", title: "Browser", description: "Open a web panel in this workspace" },
      { type: "section", title: "New Section", description: "Create an empty split content panel" },
    ],
    [],
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

  const handleActivateWorkspaceTab = (tabId: string) => {
    setActiveWorkspaceTabId(tabId)
  }

  const handleCloseWorkspaceTab = (tabId: string) => {
    setWorkspaceTabs((current) => {
      const next = current.filter((tab) => tab.id !== tabId)
      if (next.length === 0) return [{ id: "review", title: "Review", type: "review" }]

      if (activeWorkspaceTabId === tabId) {
        const fallback = next[next.length - 1]
        if (fallback) setActiveWorkspaceTabId(fallback.id)
      }

      return next
    })
  }

  const handleAddWorkspaceTab = (type: AddTabOption["type"]) => {
    const nextCount = workspaceTabs.filter((tab) => tab.type === type).length + 1
    const id = `${type}-${Date.now()}`
    const nextTab: WorkspaceTab = {
      id,
      title: type === "browser" ? (nextCount === 1 ? "Browser" : `Browser ${nextCount}`) : `Section ${nextCount}`,
      type,
    }
    setWorkspaceTabs((current) => [...current, nextTab])
    setActiveWorkspaceTabId(id)
    setIsAddTabMenuOpen(false)
  }
  
  return (
    <div className="flex min-h-0 flex-1 bg-background text-foreground">
      <Sidebar />
      <div className="min-w-0 flex-1 flex flex-col bg-background">
        <TabBar />
        <div className="min-h-0 min-w-0 flex-1 overflow-hidden bg-background">
          <div className="flex h-10 items-center justify-between border-b border-border px-3">
            <div className="relative flex min-w-0 flex-1 items-center gap-1 overflow-hidden">
              <div className="flex min-w-0 items-center gap-1 overflow-x-auto thin-scrollbar">
                {workspaceTabs.map((tab) => {
                  const isActive = tab.id === activeWorkspaceTab?.id
                  const showClose = tab.type !== "review"

                  return (
                    <button
                      key={tab.id}
                      type="button"
                      onClick={() => handleActivateWorkspaceTab(tab.id)}
                      className={`group flex h-7 shrink-0 items-center gap-1 rounded-md border px-2 text-[12px] font-medium transition-colors ${
                        isActive
                          ? "border-border bg-accent/75 text-accent-foreground"
                          : "border-transparent text-foreground/70 hover:bg-muted/45 hover:text-foreground"
                      }`}
                    >
                      {tab.type === "review" && <PanelTop className="h-3.5 w-3.5 shrink-0" />}
                      {tab.type === "browser" && <Globe className="h-3.5 w-3.5 shrink-0" />}
                      <span className="truncate">{tab.title}</span>
                      {showClose && (
                        <span
                          role="button"
                          tabIndex={0}
                          onClick={(e) => {
                            e.stopPropagation()
                            handleCloseWorkspaceTab(tab.id)
                          }}
                          onKeyDown={(e) => {
                            if (e.key !== "Enter" && e.key !== " ") return
                            e.preventDefault()
                            e.stopPropagation()
                            handleCloseWorkspaceTab(tab.id)
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
                onClick={() => setIsAddTabMenuOpen((current) => !current)}
                title="Open panel"
              >
                <Plus className="h-3.5 w-3.5" />
              </Button>

              {isAddTabMenuOpen && (
                <div className="absolute left-0 top-8 z-20 w-[260px] rounded-md border border-border bg-background p-1.5 shadow-lg">
                  {addTabOptions.map((option) => (
                    <button
                      key={option.type}
                      type="button"
                      onClick={() => handleAddWorkspaceTab(option.type)}
                      className="flex w-full flex-col items-start gap-0.5 rounded-md px-2 py-2 text-left hover:bg-muted"
                    >
                      <span className="text-xs font-medium text-foreground">{option.title}</span>
                      <span className="text-[11px] text-muted-foreground">{option.description}</span>
                    </button>
                  ))}
                </div>
              )}
            </div>

            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              className="h-7 w-7 shrink-0"
              onClick={handleToggleFileTree}
              title="Toggle file tree"
            >
              <FolderTree className="h-3.5 w-3.5" />
            </Button>
          </div>

          <div className="flex h-full min-h-0 min-w-0">
            <div className="min-h-0 min-w-0 flex-1 overflow-hidden">
              {activeWorkspaceTab?.type === "review" && (
                <>
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
                </>
              )}

              {activeWorkspaceTab?.type === "browser" && (
                <div className="flex h-full w-full items-center justify-center px-6">
                  <div className="w-full max-w-xl rounded-lg border border-border bg-card p-4">
                    <p className="text-sm font-medium text-foreground">Browser panel</p>
                    <p className="mt-1 text-sm text-muted-foreground">
                      This is ready for embedding web content in the tabbed workspace.
                    </p>
                  </div>
                </div>
              )}

              {activeWorkspaceTab?.type === "section" && (
                <div className="flex h-full w-full items-center justify-center px-6">
                  <div className="w-full max-w-xl rounded-lg border border-border bg-card p-4">
                    <p className="text-sm font-medium text-foreground">New section</p>
                    <p className="mt-1 text-sm text-muted-foreground">
                      Add your custom tool content to this panel section.
                    </p>
                  </div>
                </div>
              )}
            </div>

            {isFileTreeVisible && (
              <aside className="flex h-full w-[360px] flex-col border-l border-border bg-background">
                <div className="flex h-10 items-center border-b border-border px-3">
                  <FolderTree className="h-3.5 w-3.5 text-muted-foreground" />
                  <span className="ml-2 text-xs font-medium text-foreground">File Tree</span>
                </div>
                <div className="min-h-0 flex-1 overflow-hidden">
                  <Suspense fallback={null}>
                    <FileTree selectedFilePath={activeFilePath} onFileSelect={handleFileSelect} />
                  </Suspense>
                </div>
              </aside>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
