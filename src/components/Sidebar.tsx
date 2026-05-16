import { createEffect, createMemo, createSignal, onCleanup, onMount, For, Show } from "solid-js"
import { nativeApi } from "../services/native"
import {
  Boxes,
  FolderPlus,
  MoreHorizontal,
  Palette,
  Plus,
  SlidersHorizontal,
  Settings,
  Terminal,
  Upload,
} from "lucide-solid"
import { CliAvatar } from "./CliAvatar"
import { useStore } from "../store"
import type { Project } from "../types"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Spinner } from "@/components/ui/spinner"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar"

interface PtyDataEvent {
  sessionId: string
  data: string
}

const PROJECT_COLOR_OPTIONS = [
  { value: "#4a2567", text: "#d79cff" },
  { value: "#17344f", text: "#8ecbff" },
  { value: "#214132", text: "#97e4b0" },
  { value: "#5b3219", text: "#ffbb7a" },
  { value: "#5f1f37", text: "#ff9bc2" },
  { value: "#3e3a1a", text: "#ffe27d" },
] as const

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

const getShellLabel = (shell: string) => {
  const name = shell.split(/[\\/]/).pop()
  return name || shell
}

const formatSessionLabel = (index: number) => `Session ${index + 1}`
const getProjectBadge = (name: string) => name.trim().charAt(0).toUpperCase() || "P"

const getProjectTheme = (project?: Pick<Project, "color"> | null) => {
  const color = project?.color ?? "#62285d"
  const option = PROJECT_COLOR_OPTIONS.find((item) => item.value === color)

  return {
    bg: color,
    text: option?.text ?? "#ffffff",
  }
}

const formatRelativeSessionTime = (createdAt?: number | null) => {
  if (!createdAt || !Number.isFinite(createdAt)) return ""

  const diffMs = Math.max(0, Date.now() - createdAt)
  const minutes = Math.floor(diffMs / 60000)

  if (minutes < 1) return "now"
  if (minutes < 60) return `${minutes}m`

  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h`

  const days = Math.floor(hours / 24)
  if (days < 7) return `${days}d`

  const weeks = Math.floor(days / 7)
  if (weeks < 5) return `${weeks}w`

  const months = Math.floor(days / 30)
  if (months < 12) return `${months}mo`

  const years = Math.floor(days / 365)
  return `${years}y`
}

const formatCommandCount = (count?: number | null) => {
  if (typeof count !== "number" || !Number.isFinite(count) || count <= 0) return ""
  if (count < 1000) return `${Math.floor(count)} cmd`
  return `${(count / 1000).toFixed(1)}k cmd`
}

type SessionStatusTone = "active" | "running" | "idle"

const getSessionStatus = (options: {
  isActiveSession: boolean
  isRunningSession: boolean
}) => {
  if (options.isActiveSession) {
    return { label: "Active", tone: "active" as SessionStatusTone, title: "Session is active" }
  }

  if (options.isRunningSession) {
    return { label: "Running", tone: "running" as SessionStatusTone, title: "Session is running" }
  }

  return { label: "Idle", tone: "idle" as SessionStatusTone, title: "Session is idle" }
}

const SESSION_STATUS_STYLES: Record<SessionStatusTone, string> = {
  active: "bg-sky-400 shadow-[0_0_0_1px_rgba(2,132,199,0.45)]",
  running: "bg-emerald-400 animate-pulse shadow-[0_0_0_1px_rgba(22,163,74,0.45)]",
  idle: "bg-zinc-500 shadow-[0_0_0_1px_rgba(113,113,122,0.45)]",
}

export function Sidebar() {
  const projects = useStore((s) => s.projects)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const activeSessionId = useStore((s) => s.activeSessionId)
  const preferredCliId = useStore((s) => s.preferredCliId)
  const preferredShell = useStore((s) => s.preferredShell)
  const cliLaunchMode = useStore((s) => s.cliLaunchMode)
  const cliTools = useStore((s) => s.cliTools)
  const availableShells = useStore((s) => s.availableShells)
  const store = useStore()

  const [expandedProjects, setExpandedProjects] = createSignal<Record<string, boolean>>({})
  const [isSessionPaneVisible, setIsSessionPaneVisible] = createSignal(true)
  const [busySessions, setBusySessions] = createSignal<Record<string, boolean>>({})
  const [isSettingsOpen, setIsSettingsOpen] = createSignal(false)
  const [activeSettingsSection, setActiveSettingsSection] = createSignal<"general" | "providers" | "cli-tools">("general")
  const [cliToolSearchQuery, setCliToolSearchQuery] = createSignal("")
  const [editingProjectId, setEditingProjectId] = createSignal<string | null>(null)
  const [draftProjectName, setDraftProjectName] = createSignal("")
  const [draftProjectColor, setDraftProjectColor] = createSignal<string>(PROJECT_COLOR_OPTIONS[0].value)
  const [draftProjectLogoPath, setDraftProjectLogoPath] = createSignal<string | null>(null)
  const [projectLogoUrls, setProjectLogoUrls] = createSignal<Record<string, string>>({})
  const [draftProjectLogoUrl, setDraftProjectLogoUrl] = createSignal<string | null>(null)

  const currentProject = createMemo(
    () => projects().find((project) => project.id === currentProjectId()) ?? null,
  )
  const editingProject = createMemo(
    () => projects().find((project) => project.id === editingProjectId()) ?? null,
  )
  createEffect(() => {
    setExpandedProjects((current) => {
      const next = { ...current }

      for (const project of projects()) {
        if (!(project.id in next)) {
          next[project.id] = true
        }
      }

      for (const projectId of Object.keys(next)) {
        if (!projects().some((project) => project.id === projectId)) {
          delete next[projectId]
        }
      }

      return next
    })
  })

  createEffect(() => {
    window.dispatchEvent(
      new CustomEvent("gg-sidebar-state", {
        detail: {
          isSidebarVisible: isSessionPaneVisible(),
        },
      }),
    )
  })

  onMount(() => {
    const handleSidebarToggleRequest = () => {
      setIsSessionPaneVisible((current) => !current)
    }

    window.addEventListener("gg-toggle-sidebar", handleSidebarToggleRequest)
    onCleanup(() => window.removeEventListener("gg-toggle-sidebar", handleSidebarToggleRequest))
  })

  createEffect(() => {
    const startTimer = window.setTimeout(() => {
      void syncProjectLogos()
    }, 120)

    const syncProjectLogos = async () => {
      const entries = await Promise.all(
        projects().map(async (project) => {
          if (!project.logoPath) {
            return [project.id, ""] as const
          }

          try {
            const dataUrl = await nativeApi.invoke("read_image_data_url", { path: project.logoPath })
            return [project.id, dataUrl] as const
          } catch {
            return [project.id, ""] as const
          }
        }),
      )

      setProjectLogoUrls(Object.fromEntries(entries))
    }

    onCleanup(() => {
      window.clearTimeout(startTimer)
    })
  })

  createEffect(() => {
    const syncDraftLogo = async () => {
      const logoPath = draftProjectLogoPath()
      if (!logoPath) {
        setDraftProjectLogoUrl(null)
        return
      }

      try {
        const dataUrl = await nativeApi.invoke("read_image_data_url", { path: logoPath })
        setDraftProjectLogoUrl(dataUrl)
      } catch {
        setDraftProjectLogoUrl(null)
      }
    }

    void syncDraftLogo()
  })

  onMount(() => {
    const idleTimers = new Map<string, number>()

    const handlePtyData = (event: Event) => {
      const detail = (event as CustomEvent<PtyDataEvent>).detail
      if (!detail) return
      const sessionId = detail.sessionId
      const hasVisiblePayload = detail.data.trim().length > 0
      if (!hasVisiblePayload) return

      setBusySessions((current) => {
        if (current[sessionId]) return current
        return { ...current, [sessionId]: true }
      })

      const existingTimer = idleTimers.get(sessionId)
      if (existingTimer) {
        window.clearTimeout(existingTimer)
      }

      const timer = window.setTimeout(() => {
        idleTimers.delete(sessionId)
        setBusySessions((current) => {
          if (!current[sessionId]) return current

          const next = { ...current }
          delete next[sessionId]
          return next
        })
      }, 1400)

      idleTimers.set(sessionId, timer)
    }

    window.addEventListener("gg-pty-data", handlePtyData as EventListener)

    onCleanup(() => {
      idleTimers.forEach((timer) => window.clearTimeout(timer))
      window.removeEventListener("gg-pty-data", handlePtyData as EventListener)
    })
  })

  const filteredCurrentSessions = createMemo(() => currentProject()?.sessions ?? [])
  const installedCliTools = createMemo(() => cliTools().filter((tool) => tool.installed))
  const filteredCliTools = createMemo(() => {
    const query = cliToolSearchQuery().trim().toLowerCase()
    if (!query) return cliTools()

    return cliTools().filter((tool) => {
      const haystack = [
        tool.label,
        tool.id,
        tool.installCommand,
        tool.matchedCommand ?? "",
        tool.resolvedPath ?? "",
        tool.installed ? "installed" : "not installed",
      ]

      return haystack.some((value) => value.toLowerCase().includes(query))
    })
  })

  const openEditProject = (project: Project) => {
    setEditingProjectId(project.id)
    setDraftProjectName(project.name)
    setDraftProjectColor(project.color ?? PROJECT_COLOR_OPTIONS[0].value)
    setDraftProjectLogoPath(project.logoPath ?? null)
    setDraftProjectLogoUrl(project.logoPath ? projectLogoUrls()[project.id] ?? null : null)
  }

  const handleChooseProjectLogo = async () => {
    const selected = await nativeApi.open({
      multiple: false,
      title: "Choose Project Logo",
      filters: [
        {
          name: "Images",
          extensions: ["png", "jpg", "jpeg", "webp", "svg", "ico"],
        },
      ],
    })

    if (typeof selected === "string" && selected) {
      setDraftProjectLogoPath(selected)
    }
  }

  const handleSaveProjectEdit = async () => {
    const editing = editingProject()
    if (!editing) return

    const nextName = draftProjectName().trim()
    if (!nextName) return

    await store.updateProject(editing.id, {
      name: nextName,
      color: draftProjectColor(),
      logoPath: draftProjectLogoPath(),
    })

    setEditingProjectId(null)
  }

  const handleAddProject = async () => {
    const selected = await nativeApi.open({
      directory: true,
      multiple: false,
      title: "Select Project Folder",
    })

    if (typeof selected !== "string" || !selected) return

    const existing = projects().find((project) => project.path === selected)
    if (existing) {
      store.setCurrentProject(existing.id)
      setExpandedProjects((current) => ({ ...current, [existing.id]: true }))
      return
    }

    const projectName = folderNameFromPath(selected)
    const created = await store.addProject(projectName, selected)
    setExpandedProjects((current) => ({ ...current, [created.id]: true }))
  }

  const handleCreateSession = async (projectId: string) => {
    const fallbackProjectId = projectId || currentProjectId() || projects()[0]?.id
    if (!fallbackProjectId) return
    if (currentProjectId() !== fallbackProjectId) {
      store.setCurrentProject(fallbackProjectId)
    }
    await store.launchCliSession(fallbackProjectId)
    setExpandedProjects((current) => ({ ...current, [fallbackProjectId]: true }))
  }

  const handleDeleteProject = async (projectId: string) => {
    await store.deleteProject(projectId)
    setExpandedProjects((current) => {
      const next = { ...current }
      delete next[projectId]
      return next
    })
    if (editingProjectId() === projectId) {
      setEditingProjectId(null)
    }
  }

  const renderProjectMark = (project: Project, size: "sm" | "md" = "md") => {
    const theme = getProjectTheme(project)
    const badge = getProjectBadge(project.name)
    const logoUrl = projectLogoUrls()[project.id]

    if (project.logoPath && logoUrl) {
      return (
        <Avatar
          class={
            size === "md"
              ? "h-10 w-10 overflow-hidden rounded-[8px] after:rounded-[8px]"
              : "h-6 w-6 overflow-hidden rounded-[6px] after:rounded-[6px]"
          }
        >
          <AvatarImage src={logoUrl} alt={project.name} class="rounded-none object-cover" />
          <AvatarFallback class="rounded-none" style={{ background: theme.bg, color: theme.text }}>
            {badge}
          </AvatarFallback>
        </Avatar>
      )
    }

    return (
      <Avatar
        class={
          size === "md"
            ? "h-10 w-10 overflow-hidden rounded-[8px] after:rounded-[8px]"
            : "h-6 w-6 overflow-hidden rounded-[6px] after:rounded-[6px]"
        }
      >
        <AvatarFallback class="rounded-none" style={{ background: theme.bg, color: theme.text }}>
          {badge}
        </AvatarFallback>
      </Avatar>
    )
  }

  return (
    <>
      <aside class={`relative flex h-full shrink-0 border-r bg-background text-foreground ${isSessionPaneVisible() ? "w-[392px]" : "w-[52px]"}`}>
        <div class="flex w-[52px] flex-col items-center border-r bg-muted/30 px-2 py-3">
          <div class="flex w-full flex-col items-center gap-4">
            <For each={projects()}>
              {(project) => {
                const isActive = () => project.id === currentProjectId()

                return (
                  <div class="group relative">
                    <button
                      type="button"
                      onClick={() => store.setCurrentProject(project.id)}
                      class={`relative rounded-[10px] p-0.5 transition-colors ${isActive() ? "bg-accent/70" : "hover:bg-accent/55"}`}
                    >
                      {renderProjectMark(project, "md")}
                      {isActive() && <span class="pointer-events-none absolute inset-0 rounded-[10px] ring-1 ring-ring/40" />}
                    </button>
                  </div>
                )
              }}
            </For>

            <button
              type="button"
              onClick={handleAddProject}
              class="group relative rounded-[10px] p-0.5 transition-colors hover:bg-accent/55"
              title="Add project"
            >
              <span class="flex h-10 w-10 items-center justify-center rounded-[8px] bg-muted/45 text-muted-foreground transition-colors group-hover:bg-muted/60 group-hover:text-foreground">
                <Plus class="h-4 w-4" stroke-width={2} />
              </span>
            </button>
          </div>

          <div class="mt-auto flex w-full flex-col items-center gap-3">
            <Button
              type="button"
              onClick={() => setIsSettingsOpen((current) => !current)}
              variant="ghost"
              size="icon-sm"
              class="h-8 w-8"
              title="Settings"
              aria-pressed={isSettingsOpen()}
            >
              <Settings class="h-4 w-4" stroke-width={1.9} />
            </Button>
          </div>
        </div>

        <Show when={isSessionPaneVisible()}>
        <div class="flex min-w-0 flex-1 flex-col bg-background">
          <div class="border-b px-4 py-4">
            <div class="flex items-start justify-between gap-3">
              <div class="min-w-0">
                <p class="truncate text-[28px] leading-none font-semibold text-foreground">{currentProject()?.name ?? "Workspace"}</p>
                <p class="mt-1 truncate text-[14px] text-muted-foreground">
                  {currentProject()?.path ? currentProject()!.path.replace(/^([A-Za-z]):\\Users\\[^\\]+/, "~") : "Add a project to get started"}
                </p>
              </div>
              <Show when={currentProject()}>
                <div class="relative">
                  <DropdownMenu>
                    <DropdownMenuTrigger>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-sm"
                        class="h-8 w-8 shrink-0"
                        title="Project options"
                      >
                        <MoreHorizontal class="h-4 w-4" stroke-width={2} />
                      </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent class="w-[136px] rounded-[8px] p-1">
                      <DropdownMenuItem class="px-3 py-1.5 text-[13px]" onClick={() => openEditProject(currentProject()!)}>
                        Edit
                      </DropdownMenuItem>
                      <DropdownMenuItem
                        class="px-3 py-1.5 text-[13px]"
                        variant="destructive"
                        onClick={() => handleDeleteProject(currentProject()!.id)}
                      >
                        Close
                      </DropdownMenuItem>
                    </DropdownMenuContent>
                  </DropdownMenu>
                </div>
              </Show>
            </div>

            <Show when={currentProject()}>
              <div class="mt-5">
                <Button
                  type="button"
                  onClick={() => handleCreateSession(currentProject()!.id)}
                  variant="outline"
                  class="h-10 w-full text-[15px] font-semibold"
                >
                  <FolderPlus class="mr-2 h-4 w-4" stroke-width={1.9} />
                  New session
                </Button>
              </div>
            </Show>
          </div>

          <div class="hide-scrollbar flex-1 overflow-y-auto px-3 py-4">
            <Show when={!currentProject()} fallback={
              <section>
                <Show when={expandedProjects()[currentProject()!.id] ?? true}>
                  <div class="space-y-0.5">
                    <Show when={filteredCurrentSessions().length === 0} fallback={
                      <For each={filteredCurrentSessions()}>
                        {(session, index) => {
                          const isActiveSession = () => activeSessionId() === session.id
                          const isRunningSession = () => Boolean(busySessions()[session.id] || session.pendingLaunchCommand)
                          const sessionStatus = () => getSessionStatus({
                            isActiveSession: isActiveSession(),
                            isRunningSession: isRunningSession(),
                          })
                          const projectAgeLabel = () => formatRelativeSessionTime(session.lastActiveAt ?? session.createdAt)
                          const commandCountLabel = () => formatCommandCount(session.commandCount)

                          return (
                            <div
                              class={`group flex items-center gap-2 rounded-lg px-2 py-1.5 transition ${
                                isActiveSession()
                                  ? "bg-accent text-accent-foreground"
                                  : "text-foreground/84 hover:bg-accent/50"
                              }`}
                            >
                              <button
                                type="button"
                                onClick={() => {
                                  store.setCurrentProject(currentProject()!.id)
                                  store.setActiveSession(session.id)
                                }}
                                class="flex min-w-0 flex-1 items-center gap-2 text-left"
                              >
                                <Show when={isRunningSession()}>
                                  <span class="inline-flex h-4 w-4 shrink-0 items-center justify-center">
                                    <Spinner class="h-3.5 w-3.5 text-[#00BF63]" />
                                  </span>
                                </Show>
                                <span class="min-w-0 flex-1 truncate text-[14px] font-medium text-foreground">
                                  {session.name || formatSessionLabel(index())}
                                </span>
                                <Show when={commandCountLabel()}>
                                  <span class="shrink-0 rounded-sm bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
                                    {commandCountLabel()}
                                  </span>
                                </Show>
                                <Show when={projectAgeLabel()}>
                                  <span class="shrink-0 text-[12px] text-muted-foreground">{projectAgeLabel()}</span>
                                </Show>
                              </button>

                              <div class="flex items-center gap-1">
                                <span class="relative inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-md bg-muted/50">
                                  <CliAvatar cliId={session.cliTool} label={session.name} size="sm" class="opacity-80" />
                                  <span
                                    class={`pointer-events-none absolute -right-0.5 -top-0.5 h-2.5 w-2.5 rounded-full border border-background ${SESSION_STATUS_STYLES[sessionStatus().tone]}`}
                                    title={sessionStatus().title}
                                    aria-label={sessionStatus().label}
                                  />
                                </span>
                                <Button
                                  type="button"
                                  onPointerDown={(event: PointerEvent) => {
                                    event.preventDefault()
                                    event.stopPropagation()
                                  }}
                                  onClick={(event: MouseEvent) => {
                                    event.stopPropagation()
                                    void store.removeSession(currentProject()!.id, session.id)
                                  }}
                                  variant="ghost"
                                  size="icon-xs"
                                  class="h-5 w-5 opacity-0 group-hover:opacity-100 transition-opacity"
                                  title="Close session"
                                >
                                  <span class="text-xs leading-none">&times;</span>
                                </Button>
                              </div>
                            </div>
                          )
                        }}
                      </For>
                    }>
                      <Button
                        type="button"
                        onClick={() => handleCreateSession(currentProject()!.id)}
                        variant="ghost"
                        class="w-full justify-start px-3 py-3 h-auto"
                      >
                        Create your first session
                      </Button>
                    </Show>
                  </div>
                </Show>
              </section>
            }>
              <p class="px-3 py-4 text-sm text-muted-foreground">Select or add a project to view sessions.</p>
            </Show>
          </div>
        </div>
        </Show>

      </aside>

      <Dialog open={isSettingsOpen()} onOpenChange={setIsSettingsOpen}>
        <DialogContent
          class="!h-[640px] !w-[980px] !max-w-none sm:!max-w-none gap-0 overflow-hidden border p-0"
          style={{ width: "980px", height: "640px", "max-width": "none" }}
        >
          <div class="flex h-full flex-row">
            <aside class="w-[190px] border-r bg-card px-3 py-5">
              <div class="thin-scrollbar max-h-full overflow-y-auto pr-1">
                <div class="grid grid-cols-1 gap-1.5">
                  <Button
                    type="button"
                    onClick={() => setActiveSettingsSection("general")}
                    variant={activeSettingsSection() === "general" ? "secondary" : "ghost"}
                    class="justify-start gap-2.5 px-3 py-2 h-auto"
                  >
                    <SlidersHorizontal class="h-4 w-4" stroke-width={1.9} />
                    <span class="font-medium">General</span>
                  </Button>
                  <Button
                    type="button"
                    onClick={() => setActiveSettingsSection("providers")}
                    variant={activeSettingsSection() === "providers" ? "secondary" : "ghost"}
                    class="justify-start gap-2.5 px-3 py-2 h-auto"
                  >
                    <Boxes class="h-4 w-4" stroke-width={1.9} />
                    <span class="font-medium">Providers</span>
                  </Button>
                  <Button
                    type="button"
                    onClick={() => setActiveSettingsSection("cli-tools")}
                    variant={activeSettingsSection() === "cli-tools" ? "secondary" : "ghost"}
                    class="justify-start gap-2.5 px-3 py-2 h-auto"
                  >
                    <Terminal class="h-4 w-4" stroke-width={1.9} />
                    <span class="font-medium">CLI Tools</span>
                  </Button>
                </div>
              </div>
            </aside>

            <div class="relative min-w-0 flex-1 overflow-hidden">
              <div class="thin-scrollbar h-full overflow-y-auto p-6">
                <Show when={activeSettingsSection() === "general"}>
                  <section class="space-y-4">
                    <p class="text-lg font-semibold">General</p>
                    <div class="overflow-hidden rounded-xl border">
                      <div class="flex flex-col gap-3 border-b px-4 py-4 md:flex-row md:items-center md:justify-between md:gap-6 md:px-5">
                        <div>
                          <label class="text-sm font-semibold" for="default-cli">
                            Default CLI
                          </label>
                          <p class="mt-1 text-xs text-muted-foreground">Used when you create a new session.</p>
                        </div>
                        <div class="flex w-full items-center gap-2 md:w-[310px]">
                          <span class="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-muted/50">
                            <CliAvatar
                              cliId={preferredCliId() ?? installedCliTools()[0]?.id ?? null}
                              label="Default CLI"
                              size="sm"
                            />
                          </span>
                          <select
                            value={preferredCliId() ?? installedCliTools()[0]?.id ?? ""}
                            onChange={(event) => store.setPreferredCliTool(event.currentTarget.value || null)}
                            class="h-10 w-full rounded-lg border border-input bg-background px-3 text-sm outline-none focus:border-ring focus:ring-2 focus:ring-ring/30"
                          >
                            <Show when={installedCliTools().length > 0} fallback={<option value="" disabled>No CLI tools detected</option>}>
                              <For each={installedCliTools()}>
                                {(tool) => <option value={tool.id}>{tool.label}</option>}
                              </For>
                            </Show>
                          </select>
                        </div>
                      </div>

                      <div class="flex flex-col gap-3 px-4 py-4 md:flex-row md:items-center md:justify-between md:gap-6 md:px-5">
                        <div>
                          <label class="text-sm font-semibold" for="default-shell">
                            Default Shell
                          </label>
                          <p class="mt-1 text-xs text-muted-foreground">Used when you open a new terminal.</p>
                        </div>
                        <div class="w-full md:w-[310px]">
                          <select
                            value={preferredShell() ?? availableShells()[0] ?? ""}
                            onChange={(event) => store.setPreferredShell(event.currentTarget.value || null)}
                            class="h-10 w-full rounded-lg border border-input bg-background px-3 text-sm outline-none focus:border-ring focus:ring-2 focus:ring-ring/30"
                          >
                            <Show when={availableShells().length > 0} fallback={<option value="" disabled>No shells detected</option>}>
                              <For each={availableShells()}>
                                {(shell) => <option value={shell}>{getShellLabel(shell)}</option>}
                              </For>
                            </Show>
                          </select>
                        </div>
                      </div>
                    </div>
                  </section>
                </Show>

                <Show when={activeSettingsSection() === "providers"}>
                  <section class="space-y-4">
                    <p class="text-lg font-semibold">Providers</p>
                    <div class="overflow-hidden rounded-xl border">
                      <div class="flex flex-col gap-3 px-4 py-4 md:flex-row md:items-center md:justify-between md:gap-6 md:px-5">
                        <div>
                          <label class="text-sm font-semibold" for="cli-launch-mode">
                            Provider Switch Mode
                          </label>
                          <p class="mt-1 text-xs text-muted-foreground">Choose whether provider changes open a new tab or replace the current one.</p>
                        </div>
                        <div class="w-full md:w-[310px]">
                          <select
                            value={cliLaunchMode()}
                            onChange={(event) =>
                              store.setCliLaunchMode(event.currentTarget.value === "replace-current" ? "replace-current" : "new-tab")
                            }
                            class="h-10 w-full rounded-lg border border-input bg-background px-3 text-sm outline-none focus:border-ring focus:ring-2 focus:ring-ring/30"
                          >
                            <option value="new-tab">Open in new tab</option>
                            <option value="replace-current">Replace current tab</option>
                          </select>
                        </div>
                      </div>
                    </div>
                  </section>
                </Show>

                <Show when={activeSettingsSection() === "cli-tools"}>
                  <section class="flex h-full min-h-0 flex-col gap-4">
                    <p class="text-lg font-semibold">CLI Tools</p>
                    <div class="flex items-center gap-2">
                      <input
                        value={cliToolSearchQuery()}
                        onInput={(event) => setCliToolSearchQuery(event.currentTarget.value)}
                        placeholder="Search tools, status, command..."
                        class="h-10 w-full rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none focus:border-ring focus:ring-2 focus:ring-ring/20"
                      />
                      <Show when={cliToolSearchQuery().trim()}>
                        <Button
                          type="button"
                          size="sm"
                          variant="ghost"
                          onClick={() => setCliToolSearchQuery("")}
                        >
                          Clear
                        </Button>
                      </Show>
                    </div>
                    <div class="min-h-0 flex-1 overflow-hidden rounded-xl border">
                      <div class="thin-scrollbar h-full max-h-[460px] overflow-y-auto">
                        <Table>
                        <TableHeader>
                          <TableRow>
                            <TableHead class="w-[56%]">Tool</TableHead>
                            <TableHead class="w-[24%]">Status</TableHead>
                            <TableHead class="w-[20%] text-right">Action</TableHead>
                          </TableRow>
                        </TableHeader>
                        <TableBody>
                          <For each={filteredCliTools()}>
                            {(tool) => (
                              <TableRow>
                                <TableCell class="whitespace-normal">
                                  <div class="flex items-center gap-2">
                                    <CliAvatar cliId={tool.id} label={tool.label} size="sm" />
                                    <span class="font-medium">{tool.label}</span>
                                  </div>
                                </TableCell>
                                <TableCell class="whitespace-normal">
                                  {tool.installed ? (
                                    <span class="text-green-500">Installed</span>
                                  ) : (
                                    <span class="text-muted-foreground">Not installed</span>
                                  )}
                                </TableCell>
                                <TableCell class="text-right">
                                  {tool.installed ? (
                                    <span class="text-sm text-muted-foreground">Ready</span>
                                  ) : (
                                    <Button
                                      size="sm"
                                      variant="outline"
                                      onClick={() => {
                                        store.installCliTool(tool.id, tool.installCommand)
                                        setIsSettingsOpen(false)
                                      }}
                                    >
                                      Install
                                    </Button>
                                  )}
                                </TableCell>
                              </TableRow>
                            )}
                          </For>
                          <Show when={filteredCliTools().length === 0}>
                            <TableRow>
                              <TableCell colSpan={3} class="py-8 text-center text-sm text-muted-foreground">
                                No tools found for "{cliToolSearchQuery().trim()}".
                              </TableCell>
                            </TableRow>
                          </Show>
                        </TableBody>
                      </Table>
                      </div>
                    </div>
                  </section>
                </Show>


              </div>
            </div>
          </div>
        </DialogContent>
      </Dialog>

      <Dialog open={!!editingProject()} onOpenChange={(open: boolean) => !open && setEditingProjectId(null)}>
        <DialogContent class="w-[420px] max-w-[calc(100vw-2rem)] overflow-hidden">
          <DialogHeader class="min-w-0">
            <DialogTitle>Edit project</DialogTitle>
            <DialogDescription class="max-w-full break-all text-xs">{editingProject()?.path}</DialogDescription>
          </DialogHeader>

          <div class="min-w-0 space-y-4">
            <label class="block min-w-0">
              <span class="mb-2 block text-sm font-medium text-foreground">Project name</span>
              <input
                value={draftProjectName()}
                onInput={(event) => setDraftProjectName(event.currentTarget.value)}
                class="h-11 w-full min-w-0 max-w-full rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none focus:border-ring focus:ring-2 focus:ring-ring/20"
              />
            </label>

            <div>
              <span class="mb-2 block text-sm font-medium text-foreground">Color</span>
              <div class="flex flex-wrap gap-2">
                <For each={PROJECT_COLOR_OPTIONS}>
                  {(option) => (
                    <button
                      type="button"
                      onClick={() => setDraftProjectColor(option.value)}
                      class={`h-9 w-9 rounded-xl border transition ${
                        draftProjectColor() === option.value ? "border-foreground/80 scale-105" : "border-border"
                      }`}
                      style={{ background: option.value }}
                      title={option.value}
                    />
                  )}
                </For>
              </div>
            </div>

            <div>
              <span class="mb-2 block text-sm font-medium text-foreground">Logo</span>
              <div class="flex w-full min-w-0 max-w-full flex-col gap-3 rounded-[10px] border bg-muted p-3 sm:flex-row sm:items-center">
                <span class="inline-flex h-12 w-12 items-center justify-center overflow-hidden rounded-[10px] border bg-background">
                  <Show when={draftProjectLogoPath()} fallback={
                    <Show when={editingProject()} fallback={
                      <span class="text-[10px] text-muted-foreground">No preview</span>
                    }>
                      {renderProjectMark({ ...editingProject()!, color: draftProjectColor(), logoPath: null } as Project, "md")}
                    </Show>
                  }>
                    <Show when={draftProjectLogoUrl()} fallback={
                      <span class="text-[10px] text-muted-foreground">No preview</span>
                    }>
                      <img src={draftProjectLogoUrl()!} alt="Project logo" class="h-full w-full object-cover" />
                    </Show>
                  </Show>
                </span>
                <div class="min-w-0 flex-1 overflow-hidden">
                  <p class="truncate text-sm text-foreground/78">
                    {draftProjectLogoPath() ? draftProjectLogoPath()!.split(/[\\/]/).pop() : "No custom logo selected"}
                  </p>
                  <p class="mt-1 text-xs text-muted-foreground">PNG, JPG, WEBP, SVG or ICO</p>
                </div>
                <Button
                  type="button"
                  onClick={handleChooseProjectLogo}
                  variant="outline"
                  size="sm"
                  class="shrink-0 self-start sm:self-auto"
                >
                  <Upload class="mr-2 h-4 w-4" stroke-width={1.9} />
                  Browse
                </Button>
              </div>
              <Show when={draftProjectLogoPath()}>
                <Button
                  type="button"
                  onClick={() => setDraftProjectLogoPath(null)}
                  variant="link"
                  class="mt-2 text-xs text-muted-foreground"
                >
                  Remove custom logo
                </Button>
              </Show>
            </div>
          </div>

          <DialogFooter>
            <div class="flex items-center gap-2 text-xs text-muted-foreground">
              <Palette class="h-4 w-4" stroke-width={1.9} />
              Project appearance
            </div>
            <div class="flex gap-2">
              <Button
                type="button"
                onClick={() => setEditingProjectId(null)}
                variant="outline"
              >
                Cancel
              </Button>
              <Button
                type="button"
                onClick={handleSaveProjectEdit}
              >
                Save
              </Button>
            </div>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}
