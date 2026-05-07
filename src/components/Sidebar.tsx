import { useEffect, useMemo, useState } from "react"
import { nativeApi } from "../services/native"
import {
  Boxes,
  Folder,
  Palette,
  PencilLine,
  Plus,
  SlidersHorizontal,
  Settings,
  Terminal,
  Upload,
} from "lucide-react"
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
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
const VISIBLE_SESSIONS_PER_PROJECT = 5
const SIDEBAR_MENU_CLASS = "rounded-md border border-white/[0.08] bg-[#1c1c1c] p-1 text-[#d7d7d7] shadow-xl ring-0"
const SIDEBAR_MENU_ITEM_CLASS = "rounded-[5px] px-2.5 py-1.5 text-[13px] focus:bg-white/[0.07] focus:text-white"

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

export function Sidebar() {
  const {
    projects,
    currentProjectId,
    activeSessionId,
    preferredCliId,
    preferredShell,
    cliLaunchMode,
    cliTools,
    availableShells,
    setCurrentProject,
    setActiveSession,
    addProject,
    updateProject,
    deleteProject,
    launchCliSession,
    removeSession,
    setPreferredCliTool,
    setPreferredShell,
    setCliLaunchMode,
    installCliTool,
  } = useStore()
  const [expandedProjects, setExpandedProjects] = useState<Record<string, boolean>>({})
  const [isSessionPaneVisible, setIsSessionPaneVisible] = useState(true)
  const [busySessions, setBusySessions] = useState<Record<string, boolean>>({})
  const [isSettingsOpen, setIsSettingsOpen] = useState(false)
  const [activeSettingsSection, setActiveSettingsSection] = useState<"general" | "providers" | "cli-tools">("general")
  const [cliToolSearchQuery, setCliToolSearchQuery] = useState("")
  const [editingProjectId, setEditingProjectId] = useState<string | null>(null)
  const [draftProjectName, setDraftProjectName] = useState("")
  const [draftProjectColor, setDraftProjectColor] = useState<string>(PROJECT_COLOR_OPTIONS[0].value)
  const [draftProjectLogoPath, setDraftProjectLogoPath] = useState<string | null>(null)
  const [projectLogoUrls, setProjectLogoUrls] = useState<Record<string, string>>({})
  const [draftProjectLogoUrl, setDraftProjectLogoUrl] = useState<string | null>(null)

  const editingProject = useMemo(
    () => projects.find((project) => project.id === editingProjectId) ?? null,
    [projects, editingProjectId],
  )
  const logoSignature = useMemo(
    () => projects.map((project) => `${project.id}:${project.logoPath ?? ""}`).join("|"),
    [projects],
  )
  useEffect(() => {
    window.dispatchEvent(
      new CustomEvent("gg-sidebar-state", {
        detail: {
          isSidebarVisible: isSessionPaneVisible,
        },
      }),
    )
  }, [isSessionPaneVisible])

  useEffect(() => {
    const handleSidebarToggleRequest = () => {
      setIsSessionPaneVisible((current) => !current)
    }

    window.addEventListener("gg-toggle-sidebar", handleSidebarToggleRequest)
    return () => window.removeEventListener("gg-toggle-sidebar", handleSidebarToggleRequest)
  }, [])

  useEffect(() => {
    let isDisposed = false
    const startTimer = window.setTimeout(() => {
      void syncProjectLogos()
    }, 120)

    const syncProjectLogos = async () => {
      const entries = await Promise.all(
        projects.map(async (project) => {
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

      if (isDisposed) return
      setProjectLogoUrls(Object.fromEntries(entries))
    }

    return () => {
      isDisposed = true
      window.clearTimeout(startTimer)
    }
  }, [logoSignature, projects])

  useEffect(() => {
    let isDisposed = false

    const syncDraftLogo = async () => {
      if (!draftProjectLogoPath) {
        setDraftProjectLogoUrl(null)
        return
      }

      try {
        const dataUrl = await nativeApi.invoke("read_image_data_url", { path: draftProjectLogoPath })
        if (!isDisposed) {
          setDraftProjectLogoUrl(dataUrl)
        }
      } catch {
        if (!isDisposed) {
          setDraftProjectLogoUrl(null)
        }
      }
    }

    void syncDraftLogo()

    return () => {
      isDisposed = true
    }
  }, [draftProjectLogoPath])

  useEffect(() => {
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

    return () => {
      idleTimers.forEach((timer) => window.clearTimeout(timer))
      window.removeEventListener("gg-pty-data", handlePtyData as EventListener)
    }
  }, [activeSessionId])

  const installedCliTools = useMemo(() => cliTools.filter((tool) => tool.installed), [cliTools])
  const filteredCliTools = useMemo(() => {
    const query = cliToolSearchQuery.trim().toLowerCase()
    if (!query) return cliTools

    return cliTools.filter((tool) => {
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
  }, [cliTools, cliToolSearchQuery])

  const openEditProject = (project: Project) => {
    setEditingProjectId(project.id)
    setDraftProjectName(project.name)
    setDraftProjectColor(project.color ?? PROJECT_COLOR_OPTIONS[0].value)
    setDraftProjectLogoPath(project.logoPath ?? null)
    setDraftProjectLogoUrl(project.logoPath ? projectLogoUrls[project.id] ?? null : null)
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
    if (!editingProject) return

    const nextName = draftProjectName.trim()
    if (!nextName) return

    await updateProject(editingProject.id, {
      name: nextName,
      color: draftProjectColor,
      logoPath: draftProjectLogoPath,
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

    const existing = projects.find((project) => project.path === selected)
    if (existing) {
      setCurrentProject(existing.id)
      setExpandedProjects((current) => ({ ...current, [existing.id]: true }))
      return
    }

    const projectName = folderNameFromPath(selected)
    const created = await addProject(projectName, selected)
    setExpandedProjects((current) => ({ ...current, [created.id]: true }))
  }

  const handleCreateSession = async (projectId: string) => {
    await launchCliSession(projectId)
    setExpandedProjects((current) => ({ ...current, [projectId]: true }))
  }

  const handleDeleteProject = async (projectId: string) => {
    await deleteProject(projectId)
    setExpandedProjects((current) => {
      const next = { ...current }
      delete next[projectId]
      return next
    })
    if (editingProjectId === projectId) {
      setEditingProjectId(null)
    }
  }

  const renderProjectMark = (project: Project, size: "sm" | "md" = "md") => {
    const theme = getProjectTheme(project)
    const badge = getProjectBadge(project.name)
    const logoUrl = projectLogoUrls[project.id]

    if (project.logoPath && logoUrl) {
      return (
        <Avatar
          className={
            size === "md"
              ? "h-10 w-10 overflow-hidden rounded-[8px] after:rounded-[8px]"
              : "h-6 w-6 overflow-hidden rounded-[6px] after:rounded-[6px]"
          }
        >
          <AvatarImage src={logoUrl} alt={project.name} className="rounded-none object-cover" />
          <AvatarFallback className="rounded-none" style={{ background: theme.bg, color: theme.text }}>
            {badge}
          </AvatarFallback>
        </Avatar>
      )
    }

    return (
      <Avatar
        className={
          size === "md"
            ? "h-10 w-10 overflow-hidden rounded-[8px] after:rounded-[8px]"
            : "h-6 w-6 overflow-hidden rounded-[6px] after:rounded-[6px]"
        }
      >
        <AvatarFallback className="rounded-none" style={{ background: theme.bg, color: theme.text }}>
          {badge}
        </AvatarFallback>
      </Avatar>
    )
  }

  return (
    <>
      <aside
        className={`relative flex h-full shrink-0 border-r border-[#242424] bg-[#171717] text-[#dedede] transition-[width] duration-150 ${
          isSessionPaneVisible ? "w-[304px]" : "w-[52px]"
        }`}
      >
        {!isSessionPaneVisible ? (
          <div className="flex w-full flex-col items-center px-2 py-3">
            <div className="flex w-full flex-col items-center gap-2">
              {projects.map((project) => {
                const isActive = project.id === currentProjectId

                return (
                  <button
                    key={project.id}
                    type="button"
                    onClick={() => setCurrentProject(project.id)}
                    className={`relative rounded-[8px] p-1 transition-colors ${
                      isActive ? "bg-[#2b2b2b]" : "hover:bg-white/[0.06]"
                    }`}
                    title={project.name}
                    aria-label={project.name}
                  >
                    {renderProjectMark(project, "sm")}
                  </button>
                )
              })}

              <button
                type="button"
                onClick={handleAddProject}
                className="flex h-8 w-8 items-center justify-center rounded-[8px] text-[#8b8b8b] transition-colors hover:bg-white/[0.06] hover:text-white"
                title="Add project"
                aria-label="Add project"
              >
                <Plus className="h-4 w-4" strokeWidth={1.9} />
              </button>
            </div>

            <button
              type="button"
              onClick={() => setIsSettingsOpen((current) => !current)}
              className="mt-auto flex h-8 w-8 items-center justify-center rounded-[8px] text-[#8b8b8b] transition-colors hover:bg-white/[0.06] hover:text-white"
              title="Settings"
              aria-label="Settings"
              aria-pressed={isSettingsOpen}
            >
              <Settings className="h-4 w-4" strokeWidth={1.9} />
            </button>
          </div>
        ) : (
          <div className="flex min-w-0 flex-1 flex-col bg-[#171717]">
            <div className="group/sidebar flex h-10 shrink-0 items-center justify-between px-4">
              <p className="text-[13px] font-medium text-[#858585]">Projects</p>
              <div className="flex items-center gap-1 opacity-70 transition-opacity group-hover/sidebar:opacity-100">
                <button
                  type="button"
                  onClick={handleAddProject}
                  className="flex h-7 w-7 items-center justify-center rounded-[7px] text-[#8f8f8f] transition-colors hover:bg-white/[0.06] hover:text-white"
                  title="Add project"
                  aria-label="Add project"
                >
                  <Plus className="h-3.5 w-3.5" strokeWidth={1.9} />
                </button>
                <button
                  type="button"
                  onClick={() => setIsSettingsOpen((current) => !current)}
                  className="flex h-7 w-7 items-center justify-center rounded-[7px] text-[#8f8f8f] transition-colors hover:bg-white/[0.06] hover:text-white"
                  title="Settings"
                  aria-label="Settings"
                  aria-pressed={isSettingsOpen}
                >
                  <Settings className="h-3.5 w-3.5" strokeWidth={1.9} />
                </button>
              </div>
            </div>

            <div className="thin-scrollbar min-h-0 flex-1 overflow-y-auto px-2 pb-5">
              {projects.length === 0 ? (
                <div className="px-2 pt-4">
                  <button
                    type="button"
                    onClick={handleAddProject}
                    className="flex h-8 w-full items-center rounded-[8px] px-2 text-left text-[13px] font-medium text-[#9a9a9a] transition-colors hover:bg-white/[0.06] hover:text-white"
                  >
                    Add your first project
                  </button>
                </div>
              ) : (
                <div className="space-y-4">
                  {projects.map((project) => {
                    const isActiveProject = project.id === currentProjectId
                    const showAllSessions = expandedProjects[project.id] ?? false
                    const sortedSessions = [...project.sessions].sort((left, right) => {
                      const leftActivity = left.lastActiveAt ?? left.createdAt ?? 0
                      const rightActivity = right.lastActiveAt ?? right.createdAt ?? 0
                      return rightActivity - leftActivity
                    })
                    const visibleSessions = showAllSessions
                      ? sortedSessions
                      : sortedSessions.slice(0, VISIBLE_SESSIONS_PER_PROJECT)

                    return (
                      <section key={project.id} className="group/project min-w-0">
                        <div className="mb-1 flex h-7 items-center gap-1 px-2">
                          <button
                            type="button"
                            onClick={() => setCurrentProject(project.id)}
                            className={`flex min-w-0 flex-1 items-center gap-2 text-left text-[13px] font-semibold transition-colors ${
                              isActiveProject ? "text-[#c9c9c9]" : "text-[#a0a0a0] hover:text-white"
                            }`}
                            title={project.path}
                          >
                            <Folder className="h-4 w-4 shrink-0" strokeWidth={1.7} />
                            <span className="truncate">{project.name}</span>
                          </button>

                          <DropdownMenu>
                            <DropdownMenuTrigger asChild>
                              <button
                                type="button"
                                className="flex h-6 w-6 shrink-0 items-center justify-center rounded-[6px] text-[#929292] opacity-80 transition-colors hover:bg-white/[0.06] hover:text-white group-hover/project:opacity-100"
                                title="Project options"
                                aria-label={`${project.name} options`}
                              >
                                <PencilLine className="h-3.5 w-3.5" strokeWidth={1.8} />
                              </button>
                            </DropdownMenuTrigger>
                            <DropdownMenuContent align="end" className={`w-[146px] ${SIDEBAR_MENU_CLASS}`}>
                              <DropdownMenuItem
                                className={SIDEBAR_MENU_ITEM_CLASS}
                                onClick={() => void handleCreateSession(project.id)}
                              >
                                New session
                              </DropdownMenuItem>
                              <DropdownMenuItem
                                className={SIDEBAR_MENU_ITEM_CLASS}
                                onClick={() => openEditProject(project)}
                              >
                                Edit
                              </DropdownMenuItem>
                              <DropdownMenuItem
                                className={SIDEBAR_MENU_ITEM_CLASS}
                                variant="destructive"
                                onClick={() => handleDeleteProject(project.id)}
                              >
                                Close
                              </DropdownMenuItem>
                            </DropdownMenuContent>
                          </DropdownMenu>
                        </div>

                        <div className="space-y-0.5">
                          {visibleSessions.length === 0 ? (
                            <button
                              type="button"
                              onClick={() => void handleCreateSession(project.id)}
                              className="ml-7 flex h-8 w-[calc(100%-1.75rem)] items-center rounded-[8px] px-2 text-left text-[13px] font-medium text-[#848484] transition-colors hover:bg-white/[0.06] hover:text-white"
                            >
                              Create session
                            </button>
                          ) : (
                            visibleSessions.map((session, index) => {
                              const isActiveSession = activeSessionId === session.id
                              const isRunningSession = Boolean(busySessions[session.id] || session.pendingLaunchCommand)
                              const projectAgeLabel = formatRelativeSessionTime(session.lastActiveAt ?? session.createdAt)

                              return (
                                <div key={session.id} className="group/session relative">
                                  <button
                                    type="button"
                                    onClick={() => {
                                      setCurrentProject(project.id)
                                      setActiveSession(session.id)
                                    }}
                                    className={`flex h-8 w-full min-w-0 items-center rounded-[8px] pl-8 pr-3 text-left transition-colors ${
                                      isActiveSession
                                        ? "bg-[#2b2b2b] text-white"
                                        : "text-[#e2e2e2] hover:bg-white/[0.045] hover:text-white"
                                    }`}
                                  >
                                    {isRunningSession && (
                                      <span className="mr-1.5 inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center">
                                        <Spinner className="h-3 w-3 text-[#bdbdbd]" />
                                      </span>
                                    )}
                                    <span className="min-w-0 flex-1 truncate text-[13px] font-medium leading-none">
                                      {session.name || formatSessionLabel(index)}
                                    </span>
                                    {projectAgeLabel && (
                                      <span className="ml-3 shrink-0 text-[12px] leading-none text-[#929292] group-hover/session:opacity-0">
                                        {projectAgeLabel}
                                      </span>
                                    )}
                                  </button>

                                  <button
                                    type="button"
                                    onPointerDown={(event) => {
                                      event.preventDefault()
                                      event.stopPropagation()
                                    }}
                                    onClick={(event) => {
                                      event.stopPropagation()
                                      void removeSession(project.id, session.id)
                                    }}
                                    className="absolute right-2 top-1/2 flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded-[5px] text-[15px] leading-none text-[#9c9c9c] opacity-0 transition hover:bg-white/[0.08] hover:text-white group-hover/session:opacity-100"
                                    title="Close session"
                                    aria-label={`Close ${session.name || formatSessionLabel(index)}`}
                                  >
                                    ×
                                  </button>
                                </div>
                              )
                            })
                          )}

                          {project.sessions.length > VISIBLE_SESSIONS_PER_PROJECT && (
                            <button
                              type="button"
                              onClick={() => {
                                setExpandedProjects((current) => ({
                                  ...current,
                                  [project.id]: !showAllSessions,
                                }))
                              }}
                              className="ml-7 flex h-8 items-center px-2 text-left text-[13px] font-medium text-[#878787] transition-colors hover:text-white"
                            >
                              {showAllSessions ? "Show less" : "Show more"}
                            </button>
                          )}
                        </div>
                      </section>
                    )
                  })}
                </div>
              )}
            </div>
          </div>
        )}
      </aside>

      <Dialog open={isSettingsOpen} onOpenChange={setIsSettingsOpen}>
        <DialogContent
          className="!h-[640px] !w-[980px] !max-w-none sm:!max-w-none gap-0 overflow-hidden border p-0"
          style={{ width: 980, height: 640, maxWidth: "none" }}
        >
          <div className="flex h-full flex-row">
            <aside className="w-[190px] border-r bg-card px-3 py-5">
              <div className="thin-scrollbar max-h-full overflow-y-auto pr-1">
                <div className="grid grid-cols-1 gap-1.5">
                  <Button
                    type="button"
                    onClick={() => setActiveSettingsSection("general")}
                    variant={activeSettingsSection === "general" ? "secondary" : "ghost"}
                    className="justify-start gap-2.5 px-3 py-2 h-auto"
                  >
                    <SlidersHorizontal className="h-4 w-4" strokeWidth={1.9} />
                    <span className="font-medium">General</span>
                  </Button>
                  <Button
                    type="button"
                    onClick={() => setActiveSettingsSection("providers")}
                    variant={activeSettingsSection === "providers" ? "secondary" : "ghost"}
                    className="justify-start gap-2.5 px-3 py-2 h-auto"
                  >
                    <Boxes className="h-4 w-4" strokeWidth={1.9} />
                    <span className="font-medium">Providers</span>
                  </Button>
                  <Button
                    type="button"
                    onClick={() => setActiveSettingsSection("cli-tools")}
                    variant={activeSettingsSection === "cli-tools" ? "secondary" : "ghost"}
                    className="justify-start gap-2.5 px-3 py-2 h-auto"
                  >
                    <Terminal className="h-4 w-4" strokeWidth={1.9} />
                    <span className="font-medium">CLI Tools</span>
                  </Button>
                </div>
              </div>
            </aside>

            <div className="relative min-w-0 flex-1 overflow-hidden">
              <div className="thin-scrollbar h-full overflow-y-auto p-6">
                {activeSettingsSection === "general" && (
                  <section className="space-y-4">
                    <p className="text-lg font-semibold">General</p>
                    <div className="overflow-hidden rounded-xl border">
                      <div className="flex flex-col gap-3 border-b px-4 py-4 md:flex-row md:items-center md:justify-between md:gap-6 md:px-5">
                        <div>
                          <label className="text-sm font-semibold" htmlFor="default-cli">
                            Default CLI
                          </label>
                          <p className="mt-1 text-xs text-muted-foreground">Used when you create a new session.</p>
                        </div>
                        <div className="flex w-full items-center gap-2 md:w-[310px]">
                          <span className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-muted/50">
                            <CliAvatar
                              cliId={preferredCliId ?? installedCliTools[0]?.id ?? null}
                              label="Default CLI"
                              size="sm"
                            />
                          </span>
                          <Select
                            value={preferredCliId ?? installedCliTools[0]?.id ?? ""}
                            onValueChange={(value) => setPreferredCliTool(value || null)}
                          >
                            <SelectTrigger className="h-10 w-full">
                              <SelectValue placeholder="Select CLI" />
                            </SelectTrigger>
                            <SelectContent className="p-1">
                              {installedCliTools.length === 0 ? (
                                <SelectItem className="py-1" value="" disabled>No CLI tools detected</SelectItem>
                              ) : (
                                installedCliTools.map((tool) => (
                                  <SelectItem className="py-1" key={tool.id} value={tool.id}>
                                    {tool.label}
                                  </SelectItem>
                                ))
                              )}
                            </SelectContent>
                          </Select>
                        </div>
                      </div>

                      <div className="flex flex-col gap-3 px-4 py-4 md:flex-row md:items-center md:justify-between md:gap-6 md:px-5">
                        <div>
                          <label className="text-sm font-semibold" htmlFor="default-shell">
                            Default Shell
                          </label>
                          <p className="mt-1 text-xs text-muted-foreground">Used when you open a new terminal.</p>
                        </div>
                        <div className="w-full md:w-[310px]">
                          <Select
                            value={preferredShell ?? availableShells[0] ?? ""}
                            onValueChange={(value) => setPreferredShell(value || null)}
                          >
                            <SelectTrigger className="h-10 w-full">
                              <SelectValue placeholder="Select shell" />
                            </SelectTrigger>
                            <SelectContent className="p-1">
                              {availableShells.length === 0 ? (
                                <SelectItem className="py-1" value="" disabled>No shells detected</SelectItem>
                              ) : (
                                availableShells.map((shell) => (
                                  <SelectItem className="py-1" key={shell} value={shell}>
                                    {getShellLabel(shell)}
                                  </SelectItem>
                                ))
                              )}
                            </SelectContent>
                          </Select>
                        </div>
                      </div>
                    </div>
                  </section>
                )}

                {activeSettingsSection === "providers" && (
                  <section className="space-y-4">
                    <p className="text-lg font-semibold">Providers</p>
                    <div className="overflow-hidden rounded-xl border">
                      <div className="flex flex-col gap-3 px-4 py-4 md:flex-row md:items-center md:justify-between md:gap-6 md:px-5">
                        <div>
                          <label className="text-sm font-semibold" htmlFor="cli-launch-mode">
                            Provider Switch Mode
                          </label>
                          <p className="mt-1 text-xs text-muted-foreground">Choose whether provider changes open a new tab or replace the current one.</p>
                        </div>
                        <div className="w-full md:w-[310px]">
                          <Select
                            value={cliLaunchMode}
                            onValueChange={(value) =>
                              setCliLaunchMode(value === "replace-current" ? "replace-current" : "new-tab")
                            }
                          >
                            <SelectTrigger className="h-10 w-full">
                              <SelectValue placeholder="Select mode" />
                            </SelectTrigger>
                            <SelectContent className="p-1">
                              <SelectItem className="py-1" value="new-tab">Open in new tab</SelectItem>
                              <SelectItem className="py-1" value="replace-current">Replace current tab</SelectItem>
                            </SelectContent>
                          </Select>
                        </div>
                      </div>
                    </div>
                  </section>
                )}

                {activeSettingsSection === "cli-tools" && (
                  <section className="flex h-full min-h-0 flex-col gap-4">
                    <p className="text-lg font-semibold">CLI Tools</p>
                    <div className="flex items-center gap-2">
                      <input
                        value={cliToolSearchQuery}
                        onChange={(event) => setCliToolSearchQuery(event.target.value)}
                        placeholder="Search tools, status, command..."
                        className="h-10 w-full rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none focus:border-ring focus:ring-2 focus:ring-ring/20"
                      />
                      {cliToolSearchQuery.trim() ? (
                        <Button
                          type="button"
                          size="sm"
                          variant="ghost"
                          onClick={() => setCliToolSearchQuery("")}
                        >
                          Clear
                        </Button>
                      ) : null}
                    </div>
                    <div className="min-h-0 flex-1 overflow-hidden rounded-xl border">
                      <div className="thin-scrollbar h-full max-h-[460px] overflow-y-auto">
                        <Table>
                        <TableHeader>
                          <TableRow>
                            <TableHead className="w-[56%]">Tool</TableHead>
                            <TableHead className="w-[24%]">Status</TableHead>
                            <TableHead className="w-[20%] text-right">Action</TableHead>
                          </TableRow>
                        </TableHeader>
                        <TableBody>
                          {filteredCliTools.map((tool) => (
                            <TableRow key={tool.id}>
                              <TableCell className="whitespace-normal">
                                <div className="flex items-center gap-2">
                                  <CliAvatar cliId={tool.id} label={tool.label} size="sm" />
                                  <span className="font-medium">{tool.label}</span>
                                </div>
                              </TableCell>
                              <TableCell className="whitespace-normal">
                                {tool.installed ? (
                                  <span className="text-green-500">Installed</span>
                                ) : (
                                  <span className="text-muted-foreground">Not installed</span>
                                )}
                              </TableCell>
                              <TableCell className="text-right">
                                {tool.installed ? (
                                  <span className="text-sm text-muted-foreground">Ready</span>
                                ) : (
                                  <Button
                                    size="sm"
                                    variant="outline"
                                    onClick={() => {
                                      installCliTool(tool.id, tool.installCommand)
                                      setIsSettingsOpen(false)
                                    }}
                                  >
                                    Install
                                  </Button>
                                )}
                              </TableCell>
                            </TableRow>
                          ))}
                          {filteredCliTools.length === 0 && (
                            <TableRow>
                              <TableCell colSpan={3} className="py-8 text-center text-sm text-muted-foreground">
                                No tools found for "{cliToolSearchQuery.trim()}".
                              </TableCell>
                            </TableRow>
                          )}
                        </TableBody>
                      </Table>
                      </div>
                    </div>
                  </section>
                )}

                
              </div>
            </div>
          </div>
        </DialogContent>
      </Dialog>

      <Dialog open={!!editingProject} onOpenChange={(open) => !open && setEditingProjectId(null)}>
        <DialogContent className="w-[420px] max-w-[calc(100vw-2rem)] overflow-hidden">
          <DialogHeader className="min-w-0">
            <DialogTitle>Edit project</DialogTitle>
            <DialogDescription className="max-w-full break-all text-xs">{editingProject?.path}</DialogDescription>
          </DialogHeader>

          <div className="min-w-0 space-y-4">
            <label className="block min-w-0">
              <span className="mb-2 block text-sm font-medium text-foreground">Project name</span>
              <input
                value={draftProjectName}
                onChange={(event) => setDraftProjectName(event.target.value)}
                className="h-11 w-full min-w-0 max-w-full rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none focus:border-ring focus:ring-2 focus:ring-ring/20"
              />
            </label>

            <div>
              <span className="mb-2 block text-sm font-medium text-foreground">Color</span>
              <div className="flex flex-wrap gap-2">
                {PROJECT_COLOR_OPTIONS.map((option) => (
                  <button
                    key={option.value}
                    type="button"
                    onClick={() => setDraftProjectColor(option.value)}
                    className={`h-9 w-9 rounded-xl border transition ${
                      draftProjectColor === option.value ? "border-foreground/80 scale-105" : "border-border"
                    }`}
                    style={{ background: option.value }}
                    title={option.value}
                  />
                ))}
              </div>
            </div>

            <div>
              <span className="mb-2 block text-sm font-medium text-foreground">Logo</span>
              <div className="flex w-full min-w-0 max-w-full flex-col gap-3 rounded-[10px] border bg-muted p-3 sm:flex-row sm:items-center">
                <span className="inline-flex h-12 w-12 items-center justify-center overflow-hidden rounded-[10px] border bg-background">
                  {draftProjectLogoPath ? (
                    draftProjectLogoUrl ? (
                      <img src={draftProjectLogoUrl} alt="Project logo" className="h-full w-full object-cover" />
                    ) : (
                      <span className="text-[10px] text-muted-foreground">No preview</span>
                    )
                  ) : (
                    editingProject && renderProjectMark({ ...editingProject, color: draftProjectColor, logoPath: null } as Project, "md")
                  )}
                </span>
                <div className="min-w-0 flex-1 overflow-hidden">
                  <p className="truncate text-sm text-foreground/78">
                    {draftProjectLogoPath ? draftProjectLogoPath.split(/[\\/]/).pop() : "No custom logo selected"}
                  </p>
                  <p className="mt-1 text-xs text-muted-foreground">PNG, JPG, WEBP, SVG or ICO</p>
                </div>
                <Button
                  type="button"
                  onClick={handleChooseProjectLogo}
                  variant="outline"
                  size="sm"
                  className="shrink-0 self-start sm:self-auto"
                >
                  <Upload className="mr-2 h-4 w-4" strokeWidth={1.9} />
                  Browse
                </Button>
              </div>
              {draftProjectLogoPath && (
                <Button
                  type="button"
                  onClick={() => setDraftProjectLogoPath(null)}
                  variant="link"
                  className="mt-2 text-xs text-muted-foreground"
                >
                  Remove custom logo
                </Button>
              )}
            </div>
          </div>

          <DialogFooter>
            <div className="flex items-center gap-2 text-xs text-muted-foreground">
              <Palette className="h-4 w-4" strokeWidth={1.9} />
              Project appearance
            </div>
            <div className="flex gap-2">
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
