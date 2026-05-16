import { createEffect, createSignal, For, onCleanup, onMount, Show } from "solid-js"
import { Folder, MoreHorizontal, Plus, Settings, SquarePen } from "lucide-solid"
import { nativeApi } from "../services/native"
import { useStore } from "../store"
import type { Project } from "../types"

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

const formatSessionAge = (createdAt?: number | null) => {
  if (!createdAt || !Number.isFinite(createdAt)) return ""
  const mins = Math.floor((Date.now() - createdAt) / 60000)
  if (mins < 1) return "now"
  if (mins < 60) return `${mins}m`
  const h = Math.floor(mins / 60)
  if (h < 24) return `${h}h`
  return `${Math.floor(h / 24)}d`
}

function FolderSection(props: {
  project: Project
  activeSessionId: string | null
  onSelectProject: (id: string) => void
  onSelectSession: (projectId: string, sessionId: string) => void
  onCreateSession: (projectId: string) => void
  onOpenWorkspacePage?: () => void
}) {
  const [isOpen, setIsOpen] = createSignal(true)

  return (
    <div class="flex flex-col">
      <div
        class="group mx-2 flex cursor-pointer items-center justify-between rounded-md px-3 py-1.5 hover:bg-[#2a2d2e]"
        onClick={() => {
          props.onOpenWorkspacePage?.()
          props.onSelectProject(props.project.id)
          setIsOpen(!isOpen())
        }}
      >
        <div class="flex items-center gap-2.5 text-[#cccccc]">
          <Folder size={15} class="stroke-[1.5]" />
          <span class="text-[13px] leading-none font-medium">{props.project.name}</span>
        </div>

        <div class="flex items-center gap-1 opacity-0 transition-opacity duration-150 group-hover:opacity-100">
          <button
            class="rounded p-1 text-[#cccccc] transition-colors hover:bg-[#3c3c3c]"
            onClick={(e) => e.stopPropagation()}
          >
            <MoreHorizontal size={14} />
          </button>
          <div class="relative flex items-center">
            <button
              class="peer rounded p-1 text-[#cccccc] transition-colors hover:bg-[#3c3c3c]"
              onClick={(e) => {
                e.stopPropagation()
                void props.onCreateSession(props.project.id)
              }}
            >
              <SquarePen size={14} />
            </button>

            <div class="pointer-events-none absolute top-full right-0 z-50 mt-1.5 whitespace-nowrap rounded border border-[#454545] bg-[#252526] px-3 py-1.5 text-[12px] text-[#cccccc] opacity-0 shadow-xl transition-opacity peer-hover:opacity-100">
              Start new chat in {props.project.name}
            </div>
          </div>
        </div>
      </div>

      <Show when={isOpen()}>
        <div class="mt-0.5 flex flex-col">
          <Show
            when={props.project.sessions.length > 0}
            fallback={<div class="py-[5px] pr-4 pl-[38px] text-[13px] text-[#858585]">No sessions</div>}
          >
            <For each={props.project.sessions}>
              {(session) => (
                <div
                  class={`group flex cursor-pointer items-center justify-between py-[5px] pr-4 pl-[38px] hover:bg-[#2a2d2e] ${
                    props.activeSessionId === session.id ? "bg-[#2a2d2e]" : ""
                  }`}
                  onClick={() => props.onSelectSession(props.project.id, session.id)}
                >
                  <span class={`truncate text-[13px] ${props.activeSessionId === session.id ? "text-[#cccccc]" : "text-[#b8b8b8]"}`}>
                    {session.name}
                  </span>

                  <span class="ml-3 shrink-0 rounded bg-[#2d2d2d] px-1.5 py-[1px] text-[11px] font-medium text-[#858585] transition-colors group-hover:bg-[#3c3c3c]">
                    {formatSessionAge(session.lastActiveAt ?? session.createdAt)}
                  </span>
                </div>
              )}
            </For>
          </Show>
        </div>
      </Show>
    </div>
  )
}

export function Sidebar(props: {
  onOpenSettingsPage?: () => void
  onOpenWorkspacePage?: () => void
}) {
  const projects = useStore((s) => s.projects)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const activeSessionId = useStore((s) => s.activeSessionId)
  const setCurrentProject = useStore((s) => s.setCurrentProject)
  const setActiveSession = useStore((s) => s.setActiveSession)
  const addProject = useStore((s) => s.addProject)
  const launchCliSession = useStore((s) => s.launchCliSession)
  const [isSidebarVisible, setIsSidebarVisible] = createSignal(true)

  createEffect(() => {
    window.dispatchEvent(
      new CustomEvent("gg-sidebar-state", {
        detail: { isSidebarVisible: isSidebarVisible() },
      }),
    )
  })

  onMount(() => {
    const handleSidebarToggleRequest = () => {
      setIsSidebarVisible((current) => !current)
    }

    window.addEventListener("gg-toggle-sidebar", handleSidebarToggleRequest)
    onCleanup(() => window.removeEventListener("gg-toggle-sidebar", handleSidebarToggleRequest))
  })

  createEffect(() => {
    if (!currentProjectId() && projects().length > 0) {
      setCurrentProject(projects()[0].id)
    }
  })

  const handleAddProject = async () => {
    props.onOpenWorkspacePage?.()
    const selected = await nativeApi.open({
      directory: true,
      multiple: false,
      title: "Select Project Folder",
    })
    if (typeof selected !== "string" || !selected) return
    const existing = projects().find((project) => project.path === selected)
    if (existing) {
      setCurrentProject(existing.id)
      return
    }
    const created = await addProject(folderNameFromPath(selected), selected)
    setCurrentProject(created.id)
  }

  const handleCreateSession = async (projectId: string) => {
    props.onOpenWorkspacePage?.()
    setCurrentProject(projectId)
    await launchCliSession(projectId)
  }

  const handleSelectSession = (projectId: string, sessionId: string) => {
    props.onOpenWorkspacePage?.()
    setCurrentProject(projectId)
    setActiveSession(sessionId)
  }

  return (
    <aside
      class={`relative h-full shrink-0 transition-all duration-200 ${
        isSidebarVisible() ? "w-[320px] border-r border-[#2d2d2d]" : "w-0 border-r-0"
      }`}
    >
      <div class="relative flex h-full max-h-full flex-col bg-[#1e1e1e] select-none">
      <div class="sticky top-0 z-10 flex items-center justify-between bg-[#1e1e1e] px-3 pt-4 pb-2">
        <div class="px-2 text-[13px] font-medium text-[#858585]">Projects</div>
        <div class="flex items-center gap-0.5 text-[#858585]">
          <button
            class="flex items-center justify-center rounded-md p-1.5 transition-colors hover:bg-[#3c3c3c] hover:text-[#cccccc]"
            title="New Project"
            onClick={() => void handleAddProject()}
          >
            <Plus size={16} />
          </button>
        </div>
      </div>

      <div class="custom-scrollbar mt-2 flex-1 overflow-y-auto">
        <div class="mb-2 px-5 text-[11px] font-semibold tracking-wider text-[#858585] uppercase">Projects</div>
        <div class="flex flex-col gap-0.5 pb-3">
          <For each={projects()}>
            {(project) => (
              <FolderSection
                project={project}
                activeSessionId={activeSessionId()}
                onSelectProject={setCurrentProject}
                onSelectSession={handleSelectSession}
                onCreateSession={handleCreateSession}
                onOpenWorkspacePage={props.onOpenWorkspacePage}
              />
            )}
          </For>
        </div>
      </div>

      <div class="border-t border-[#2d2d2d] p-2">
        <button
          type="button"
          class="flex h-8 w-full items-center gap-2 rounded-md px-3 text-left text-[13px] text-[#e4e4e4] transition-colors hover:bg-[#2a2d2e]"
          title="Settings"
          onClick={() => {
            props.onOpenSettingsPage?.()
          }}
        >
          <Settings size={15} />
          <span class="text-[13px] leading-none">Settings</span>
        </button>
      </div>
      </div>
    </aside>
  )
}
