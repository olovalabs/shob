import { lazy, Suspense, useEffect, useMemo, useState } from "react"
import { open } from "@tauri-apps/plugin-dialog"
import { Sidebar } from "./Sidebar"
import { TabBar } from "./TabBar"
import { Terminal } from "./Terminal"
import { WelcomeScreen } from "./WelcomeScreen"
import { useStore } from "../store"

const FileTree = lazy(async () => {
  const module = await import("./FileTree")
  return { default: module.FileTree }
})

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
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
  const [bootedSessionIds, setBootedSessionIds] = useState<Set<string>>(new Set())
  const projectSessions = useMemo(() => currentProject?.sessions ?? [], [currentProject])
  const allSessions = useMemo(() => projects.flatMap((p) => p.sessions), [projects])

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
    const selected = await open({
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
  
  return (
    <div className="flex min-h-0 flex-1 bg-background text-foreground">
      <Sidebar />
      <div className="min-w-0 flex-1 flex flex-col bg-background">
        <TabBar />
        <div className="min-h-0 min-w-0 flex-1 overflow-hidden bg-background">
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
      </div>
      {isFileTreeVisible && (
        <Suspense fallback={null}>
          <FileTree selectedFilePath={activeFilePath} onFileSelect={handleFileSelect} />
        </Suspense>
      )}
    </div>
  )
}
