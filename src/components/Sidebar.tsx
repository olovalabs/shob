import { useEffect, useMemo, useState } from "react"
import { open } from "@tauri-apps/plugin-dialog"
import { invoke } from "@tauri-apps/api/core"
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

  const currentProject = useMemo(
    () => projects.find((project) => project.id === currentProjectId) ?? null,
    [projects, currentProjectId],
  )
  const editingProject = useMemo(
    () => projects.find((project) => project.id === editingProjectId) ?? null,
    [projects, editingProjectId],
  )
  const logoSignature = useMemo(
    () => projects.map((project) => `${project.id}:${project.logoPath ?? ""}`).join("|"),
    [projects],
  )
  useEffect(() => {
    setExpandedProjects((current) => {
      const next = { ...current }

      for (const project of projects) {
        if (!(project.id in next)) {
          next[project.id] = true
        }
      }

      for (const projectId of Object.keys(next)) {
        if (!projects.some((project) => project.id === projectId)) {
          delete next[projectId]
        }
      }

      return next
    })
  }, [projects])

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
            const dataUrl = await invoke<string>("read_image_data_url", { path: project.logoPath })
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
  }, [logoSignature])

  useEffect(() => {
    let isDisposed = false

    const syncDraftLogo = async () => {
      if (!draftProjectLogoPath) {
        setDraftProjectLogoUrl(null)
        return
      }

      try {
        const dataUrl = await invoke<string>("read_image_data_url", { path: draftProjectLogoPath })
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

  const filteredCurrentSessions = useMemo(() => currentProject?.sessions ?? [], [currentProject])
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
    const selected = await open({
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
    const selected = await open({
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
      <aside className={`relative flex h-full shrink-0 border-r bg-background text-foreground ${isSessionPaneVisible ? "w-[392px]" : "w-[52px]"}`}>
        <div className="flex w-[52px] flex-col items-center border-r bg-muted/30 px-2 py-3">
          <div className="flex w-full flex-col items-center gap-4">
            {projects.map((project) => {
              const isActive = project.id === currentProjectId

              return (
                <div key={project.id} className="group relative">
                  <button
                    type="button"
                    onClick={() => setCurrentProject(project.id)}
                    className={`relative rounded-[10px] p-0.5 transition-colors ${isActive ? "bg-accent/70" : "hover:bg-accent/55"}`}
                  >
                    {renderProjectMark(project, "md")}
                    {isActive && <span className="pointer-events-none absolute inset-0 rounded-[10px] ring-1 ring-ring/40" />}
                  </button>
                </div>
              )
            })}

            <button
              type="button"
              onClick={handleAddProject}
              className="group relative rounded-[10px] p-0.5 transition-colors hover:bg-accent/55"
              title="Add project"
            >
              <span className="flex h-10 w-10 items-center justify-center rounded-[8px] bg-muted/45 text-muted-foreground transition-colors group-hover:bg-muted/60 group-hover:text-foreground">
                <Plus className="h-4 w-4" strokeWidth={2} />
              </span>
            </button>
          </div>

          <div className="mt-auto flex w-full flex-col items-center gap-3">
            <Button
              type="button"
              onClick={() => setIsSettingsOpen((current) => !current)}
              variant="ghost"
              size="icon-sm"
              className="h-8 w-8"
              title="Settings"
              aria-pressed={isSettingsOpen}
            >
              <Settings className="h-4 w-4" strokeWidth={1.9} />
            </Button>
          </div>
        </div>

        {isSessionPaneVisible && (
        <div className="flex min-w-0 flex-1 flex-col bg-background">
          <div className="border-b px-4 py-4">
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <p className="truncate text-[28px] leading-none font-semibold text-foreground">{currentProject?.name ?? "Workspace"}</p>
                <p className="mt-1 truncate text-[14px] text-muted-foreground">
                  {currentProject?.path ? currentProject.path.replace(/^([A-Za-z]):\\Users\\[^\\]+/, "~") : "Add a project to get started"}
                </p>
              </div>
              {currentProject && (
                <div className="relative">
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-sm"
                        className="h-8 w-8 shrink-0"
                        title="Project options"
                      >
                        <MoreHorizontal className="h-4 w-4" strokeWidth={2} />
                      </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end" className="w-[136px] rounded-[8px] p-1">
                      <DropdownMenuItem className="px-3 py-1.5 text-[13px]" onClick={() => openEditProject(currentProject)}>
                        Edit
                      </DropdownMenuItem>
                      <DropdownMenuItem
                        className="px-3 py-1.5 text-[13px]"
                        variant="destructive"
                        onClick={() => handleDeleteProject(currentProject.id)}
                      >
                        Close
                      </DropdownMenuItem>
                    </DropdownMenuContent>
                  </DropdownMenu>
                </div>
              )}
            </div>

            {currentProject && (
              <div className="mt-5">
                <Button
                  type="button"
                  onClick={() => handleCreateSession(currentProject.id)}
                  variant="outline"
                  className="h-10 w-full text-[15px] font-semibold"
                >
                  <FolderPlus className="mr-2 h-4 w-4" strokeWidth={1.9} />
                  New session
                </Button>
              </div>
            )}
          </div>

          <div className="hide-scrollbar flex-1 overflow-y-auto px-3 py-4">
            {!currentProject ? (
              <p className="px-3 py-4 text-sm text-muted-foreground">Select or add a project to view sessions.</p>
            ) : (
              <section>
                {(expandedProjects[currentProject.id] ?? true) && (
                  <div className="space-y-0.5">
                    {filteredCurrentSessions.length === 0 ? (
                      <Button
                        type="button"
                        onClick={() => handleCreateSession(currentProject.id)}
                        variant="ghost"
                        className="w-full justify-start px-3 py-3 h-auto"
                      >
                        Create your first session
                      </Button>
                    ) : (
                      filteredCurrentSessions.map((session, index) => {
                        const isActiveSession = activeSessionId === session.id
                        const isRunningSession = Boolean(busySessions[session.id] || session.pendingLaunchCommand)
                        const sessionStatus = getSessionStatus({
                          isActiveSession,
                          isRunningSession,
                        })
                        const projectAgeLabel = formatRelativeSessionTime(session.lastActiveAt ?? session.createdAt)
                        const commandCountLabel = formatCommandCount(session.commandCount)

                        return (
                          <div
                            key={session.id}
                            className={`group flex items-center gap-2 rounded-lg px-2 py-1.5 transition ${
                              isActiveSession
                                ? "bg-accent text-accent-foreground"
                                : "text-foreground/84 hover:bg-accent/50"
                            }`}
                          >
                            <button
                              type="button"
                              onClick={() => {
                                setCurrentProject(currentProject.id)
                                setActiveSession(session.id)
                              }}
                              className="flex min-w-0 flex-1 items-center gap-2 text-left"
                            >
                              {isRunningSession && (
                                <span className="inline-flex h-4 w-4 shrink-0 items-center justify-center">
                                  <Spinner className="h-3.5 w-3.5 text-[#00BF63]" />
                                </span>
                              )}
                              <span className="min-w-0 flex-1 truncate text-[14px] font-medium text-foreground">
                                {session.name || formatSessionLabel(index)}
                              </span>
                              {commandCountLabel && (
                                <span className="shrink-0 rounded-sm bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
                                  {commandCountLabel}
                                </span>
                              )}
                              {projectAgeLabel && (
                                <span className="shrink-0 text-[12px] text-muted-foreground">{projectAgeLabel}</span>
                              )}
                            </button>

                            <div className="flex items-center gap-1">
                              <span className="relative inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-md bg-muted/50">
                                <CliAvatar cliId={session.cliTool} label={session.name} size="sm" className="opacity-80" />
                                <span
                                  className={`pointer-events-none absolute -right-0.5 -top-0.5 h-2.5 w-2.5 rounded-full border border-background ${SESSION_STATUS_STYLES[sessionStatus.tone]}`}
                                  title={sessionStatus.title}
                                  aria-label={sessionStatus.label}
                                />
                              </span>
                              <Button
                                type="button"
                                onPointerDown={(event) => {
                                  event.preventDefault()
                                  event.stopPropagation()
                                }}
                                onClick={(event) => {
                                  event.stopPropagation()
                                  void removeSession(currentProject.id, session.id)
                                }}
                                variant="ghost"
                                size="icon-xs"
                                className="h-5 w-5 opacity-0 group-hover:opacity-100 transition-opacity"
                                title="Close session"
                              >
                                <span className="text-xs leading-none">×</span>
                              </Button>
                            </div>
                          </div>
                        )
                      })
                    )}
                  </div>
                )}
              </section>
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
