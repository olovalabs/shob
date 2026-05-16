import { createEffect, createMemo, createSignal, lazy, Show, Suspense } from 'solid-js'
import { nativeApi } from '../services/native'
import { Sidebar } from './Sidebar'
import { TabBar } from './TabBar'
import { Terminal } from './Terminal'
import { WelcomeScreen } from './WelcomeScreen'
import { useStore } from '../store'

const FileTree = lazy(async () => {
  const mod = await import('./FileTree')
  return { default: mod.FileTree }
})

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

export function MainView() {
  const store = useStore()
  const projects = useStore((s) => s.projects)
  const currentProject = useStore((s) =>
    s.projects.find((project) => project.id === s.currentProjectId) ?? null,
  )
  const activeSessionId = useStore((s) => s.activeSessionId)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const [activeFilePath, setActiveFilePath] = createSignal<string | null>(null)
  const [isFileTreeVisible, setIsFileTreeVisible] = createSignal(false)
  const projectSessions = createMemo(() => currentProject()?.sessions ?? [])
  const activeProjectSessionId = createMemo(() => {
    const sessionId = activeSessionId()
    if (!sessionId) return null
    return projectSessions().some((session) => session.id === sessionId) ? sessionId : null
  })

  createEffect(() => {
    currentProjectId()
    setActiveFilePath(null)
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
      store.setCurrentProject(existing.id)
      return
    }

    const created = await store.addProject(folderNameFromPath(selected), selected)
    store.setCurrentProject(created.id)
  }

  const handleCreateSession = async () => {
    const cpid = currentProjectId() ?? projects()[0]?.id ?? null
    if (!cpid) return
    if (currentProjectId() !== cpid) {
      store.setCurrentProject(cpid)
    }
    await store.launchCliSession(cpid)
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
            <Show when={activeProjectSessionId()} keyed>
              {(sessionId) => (
                <Terminal
                  sessionId={sessionId}
                  isActive
                />
              )}
            </Show>
          </div>

          {projectSessions().length === 0 && (
            <WelcomeScreen
              projects={projects()}
              currentProject={currentProject()}
              onOpenFolder={handleOpenFolder}
              onCreateSession={handleCreateSession}
              onSelectProject={store.setCurrentProject}
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
