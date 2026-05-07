import { useMemo, useState, type ReactNode } from "react"
import { Boxes, Check, Monitor, Moon, Palette, SlidersHorizontal, Sun, Terminal } from "lucide-react"
import { CliAvatar } from "./CliAvatar"
import { useStore, type SettingsSection } from "../store"
import {
  APPEARANCE_THEMES,
  type AppearanceThemeId,
  type ColorScheme,
} from "../theme"
import { Button } from "@/components/ui/button"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"

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

const TOKEN_PREVIEW_ITEMS = [
  { label: "Background", value: "var(--background)" },
  { label: "Surface", value: "var(--card)" },
  { label: "Border", value: "var(--border)" },
  { label: "Text", value: "var(--foreground)" },
  { label: "Accent", value: "var(--ring)" },
  { label: "Success", value: "var(--success)" },
  { label: "Warning", value: "var(--warning)" },
  { label: "Danger", value: "var(--destructive)" },
] as const

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
    <div className="grid gap-3 border-b border-border px-4 py-4 last:border-b-0 md:grid-cols-[minmax(0,1fr)_minmax(260px,340px)] md:items-center md:px-5">
      <div className="min-w-0">
        <div className="text-sm font-semibold text-foreground">{title}</div>
        {description ? <div className="mt-1 text-xs text-muted-foreground">{description}</div> : null}
      </div>
      <div className="min-w-0">{children}</div>
    </div>
  )
}

function SettingsGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-3">
      <h2 className="px-1 text-sm font-semibold text-foreground">{title}</h2>
      <div className="overflow-hidden rounded-lg border border-border bg-card">{children}</div>
    </section>
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
    <SettingsGroup title="General">
      <SettingsRow title="Default CLI" description="Used for newly-created sessions.">
        <div className="flex items-center gap-2">
          <span className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-muted/50">
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
          <SelectTrigger className="h-10 w-full">
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
    </SettingsGroup>
  )
}

function AppearanceSettings() {
  const { appearanceThemeId, colorScheme, setAppearanceTheme, setColorScheme } = useStore()

  return (
    <div className="space-y-6">
      <SettingsGroup title="Appearance">
        <SettingsRow title="Color Scheme" description="Choose the base light or dark mode.">
          <div className="grid grid-cols-3 rounded-lg border border-border bg-muted p-1">
            {COLOR_SCHEME_OPTIONS.map((option) => {
              const Icon = option.icon
              const isActive = colorScheme === option.value

              return (
                <button
                  key={option.value}
                  type="button"
                  onClick={() => setColorScheme(option.value)}
                  className={`flex h-9 items-center justify-center gap-2 rounded-md text-sm font-medium transition ${
                    isActive
                      ? "bg-card text-foreground shadow-sm"
                      : "text-muted-foreground hover:bg-accent hover:text-foreground"
                  }`}
                >
                  <Icon className="h-4 w-4" strokeWidth={1.9} />
                  <span>{option.label}</span>
                </button>
              )
            })}
          </div>
        </SettingsRow>

        <SettingsRow title="Theme" description="Theme tokens are applied across chrome, panels, and terminals.">
          <Select
            value={appearanceThemeId}
            onValueChange={(value) => setAppearanceTheme(value as AppearanceThemeId)}
          >
            <SelectTrigger className="h-10 w-full">
              <SelectValue placeholder="Select theme" />
            </SelectTrigger>
            <SelectContent className="p-1">
              {APPEARANCE_THEMES.map((theme) => (
                <SelectItem className="py-1" key={theme.id} value={theme.id}>
                  {theme.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </SettingsRow>
      </SettingsGroup>

      <SettingsGroup title="Theme Gallery">
        <div className="grid gap-2 p-4 sm:grid-cols-2 xl:grid-cols-3">
          {APPEARANCE_THEMES.map((theme) => {
            const isActive = appearanceThemeId === theme.id

            return (
              <button
                key={theme.id}
                type="button"
                onClick={() => setAppearanceTheme(theme.id)}
                className={`min-w-0 rounded-lg border p-3 text-left transition ${
                  isActive
                    ? "border-ring bg-accent text-foreground"
                    : "border-border bg-background hover:border-ring/55 hover:bg-accent/55"
                }`}
              >
                <div className="flex items-center justify-between gap-3">
                  <span className="truncate text-sm font-semibold">{theme.name}</span>
                  {isActive ? <Check className="h-4 w-4 shrink-0 text-ring" strokeWidth={2} /> : null}
                </div>
                <div className="mt-3 grid grid-cols-4 gap-1">
                  {theme.swatches.map((swatch) => (
                    <span
                      key={swatch}
                      className="h-7 rounded-md border border-border/70"
                      style={{ background: swatch }}
                    />
                  ))}
                </div>
              </button>
            )
          })}
        </div>
      </SettingsGroup>

      <SettingsGroup title="Theme Tokens">
        <div className="grid gap-2 p-4 sm:grid-cols-2 xl:grid-cols-4">
          {TOKEN_PREVIEW_ITEMS.map((item) => (
            <div key={item.label} className="rounded-lg border border-border bg-background p-3">
              <div className="flex items-center justify-between gap-3">
                <span className="text-xs font-medium text-muted-foreground">{item.label}</span>
                <span
                  className="h-6 w-6 shrink-0 rounded-md border border-border"
                  style={{ background: item.value }}
                />
              </div>
              <div className="mt-3 h-1.5 rounded-full" style={{ background: item.value }} />
            </div>
          ))}
        </div>
      </SettingsGroup>
    </div>
  )
}

function ProvidersSettings() {
  const { cliLaunchMode, setCliLaunchMode } = useStore()

  return (
    <SettingsGroup title="Providers">
      <SettingsRow title="Provider Switch Mode" description="Controls how launcher changes open sessions.">
        <Select
          value={cliLaunchMode}
          onValueChange={(value) => setCliLaunchMode(value === "replace-current" ? "replace-current" : "new-tab")}
        >
          <SelectTrigger className="h-10 w-full">
            <SelectValue placeholder="Select mode" />
          </SelectTrigger>
          <SelectContent className="p-1">
            <SelectItem className="py-1" value="new-tab">
              Open in new tab
            </SelectItem>
            <SelectItem className="py-1" value="replace-current">
              Replace current tab
            </SelectItem>
          </SelectContent>
        </Select>
      </SettingsRow>
    </SettingsGroup>
  )
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
    <section className="flex min-h-0 flex-col gap-4">
      <div className="flex items-center gap-2">
        <input
          value={cliToolSearchQuery}
          onChange={(event) => setCliToolSearchQuery(event.target.value)}
          placeholder="Search tools, status, command..."
          className="h-10 w-full rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none focus:border-ring focus:ring-2 focus:ring-ring/20"
        />
        {cliToolSearchQuery.trim() ? (
          <Button type="button" size="sm" variant="ghost" onClick={() => setCliToolSearchQuery("")}>
            Clear
          </Button>
        ) : null}
      </div>

      <div className="min-h-0 overflow-hidden rounded-lg border border-border bg-card">
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
  )
}

export function SettingsPage() {
  const activeSettingsSection = useStore((state) => state.activeSettingsSection)
  const setActiveSettingsSection = useStore((state) => state.setActiveSettingsSection)
  const setActivePage = useStore((state) => state.setActivePage)
  const activeSection = SETTINGS_SECTIONS.find((section) => section.id === activeSettingsSection) ?? SETTINGS_SECTIONS[0]

  return (
    <div className="flex h-full min-h-0 bg-background text-foreground">
      <aside className="hidden w-[220px] shrink-0 border-r border-border bg-card md:flex md:flex-col">
        <div className="border-b border-border px-4 py-4">
          <p className="text-sm font-semibold">Settings</p>
        </div>
        <nav className="flex flex-col gap-1 p-3">
          {SETTINGS_SECTIONS.map((section) => {
            const Icon = section.icon
            const isActive = activeSettingsSection === section.id

            return (
              <button
                key={section.id}
                type="button"
                onClick={() => setActiveSettingsSection(section.id)}
                className={`flex h-10 items-center gap-3 rounded-lg px-3 text-left text-sm font-medium transition ${
                  isActive
                    ? "bg-accent text-foreground"
                    : "text-muted-foreground hover:bg-accent/65 hover:text-foreground"
                }`}
              >
                <Icon className="h-4 w-4 shrink-0" strokeWidth={1.9} />
                <span>{section.label}</span>
              </button>
            )
          })}
        </nav>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border bg-background px-4 md:px-6">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-8 px-2 text-muted-foreground hover:text-foreground"
            onClick={() => setActivePage("workspace")}
          >
            Workspace
          </Button>
          <div className="h-4 w-px bg-border" />
          <h1 className="truncate text-base font-semibold">{activeSection.label}</h1>
        </header>

        <div className="border-b border-border bg-card px-3 py-2 md:hidden">
          <div className="thin-scrollbar flex gap-1 overflow-x-auto">
            {SETTINGS_SECTIONS.map((section) => {
              const Icon = section.icon
              const isActive = activeSettingsSection === section.id

              return (
                <button
                  key={section.id}
                  type="button"
                  onClick={() => setActiveSettingsSection(section.id)}
                  className={`flex h-9 shrink-0 items-center gap-2 rounded-md px-3 text-sm font-medium ${
                    isActive ? "bg-accent text-foreground" : "text-muted-foreground"
                  }`}
                >
                  <Icon className="h-4 w-4" strokeWidth={1.9} />
                  <span>{section.label}</span>
                </button>
              )
            })}
          </div>
        </div>

        <main className="thin-scrollbar min-h-0 flex-1 overflow-y-auto px-4 py-5 md:px-6">
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
