import { FolderOpen, PanelLeftOpen, Plus, SquareTerminal } from "lucide-react"
import { useMemo } from "react"
import type { Project } from "../types"

interface WelcomeScreenProps {
  projects: Project[]
  currentProject: Project | null
  onOpenFolder: () => Promise<void> | void
  onCreateSession: () => Promise<void> | void
  onSelectProject: (projectId: string) => void
  onToggleFileTree: () => void
}

const formatProjectActivity = (project: Project) => {
  const latestActivity = Math.max(
    ...project.sessions.map((session) => session.lastActiveAt ?? session.createdAt ?? 0),
    0,
  )

  if (latestActivity <= 0) return "New"

  const diffMinutes = Math.floor(Math.max(0, Date.now() - latestActivity) / 60000)
  if (diffMinutes < 1) return "Now"
  if (diffMinutes < 60) return `${diffMinutes}m`

  const diffHours = Math.floor(diffMinutes / 60)
  if (diffHours < 24) return `${diffHours}h`

  const diffDays = Math.floor(diffHours / 24)
  return `${diffDays}d`
}

const projectLabel = (project: Project) => project.name.trim() || "Untitled"

export function WelcomeScreen({
  projects,
  currentProject,
  onOpenFolder,
  onCreateSession,
  onSelectProject,
  onToggleFileTree,
}: WelcomeScreenProps) {
  const recentProjects = useMemo(() => {
    return [...projects].sort((left, right) => {
      const leftActivity = Math.max(...left.sessions.map((session) => session.lastActiveAt ?? session.createdAt ?? 0), 0)
      const rightActivity = Math.max(...right.sessions.map((session) => session.lastActiveAt ?? session.createdAt ?? 0), 0)
      return rightActivity - leftActivity
    })
  }, [projects])

  return (
    <div className="h-full overflow-y-auto bg-[#050505] text-white">
      <div className="mx-auto flex min-h-full w-full max-w-5xl items-start px-8 py-14 sm:px-12 lg:px-16">
        <div className="grid w-full gap-12 lg:grid-cols-[minmax(0,300px)_minmax(0,1fr)]">
          <section className="space-y-10">
            <div className="space-y-2">
              <h1 className="text-3xl font-light uppercase tracking-[0.18em] text-white sm:text-4xl">Terminal Workspace</h1>
            </div>

            <div className="space-y-4">
              <h2 className="text-sm font-medium uppercase tracking-[0.24em] text-zinc-500">Start</h2>
              <div className="space-y-2">
                <button
                  type="button"
                  onClick={() => void onOpenFolder()}
                  className="flex items-center gap-3 text-left text-[16px] font-light text-zinc-200 transition hover:text-white"
                >
                  <FolderOpen className="h-4 w-4 shrink-0" strokeWidth={1.7} />
                  <span>Open Folder...</span>
                </button>
                <button
                  type="button"
                  onClick={() => void onCreateSession()}
                  disabled={!currentProject}
                  className="flex items-center gap-3 text-left text-[16px] font-light text-zinc-200 transition hover:text-white disabled:cursor-not-allowed disabled:text-zinc-600"
                >
                  <Plus className="h-4 w-4 shrink-0" strokeWidth={1.7} />
                  <span>New Session...</span>
                </button>
                <button
                  type="button"
                  onClick={onToggleFileTree}
                  disabled={!currentProject}
                  className="flex items-center gap-3 text-left text-[16px] font-light text-zinc-200 transition hover:text-white disabled:cursor-not-allowed disabled:text-zinc-600"
                >
                  <PanelLeftOpen className="h-4 w-4 shrink-0" strokeWidth={1.7} />
                  <span>Open Explorer...</span>
                </button>
              </div>
            </div>
          </section>

          <section className="space-y-4">
            <div className="flex items-center gap-3">
              <SquareTerminal className="h-4 w-4 text-zinc-500" strokeWidth={1.7} />
              <h2 className="text-sm font-medium uppercase tracking-[0.24em] text-zinc-500">Recent</h2>
            </div>
            {recentProjects.length === 0 ? (
              <p className="text-sm text-zinc-500">No recent folders yet. Open a project folder to get started.</p>
            ) : (
              <div className="space-y-1">
                {recentProjects.map((project) => {
                  const isCurrent = project.id === currentProject?.id

                  return (
                    <button
                      key={project.id}
                      type="button"
                      onClick={() => onSelectProject(project.id)}
                      className={`grid w-full grid-cols-[auto_minmax(0,180px)_minmax(0,1fr)_auto] items-center gap-4 rounded-md px-3 py-2.5 text-left transition ${
                        isCurrent ? "text-white" : "text-zinc-300 hover:bg-white/[0.03] hover:text-white"
                      }`}
                    >
                      <span className={`h-2 w-2 rounded-full ${isCurrent ? "bg-zinc-100" : "bg-transparent"}`} />
                      <span className="truncate text-[15px] font-normal tracking-[0.01em]">{projectLabel(project)}</span>
                      <span className={`truncate font-mono text-[12px] ${isCurrent ? "text-zinc-400" : "text-zinc-600"}`}>
                        {project.path}
                      </span>
                      <span className={`shrink-0 text-[11px] uppercase tracking-[0.18em] ${isCurrent ? "text-zinc-300" : "text-zinc-600"}`}>
                        {formatProjectActivity(project)}
                      </span>
                    </button>
                  )
                })}
              </div>
            )}
          </section>
        </div>
      </div>
    </div>
  )
}
