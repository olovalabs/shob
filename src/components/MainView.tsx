import { createEffect, createMemo, createSignal, lazy, Suspense } from 'solid-js'
import { nativeApi } from '../services/native'
import { Sidebar } from './Sidebar'
import { TabBar } from './TabBar'
import { Terminal } from './Terminal'
import { WelcomeScreen } from './WelcomeScreen'
import { useStore } from '../store'

const FileTree = lazy(async () => import('./FileTree'))

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

export function MainView() {
  const projects = useStore((s) => s.projects)
  const currentProject = useStore((s) =>
    s.projects.find((project) => project.id === s.currentProjectId) ?? null,
  )
  const activeSessionId = useStore((s) => s.activeSessionId)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const addProject = useStore((s) => s.addProject)
  const setCurrentProject = useStore((s) => s.setCurrentProject)
  const launchCliSession = useStore((s) => s.launchCliSession)
  const [activeFilePath, setActiveFilePath] = createSignal<string | null>(null)
  const [isFileTreeVisible, setIsFileTreeVisible] = createSignal(false)
  const [bootedSessionIds, setBootedSessionIds] = createSignal<Set<string>>(new Set())
  const projectSessions = createMemo(() => currentProject()?.sessions ?? [])
  const allSessions = createMemo(() => projects().flatMap((p) => p.sessions))

  createEffect(() => {
    currentProjectId()
    setActiveFilePath(null)
  })

  createEffect(() => {
    const sid = activeSessionId()
    if (!sid) return
    setBootedSessionIds((current) => {
      if (current.has(sid)) return current
      const next = new Set(current)
      next.add(sid)
      return next
    })
  })

  createEffect(() => {
    const visible = isFileTreeVisible()
    window.dispatchEvent(
      new CustomEvent('gg-file-tree-state', {
        detail: { isFileTreeVisible: visible },
      }),
    )
  })

  createEffect(() => {
    const handleFileTreeToggleRequest = () => {
      setIsFileTreeVisible((current) => !current)
    }

    window.addEventListener('gg-toggle-file-tree', handleFileTreeToggleRequest)
    return () => window.removeEventListener('gg-toggle-file-tree', handleFileTreeToggleRequest)
  })

  const handleFileSelect = (filePath: string | null) => {
    setActiveFilePath(filePath)
  }

  const handleOpenFolder = async () => {
    const selected = await nativeApi.open({
      directory: true,
      multiple: false,
      title: 'Select Project Folder',
    })

    if (typeof selected !== 'string' || !selected) return

    const existing = projects().find((project) => project.path === selected)
    if (existing) {
      setCurrentProject(existing.id)
      return
    }

    const created = await addProject(folderNameFromPath(selected), selected)
    setCurrentProject(created.id)
  }

  const handleCreateSession = async () => {
    const cpid = currentProjectId()
    if (!cpid) return
    await launchCliSession(cpid)
  }

  const handleToggleFileTree = () => {
    if (!currentProject()) return
    setIsFileTreeVisible((current) => !current)
  }

  return (
    <div class="flex min-h-0 flex-1 bg-background text-foreground">
      <Sidebar />
      <div class="min-w-0 flex-1 flex flex-col bg-background">
        <TabBar />
        <div class="min-h-0 min-w-0 flex-1 overflow-hidden bg-background">
          <div class="relative h-full w-full min-h-0 min-w-0 overflow-hidden" style={{ display: projectSessions().length > 0 ? 'block' : 'none' }}>
            {(() => {
              const sessions = allSessions()
              const booted = bootedSessionIds()
              return sessions.map((session) => {
                const shouldBoot = booted.has(session.id)
                if (!shouldBoot) return null

                return (
                  <Terminal
                    sessionId={session.id}
                    isActive={session.id === activeSessionId()}
                    shouldBoot={shouldBoot}
                  />
                )
              })
            })()}
          </div>

          {projectSessions().length === 0 && (
            <WelcomeScreen
              projects={projects()}
              currentProject={currentProject()}
              onOpenFolder={handleOpenFolder}
              onCreateSession={handleCreateSession}
              onSelectProject={setCurrentProject}
              onToggleFileTree={handleToggleFileTree}
            />
          )}
        </div>
      </div>
      {(() => {
        if (!isFileTreeVisible()) return null
        return (
          <Suspense fallback={null}>
            <FileTree selectedFilePath={activeFilePath()} onFileSelect={handleFileSelect} />
          </Suspense>
        )
      })()}
    </div>
  )
}
