import { Plus, SquareTerminal, X } from "lucide-react"
import { CliAvatar } from "./CliAvatar"
import { getCliDisplayLabel } from "../config/cli-ui"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"
import {
  Tabs,
  TabsList,
  TabsTrigger,
} from "@/components/ui/tabs"

const compactTitle = (title: string, maxChars: number) =>
  title.length > maxChars ? `${title.slice(0, Math.max(1, maxChars - 1))}…` : title

export function TabBar() {
  const {
    projects,
    currentProjectId,
    activeSessionId,
    setActiveSession,
    launchCliSession,
    removeSession,
  } = useStore()

  const currentProject = projects.find((project) => project.id === currentProjectId) ?? null
  const totalTabs = currentProject ? currentProject.sessions.length : 0
  const isCompactTabs = totalTabs >= 5
  const sessionTitleLimit = isCompactTabs ? 12 : 24

  if (!currentProject) return null

  const handleLaunchSession = async () => {
    await launchCliSession(currentProjectId!)
  }

  const handleCloseSession = (sessionId: string) => {
    void removeSession(currentProjectId!, sessionId)
  }

  return (
    <div className="chrome-tabbar">
      <div className="chrome-tabbar-inner">
        <div className="hide-scrollbar flex min-w-0 flex-1 items-end overflow-x-auto">
          <Tabs
            value={activeSessionId ?? ""}
            onValueChange={setActiveSession}
            className="chrome-tabs flex items-end"
          >
            <TabsList variant="line" className="chrome-tabs-list bg-transparent p-0">
              {currentProject.sessions.map((session) => {
                const isActive = session.id === activeSessionId
                const displayName = compactTitle(session.name, sessionTitleLimit)

                return (
                  <TabsTrigger
                    key={session.id}
                    value={session.id}
                    className={`chrome-tab-trigger relative group inline-flex items-center ${
                      isCompactTabs
                        ? "max-w-[156px] min-w-[92px] gap-1.5 px-2 text-[12px]"
                        : "max-w-[240px] min-w-[116px] gap-2 px-3 text-sm"
                    }`}
                    title={session.name}
                  >
                    {!isActive && <span className="chrome-tab-divider" aria-hidden="true" />}
                    {session.cliTool ? (
                      <CliAvatar
                        cliId={session.cliTool}
                        label={getCliDisplayLabel(session.cliTool) ?? session.name}
                        size="sm"
                        className={isCompactTabs ? "scale-90" : ""}
                      />
                    ) : (
                      <SquareTerminal className={`${isCompactTabs ? "h-3 w-3" : "h-3.5 w-3.5"} shrink-0 text-muted-foreground`} strokeWidth={1.9} />
                    )}
                    <span className="truncate">{displayName}</span>
                    <span
                      onPointerDown={(event) => {
                        event.preventDefault()
                        event.stopPropagation()
                      }}
                      onClick={(event) => {
                        event.stopPropagation()
                        handleCloseSession(session.id)
                      }}
                      className={`chrome-tab-close inline-flex h-4 w-4 items-center justify-center rounded-full transition ${
                        isActive ? "opacity-100" : "opacity-0 group-hover:opacity-100"
                      }`}
                      aria-label={`Close ${session.name}`}
                    >
                      <X className="h-3 w-3" strokeWidth={2} />
                    </span>
                  </TabsTrigger>
                )
              })}
            </TabsList>
          </Tabs>

          <Button
            type="button"
            onClick={() => handleLaunchSession()}
            variant="ghost"
            size="icon-sm"
            className="chrome-tab-new-button mb-[4px] ml-1 h-7 w-7 shrink-0 rounded-full"
            title="New session"
          >
            <Plus className="h-4 w-4" strokeWidth={2} />
          </Button>
        </div>
      </div>
    </div>
  )
}
