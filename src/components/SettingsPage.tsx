import { useMemo, useState, type ReactNode } from "react"
import { Boxes, Check, ChevronDown, Monitor, Moon, Palette, SlidersHorizontal, Sun, Terminal } from "lucide-react"
import { CliAvatar } from "./CliAvatar"
import { useStore, type SettingsSection } from "../store"
import {
  APPEARANCE_THEMES,
  type AppearanceThemeId,
  type ColorScheme,
} from "../theme"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { OpenCodeProvidersSettings } from "@/components/opencode/OpenCodeProvidersSettings"

const SETTINGS_SECTIONS: {
  id: SettingsSection
  label: string
  icon: typeof SlidersHorizontal
}[] = [
  { id: "general", label: "General", icon: SlidersHorizontal },
  { id: "appearance", label: "Appearance", icon: Palette },
  { id: "providers", label: "Providers", icon: Boxes },
  { id: "cli-tools", label: "CLI Tools", icon: Terminal },
]

const COLOR_SCHEME_OPTIONS: {
  value: ColorScheme
  label: string
  icon: typeof Monitor
}[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
]

const getShellLabel = (shell: string) => {
  const name = shell.split(/[\\/]/).pop()
  return name || shell
}

function SettingsRow({
  title,
  description,
  children,
}: {
  title: ReactNode
  description?: ReactNode
  children: ReactNode
}) {
  return (
    <div className="grid gap-3 border-b border-[var(--border)] px-4 py-3 last:border-b-0 md:grid-cols-[minmax(0,1fr)_minmax(260px,360px)] md:items-center">
      <div className="min-w-0">
        <div className="text-sm font-medium text-[var(--foreground)]">{title}</div>
        {description ? <div className="mt-1 text-xs text-[var(--muted-foreground)]">{description}</div> : null}
      </div>
      <div className="min-w-0">{children}</div>
    </div>
  )
}

function SettingsBlock({ title, children }: { title: string; children: ReactNode }) {
  return (
    <Card className="before:hidden">
      <CardHeader className="border-b border-[var(--border)] pb-2">
        <CardTitle className="text-sm">{title}</CardTitle>
      </CardHeader>
      <CardContent className="p-0">{children}</CardContent>
    </Card>
  )
}

function GeneralSettings() {
  const {
    preferredCliId,
    preferredShell,
    cliTools,
    availableShells,
    setPreferredCliTool,
    setPreferredShell,
  } = useStore()
  const installedCliTools = useMemo(() => cliTools.filter((tool) => tool.installed), [cliTools])

  return (
    <SettingsBlock title="General">
      <SettingsRow title="Default CLI" description="Used for newly-created sessions.">
        <div className="flex items-center gap-2">
          <span className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-[var(--muted)]">
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
            <SelectTrigger className="h-8 w-full">
              <SelectValue placeholder="Select CLI" />
            </SelectTrigger>
            <SelectContent className="p-1">
              {installedCliTools.length === 0 ? (
                <SelectItem className="py-1" value="" disabled>
                  No CLI tools detected
                </SelectItem>
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
      </SettingsRow>

      <SettingsRow title="Default Shell" description="Used for new terminal panels.">
        <Select
          value={preferredShell ?? availableShells[0] ?? ""}
          onValueChange={(value) => setPreferredShell(value || null)}
        >
          <SelectTrigger className="h-8 w-full">
            <SelectValue placeholder="Select shell" />
          </SelectTrigger>
          <SelectContent className="p-1">
            {availableShells.length === 0 ? (
              <SelectItem className="py-1" value="" disabled>
                No shells detected
              </SelectItem>
            ) : (
              availableShells.map((shell) => (
                <SelectItem className="py-1" key={shell} value={shell}>
                  {getShellLabel(shell)}
                </SelectItem>
              ))
            )}
          </SelectContent>
        </Select>
      </SettingsRow>
    </SettingsBlock>
  )
}

function AppearanceSettings() {
  const { appearanceThemeId, colorScheme, setAppearanceTheme, setColorScheme } = useStore()
  const activeTheme = APPEARANCE_THEMES.find((theme) => theme.id === appearanceThemeId) ?? APPEARANCE_THEMES[0]
  const [themePickerOpen, setThemePickerOpen] = useState(false)

  return (
    <div className="space-y-4">
      <SettingsBlock title="Appearance">
        <SettingsRow title="Color Scheme" description="Choose the base light or dark mode.">
          <Tabs value={colorScheme} onValueChange={(value) => setColorScheme(value as ColorScheme)}>
            <TabsList className="w-full" variant="default">
              {COLOR_SCHEME_OPTIONS.map((option) => {
                const Icon = option.icon
                return (
                  <TabsTrigger key={option.value} value={option.value} className="h-7 text-xs">
                    <Icon className="h-3.5 w-3.5" strokeWidth={1.9} />
                    <span>{option.label}</span>
                  </TabsTrigger>
                )
              })}
            </TabsList>
          </Tabs>
        </SettingsRow>

        <SettingsRow title="Theme" description="Theme tokens are applied across chrome, panels, and terminals.">
          <Popover open={themePickerOpen} onOpenChange={setThemePickerOpen}>
            <PopoverTrigger asChild>
              <Button
                type="button"
                variant="outline"
                className="h-8 w-full justify-between px-2.5 text-sm"
              >
                <span className="truncate">{activeTheme.name}</span>
                <ChevronDown className="h-4 w-4 text-[var(--muted-foreground)]" />
              </Button>
            </PopoverTrigger>
            <PopoverContent align="start" className="w-[var(--radix-popover-trigger-width)] p-1.5">
              <div className="max-h-64 overflow-y-auto">
                {APPEARANCE_THEMES.map((theme) => {
                  const isActive = theme.id === appearanceThemeId
                  return (
                    <button
                      key={theme.id}
                      type="button"
                      onClick={() => {
                        setAppearanceTheme(theme.id as AppearanceThemeId)
                        setThemePickerOpen(false)
                      }}
                      className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm transition ${
                        isActive ? "bg-[var(--accent)] text-[var(--foreground)]" : "hover:bg-[var(--muted)]"
                      }`}
                    >
                      <span className="grid grid-cols-4 gap-0.5">
                        {theme.swatches.slice(0, 4).map((swatch) => (
                          <span
                            key={`${theme.id}-${swatch}`}
                            className="h-2.5 w-2.5 rounded-[3px] border border-[var(--border)]"
                            style={{ background: swatch }}
                          />
                        ))}
                      </span>
                      <span className="min-w-0 flex-1 truncate">{theme.name}</span>
                      {isActive ? <Check className="h-3.5 w-3.5 shrink-0 text-[var(--ring)]" /> : null}
                    </button>
                  )
                })}
              </div>
            </PopoverContent>
          </Popover>
        </SettingsRow>
        <SettingsRow title="Preview" description="Colors from the currently selected theme.">
          <div className="grid grid-cols-4 gap-1.5">
            {activeTheme.swatches.map((swatch) => (
              <span
                key={`${activeTheme.id}-${swatch}`}
                className="h-7 rounded-md border border-[var(--border)]"
                style={{ background: swatch }}
              />
            ))}
          </div>
        </SettingsRow>
      </SettingsBlock>

    </div>
  )
}

function ProvidersSettings() {
  return <OpenCodeProvidersSettings />
}
function CliToolsSettings() {
  const { cliTools, installCliTool } = useStore()
  const [cliToolSearchQuery, setCliToolSearchQuery] = useState("")
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

  return (
    <div className="flex min-h-0 flex-col gap-3">
      <div className="flex items-center gap-2">
        <Input
          value={cliToolSearchQuery}
          onChange={(event) => setCliToolSearchQuery(event.target.value)}
          placeholder="Search tools, status, command..."
          className="h-8"
        />
        {cliToolSearchQuery.trim() ? (
          <Button type="button" size="sm" variant="ghost" onClick={() => setCliToolSearchQuery("")}>
            Clear
          </Button>
        ) : null}
      </div>

      <Card className="min-h-0 overflow-hidden before:hidden">
        <div className="thin-scrollbar max-h-[540px] overflow-y-auto">
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
                      <span className="text-success">Installed</span>
                    ) : (
                      <span className="text-[var(--muted-foreground)]">Not installed</span>
                    )}
                  </TableCell>
                  <TableCell className="text-right">
                    {tool.installed ? (
                      <span className="text-sm text-[var(--muted-foreground)]">Ready</span>
                    ) : (
                      <Button
                        size="sm"
                        variant="outline"
                        onClick={() => void installCliTool(tool.id, tool.installCommand)}
                      >
                        Install
                      </Button>
                    )}
                  </TableCell>
                </TableRow>
              ))}
              {filteredCliTools.length === 0 && (
                <TableRow>
                  <TableCell colSpan={3} className="py-8 text-center text-sm text-[var(--muted-foreground)]">
                    No tools found for "{cliToolSearchQuery.trim()}".
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      </Card>
    </div>
  )
}

export function SettingsPage() {
  const activeSettingsSection = useStore((state) => state.activeSettingsSection)
  const setActiveSettingsSection = useStore((state) => state.setActiveSettingsSection)
  const setActivePage = useStore((state) => state.setActivePage)
  const activeSection = SETTINGS_SECTIONS.find((section) => section.id === activeSettingsSection) ?? SETTINGS_SECTIONS[0]

  return (
    <div className="flex h-full min-h-0 bg-[var(--background)] text-[var(--foreground)]">
      <aside className="hidden w-[236px] shrink-0 border-r border-[var(--border)] bg-[var(--card)] md:flex md:flex-col">
        <div className="border-b border-[var(--border)] px-4 py-3">
          <p className="text-sm font-medium">Settings</p>
        </div>
        <nav className="flex flex-col gap-1 p-2">
          {SETTINGS_SECTIONS.map((section) => {
            const Icon = section.icon
            const isActive = activeSettingsSection === section.id

            return (
              <Button
                key={section.id}
                type="button"
                variant="ghost"
                size="default"
                onClick={() => setActiveSettingsSection(section.id)}
                className={`h-8 w-full justify-start gap-2 px-2 text-left ${
                  isActive ? "bg-[var(--accent)] text-[var(--foreground)]" : "text-[var(--muted-foreground)]"
                }`}
              >
                <Icon className="h-4 w-4 shrink-0" strokeWidth={1.9} />
                <span>{section.label}</span>
              </Button>
            )
          })}
        </nav>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center gap-3 border-b border-[var(--border)] px-4 md:px-6">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 px-2 text-[var(--muted-foreground)] hover:text-[var(--foreground)]"
            onClick={() => setActivePage("workspace")}
          >
            Workspace
          </Button>
          <div className="h-4 w-px bg-[var(--border)]" />
          <h1 className="truncate text-sm font-medium">{activeSection.label}</h1>
        </header>

        <div className="border-b border-[var(--border)] bg-[var(--card)] px-3 py-2 md:hidden">
          <Tabs value={activeSettingsSection} onValueChange={(value) => setActiveSettingsSection(value as SettingsSection)}>
            <TabsList variant="line" className="thin-scrollbar w-full justify-start overflow-x-auto">
              {SETTINGS_SECTIONS.map((section) => {
                const Icon = section.icon
                return (
                  <TabsTrigger key={section.id} value={section.id} className="h-8 shrink-0 gap-2 px-3 text-sm">
                    <Icon className="h-4 w-4" strokeWidth={1.9} />
                    <span>{section.label}</span>
                  </TabsTrigger>
                )
              })}
            </TabsList>
          </Tabs>
        </div>

        <main className="thin-scrollbar min-h-0 flex-1 overflow-y-auto px-4 py-4 md:px-6">
          <div className="mx-auto w-full max-w-5xl">
            {activeSettingsSection === "general" && <GeneralSettings />}
            {activeSettingsSection === "appearance" && <AppearanceSettings />}
            {activeSettingsSection === "providers" && <ProvidersSettings />}
            {activeSettingsSection === "cli-tools" && <CliToolsSettings />}
          </div>
        </main>
      </div>
    </div>
  )
}
