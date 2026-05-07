import { STORAGE_KEYS } from "../constants/storage"

export type ColorScheme = "system" | "light" | "dark"
export type ThemeMode = Exclude<ColorScheme, "system">

type ThemeVariables = Record<`--${string}`, string>

export interface AppearanceTheme {
  id: string
  name: string
  swatches: string[]
  modes: Record<ThemeMode, ThemeVariables>
}

export const DEFAULT_APPEARANCE_THEME_ID = "oc-2"
export const DEFAULT_COLOR_SCHEME: ColorScheme = "system"
export const THEME_CHANGED_EVENT = "shob-theme-change"

const oc2Light: ThemeVariables = {
  "--background": "#f8f8f8",
  "--foreground": "#171717",
  "--card": "#ffffff",
  "--popover": "#ffffff",
  "--primary": "#171717",
  "--primary-foreground": "#f8f8f8",
  "--secondary": "#ededed",
  "--muted": "#f0f0f0",
  "--muted-foreground": "#6f6f6f",
  "--accent": "#e5e5e5",
  "--border": "#e5e5e5",
  "--input": "#fcfcfc",
  "--ring": "#034cff",
  "--destructive": "#fc533a",
  "--success": "#2dba26",
  "--warning": "#d68c27",
  "--info": "#2090f5",
  "--sidebar": "#f3f3f3",
  "--sidebar-border": "#e5e5e5",
  "--sidebar-accent": "#ffffff",
  "--chrome": "#fcfcfc",
  "--chrome-button": "#f3f3f3",
  "--term-bg": "#ffffff",
  "--term-foreground": "#171717",
  "--term-cursor": "#171717",
  "--term-selection": "rgba(3, 76, 255, 0.16)",
  "--ansi-black": "#171717",
  "--ansi-bright-black": "#8f8f8f",
  "--ansi-red": "#ed4831",
  "--ansi-bright-red": "#fc806a",
  "--ansi-green": "#2dba26",
  "--ansi-bright-green": "#12c905",
  "--ansi-yellow": "#b0851f",
  "--ansi-bright-yellow": "#d68c27",
  "--ansi-blue": "#034cff",
  "--ansi-bright-blue": "#2090f5",
  "--ansi-magenta": "#a753ae",
  "--ansi-bright-magenta": "#ed6dc8",
  "--ansi-cyan": "#007b80",
  "--ansi-bright-cyan": "#0092a8",
  "--ansi-white": "#f8f8f8",
  "--ansi-bright-white": "#ffffff",
}

const oc2Dark: ThemeVariables = {
  "--background": "#101010",
  "--foreground": "rgba(255, 255, 255, 0.936)",
  "--card": "#161616",
  "--popover": "#1c1c1c",
  "--primary": "#ededed",
  "--primary-foreground": "#101010",
  "--secondary": "#1c1c1c",
  "--muted": "#202020",
  "--muted-foreground": "rgba(255, 255, 255, 0.422)",
  "--accent": "#282828",
  "--border": "#282828",
  "--input": "#1c1c1c",
  "--ring": "#9dbefe",
  "--destructive": "#fc533a",
  "--success": "#12c905",
  "--warning": "#fbb73c",
  "--info": "#93e9f6",
  "--sidebar": "#151515",
  "--sidebar-border": "#282828",
  "--sidebar-accent": "#282828",
  "--chrome": "#121212",
  "--chrome-button": "#1c1c1c",
  "--term-bg": "#101010",
  "--term-foreground": "#eeeeee",
  "--term-cursor": "#eeeeee",
  "--term-selection": "rgba(255, 255, 255, 0.16)",
  "--ansi-black": "#101010",
  "--ansi-bright-black": "#707070",
  "--ansi-red": "#fc533a",
  "--ansi-bright-red": "#faa494",
  "--ansi-green": "#12c905",
  "--ansi-bright-green": "#96ec8e",
  "--ansi-yellow": "#fbb73c",
  "--ansi-bright-yellow": "#fcd53a",
  "--ansi-blue": "#9dbefe",
  "--ansi-bright-blue": "#2090f5",
  "--ansi-magenta": "#edb2f1",
  "--ansi-bright-magenta": "#ff9ae2",
  "--ansi-cyan": "#93e9f6",
  "--ansi-bright-cyan": "#56b6c2",
  "--ansi-white": "#ededed",
  "--ansi-bright-white": "#ffffff",
}

const theme = (
  id: string,
  name: string,
  swatches: string[],
  light: ThemeVariables,
  dark: ThemeVariables,
): AppearanceTheme => ({
  id,
  name,
  swatches,
  modes: { light, dark },
})

export const APPEARANCE_THEMES = [
  theme("oc-2", "OC-2", ["#101010", "#282828", "#9dbefe", "#fab283"], oc2Light, oc2Dark),
  theme(
    "opencode",
    "OpenCode",
    ["#0f0f0f", "#23201d", "#fab283", "#12c905"],
    {
      ...oc2Light,
      "--background": "#faf7f2",
      "--card": "#fffdf8",
      "--muted": "#f0e8dc",
      "--accent": "#f3dfc8",
      "--ring": "#c86d1f",
      "--warning": "#b96d20",
      "--sidebar": "#f2eadf",
      "--chrome": "#fffaf3",
      "--chrome-button": "#f0e6da",
      "--term-bg": "#fffdf8",
      "--ansi-yellow": "#b96d20",
      "--ansi-bright-yellow": "#d68c27",
    },
    {
      ...oc2Dark,
      "--background": "#0f0f0f",
      "--card": "#171411",
      "--muted": "#24211e",
      "--accent": "#2e2924",
      "--ring": "#fab283",
      "--warning": "#fab283",
      "--sidebar": "#141210",
      "--sidebar-accent": "#2b2620",
      "--chrome": "#12100e",
      "--chrome-button": "#201c18",
      "--term-bg": "#0f0f0f",
      "--ansi-yellow": "#fab283",
      "--ansi-bright-yellow": "#ffd2a8",
    },
  ),
  theme(
    "codex",
    "Codex",
    ["#0b0f0d", "#18362c", "#3dd68c", "#58a6ff"],
    {
      ...oc2Light,
      "--background": "#f7faf8",
      "--card": "#ffffff",
      "--muted": "#e9f3ee",
      "--accent": "#dcefe6",
      "--ring": "#13795b",
      "--success": "#13795b",
      "--info": "#1f6feb",
      "--sidebar": "#edf6f1",
      "--chrome": "#fbfefc",
      "--chrome-button": "#e8f3ed",
      "--ansi-green": "#13795b",
      "--ansi-bright-green": "#2da66d",
    },
    {
      ...oc2Dark,
      "--background": "#0b0f0d",
      "--card": "#111815",
      "--muted": "#17221d",
      "--accent": "#1d362c",
      "--ring": "#3dd68c",
      "--success": "#3dd68c",
      "--info": "#58a6ff",
      "--sidebar": "#0f1512",
      "--sidebar-accent": "#1b2d25",
      "--chrome": "#0d120f",
      "--chrome-button": "#15201b",
      "--term-bg": "#0b0f0d",
      "--ansi-green": "#3dd68c",
      "--ansi-bright-green": "#72f0ad",
    },
  ),
  theme(
    "catppuccin",
    "Catppuccin",
    ["#1e1e2e", "#313244", "#89b4fa", "#f5c2e7"],
    {
      ...oc2Light,
      "--background": "#eff1f5",
      "--foreground": "#4c4f69",
      "--card": "#ffffff",
      "--muted": "#e6e9ef",
      "--muted-foreground": "#6c6f85",
      "--accent": "#dce0e8",
      "--border": "#ccd0da",
      "--ring": "#1e66f5",
      "--destructive": "#d20f39",
      "--success": "#40a02b",
      "--warning": "#df8e1d",
      "--info": "#04a5e5",
      "--sidebar": "#e6e9ef",
      "--chrome": "#eff1f5",
      "--chrome-button": "#dce0e8",
      "--term-bg": "#eff1f5",
      "--term-foreground": "#4c4f69",
      "--term-cursor": "#4c4f69",
    },
    {
      ...oc2Dark,
      "--background": "#1e1e2e",
      "--foreground": "#cdd6f4",
      "--card": "#242438",
      "--muted": "#313244",
      "--muted-foreground": "#a6adc8",
      "--accent": "#45475a",
      "--border": "#313244",
      "--ring": "#89b4fa",
      "--destructive": "#f38ba8",
      "--success": "#a6e3a1",
      "--warning": "#f9e2af",
      "--info": "#89dceb",
      "--sidebar": "#181825",
      "--sidebar-accent": "#313244",
      "--chrome": "#181825",
      "--chrome-button": "#313244",
      "--term-bg": "#1e1e2e",
      "--term-foreground": "#cdd6f4",
      "--term-cursor": "#f5e0dc",
      "--ansi-blue": "#89b4fa",
      "--ansi-magenta": "#f5c2e7",
      "--ansi-cyan": "#89dceb",
    },
  ),
  theme(
    "tokyonight",
    "Tokyo Night",
    ["#1a1b26", "#24283b", "#7aa2f7", "#bb9af7"],
    {
      ...oc2Light,
      "--background": "#d5d6db",
      "--foreground": "#343b58",
      "--card": "#e6e7ed",
      "--muted": "#c9ccd8",
      "--muted-foreground": "#5a6080",
      "--accent": "#bfc4d8",
      "--border": "#b9bfd3",
      "--ring": "#2e7de9",
      "--sidebar": "#cfd2de",
      "--chrome": "#e1e2e8",
      "--chrome-button": "#c9ccd8",
      "--term-bg": "#d5d6db",
      "--term-foreground": "#343b58",
      "--term-cursor": "#343b58",
    },
    {
      ...oc2Dark,
      "--background": "#1a1b26",
      "--foreground": "#c0caf5",
      "--card": "#1f2335",
      "--muted": "#24283b",
      "--muted-foreground": "#9aa5ce",
      "--accent": "#292e42",
      "--border": "#2f3549",
      "--ring": "#7aa2f7",
      "--success": "#9ece6a",
      "--warning": "#e0af68",
      "--info": "#7dcfff",
      "--sidebar": "#16161e",
      "--sidebar-accent": "#24283b",
      "--chrome": "#16161e",
      "--chrome-button": "#24283b",
      "--term-bg": "#1a1b26",
      "--term-foreground": "#c0caf5",
      "--term-cursor": "#c0caf5",
      "--ansi-blue": "#7aa2f7",
      "--ansi-magenta": "#bb9af7",
      "--ansi-cyan": "#7dcfff",
    },
  ),
  theme(
    "github",
    "GitHub",
    ["#ffffff", "#d0d7de", "#0969da", "#1a7f37"],
    {
      ...oc2Light,
      "--background": "#ffffff",
      "--foreground": "#1f2328",
      "--card": "#f6f8fa",
      "--muted": "#f6f8fa",
      "--muted-foreground": "#656d76",
      "--accent": "#eaeef2",
      "--border": "#d0d7de",
      "--ring": "#0969da",
      "--success": "#1a7f37",
      "--warning": "#9a6700",
      "--sidebar": "#f6f8fa",
      "--chrome": "#ffffff",
      "--chrome-button": "#f6f8fa",
      "--term-bg": "#ffffff",
      "--term-foreground": "#1f2328",
      "--term-cursor": "#1f2328",
    },
    {
      ...oc2Dark,
      "--background": "#0d1117",
      "--foreground": "#f0f6fc",
      "--card": "#161b22",
      "--muted": "#21262d",
      "--muted-foreground": "#8b949e",
      "--accent": "#30363d",
      "--border": "#30363d",
      "--ring": "#2f81f7",
      "--success": "#3fb950",
      "--warning": "#d29922",
      "--info": "#58a6ff",
      "--sidebar": "#010409",
      "--sidebar-accent": "#21262d",
      "--chrome": "#010409",
      "--chrome-button": "#21262d",
      "--term-bg": "#0d1117",
      "--term-foreground": "#f0f6fc",
      "--term-cursor": "#f0f6fc",
    },
  ),
  theme(
    "vercel",
    "Vercel",
    ["#000000", "#1d1d1d", "#fafafa", "#888888"],
    {
      ...oc2Light,
      "--background": "#ffffff",
      "--foreground": "#171717",
      "--card": "#fafafa",
      "--muted": "#f5f5f5",
      "--muted-foreground": "#666666",
      "--accent": "#eeeeee",
      "--border": "#e5e5e5",
      "--ring": "#000000",
      "--sidebar": "#fafafa",
      "--chrome": "#ffffff",
      "--chrome-button": "#f5f5f5",
      "--term-bg": "#ffffff",
      "--term-foreground": "#171717",
      "--term-cursor": "#171717",
    },
    {
      ...oc2Dark,
      "--background": "#000000",
      "--foreground": "#fafafa",
      "--card": "#111111",
      "--muted": "#1d1d1d",
      "--muted-foreground": "#888888",
      "--accent": "#2a2a2a",
      "--border": "#242424",
      "--ring": "#fafafa",
      "--sidebar": "#0a0a0a",
      "--sidebar-accent": "#1d1d1d",
      "--chrome": "#000000",
      "--chrome-button": "#111111",
      "--term-bg": "#000000",
      "--term-foreground": "#fafafa",
      "--term-cursor": "#fafafa",
    },
  ),
] as const satisfies readonly AppearanceTheme[]

export type AppearanceThemeId = (typeof APPEARANCE_THEMES)[number]["id"]

const THEME_BY_ID = new Map(APPEARANCE_THEMES.map((item) => [item.id, item]))

export function normalizeAppearanceThemeId(value: string | null | undefined): AppearanceThemeId {
  return THEME_BY_ID.has(value ?? "") ? (value as AppearanceThemeId) : DEFAULT_APPEARANCE_THEME_ID
}

export function normalizeColorScheme(value: string | null | undefined): ColorScheme {
  return value === "light" || value === "dark" || value === "system" ? value : DEFAULT_COLOR_SCHEME
}

export function getAppearanceTheme(themeId: string | null | undefined): AppearanceTheme {
  return THEME_BY_ID.get(normalizeAppearanceThemeId(themeId)) ?? APPEARANCE_THEMES[0]
}

export function getSystemThemeMode(): ThemeMode {
  if (typeof window === "undefined") return "dark"
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"
}

export function resolveColorScheme(colorScheme: ColorScheme): ThemeMode {
  return colorScheme === "system" ? getSystemThemeMode() : colorScheme
}

function completeVariables(input: ThemeVariables, mode: ThemeMode): ThemeVariables {
  const vars: ThemeVariables = {
    "--card-foreground": input["--foreground"],
    "--popover-foreground": input["--foreground"],
    "--secondary-foreground": input["--foreground"],
    "--accent-foreground": input["--foreground"],
    "--sidebar-foreground": input["--foreground"],
    "--sidebar-primary": input["--ring"],
    "--sidebar-primary-foreground": input["--primary-foreground"],
    "--sidebar-ring": input["--ring"],
    "--chrome-foreground": input["--foreground"],
    "--chrome-muted": input["--muted-foreground"],
    "--chrome-border": input["--border"],
    "--chrome-border-hover": input["--muted-foreground"],
    "--chrome-button-hover": input["--accent"],
    "--chart-1": input["--ring"],
    "--chart-2": input["--success"],
    "--chart-3": input["--warning"],
    "--chart-4": input["--info"],
    "--chart-5": input["--destructive"],
    "--tab-strip-bg": "color-mix(in oklch, var(--background) 76%, var(--muted) 24%)",
    "--tab-active-bg": "color-mix(in oklch, var(--card) 92%, var(--background) 8%)",
    "--tab-active-border": "color-mix(in oklch, var(--border) 64%, var(--foreground) 36%)",
    "--tab-inactive-text": "color-mix(in oklch, var(--muted-foreground) 88%, var(--foreground) 12%)",
    "--tab-hover-text": "color-mix(in oklch, var(--foreground) 88%, var(--muted-foreground) 12%)",
    "--tab-hover-bg": "color-mix(in oklch, var(--accent) 80%, var(--card) 20%)",
    "--tab-divider": "color-mix(in oklch, var(--border) 60%, transparent)",
    "--tab-toolbar-divider": "color-mix(in oklch, var(--border) 78%, transparent)",
    "--tab-button-hover-bg": "color-mix(in oklch, var(--accent) 78%, var(--card) 22%)",
    "--tab-button-active-bg": "color-mix(in oklch, var(--accent) 90%, var(--background) 10%)",
    "--term-scrollbar": mode === "dark" ? "rgba(255, 255, 255, 0.18)" : "rgba(0, 0, 0, 0.2)",
    "--term-scrollbar-hover": mode === "dark" ? "rgba(255, 255, 255, 0.32)" : "rgba(0, 0, 0, 0.34)",
    "--term-scrollbar-active": mode === "dark" ? "rgba(255, 255, 255, 0.46)" : "rgba(0, 0, 0, 0.48)",
    "--term-glow-color": "color-mix(in oklch, var(--ring) 12%, transparent)",
    "--term-cursor-accent": input["--term-bg"],
    "--term-selection-foreground": input["--term-foreground"],
  }

  return { ...vars, ...input }
}

function writeStorage(key: string, value: string) {
  if (typeof window === "undefined") return
  window.localStorage.setItem(key, value)
}

function readStorage(key: string) {
  if (typeof window === "undefined") return null
  return window.localStorage.getItem(key)
}

export function applyAppearanceTheme(themeId: string, colorScheme: ColorScheme) {
  if (typeof document === "undefined") return

  const theme = getAppearanceTheme(themeId)
  const mode = resolveColorScheme(colorScheme)
  const root = document.documentElement
  const variables = completeVariables(theme.modes[mode], mode)

  Object.entries(variables).forEach(([key, value]) => {
    root.style.setProperty(key, value)
  })

  root.classList.toggle("dark", mode === "dark")
  root.dataset.theme = theme.id
  root.dataset.colorScheme = mode
  root.dataset.preferredColorScheme = colorScheme
  root.style.colorScheme = mode

  window.dispatchEvent(
    new CustomEvent(THEME_CHANGED_EVENT, {
      detail: { themeId: theme.id, colorScheme, mode },
    }),
  )
}

export function persistAppearanceTheme(themeId: AppearanceThemeId, colorScheme: ColorScheme) {
  writeStorage(STORAGE_KEYS.appearanceThemeId, themeId)
  writeStorage(STORAGE_KEYS.colorScheme, colorScheme)
  applyAppearanceTheme(themeId, colorScheme)
}

export function applyStoredAppearanceTheme() {
  applyAppearanceTheme(
    normalizeAppearanceThemeId(readStorage(STORAGE_KEYS.appearanceThemeId)),
    normalizeColorScheme(readStorage(STORAGE_KEYS.colorScheme)),
  )
}

const cssVar = (name: `--${string}`, fallback: string) => {
  if (typeof window === "undefined") return fallback
  const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  return value || fallback
}

export function getTerminalTheme() {
  return {
    background: cssVar("--term-bg", "#101010"),
    foreground: cssVar("--term-foreground", "#eeeeee"),
    cursor: cssVar("--term-cursor", "#eeeeee"),
    cursorAccent: cssVar("--term-cursor-accent", "#101010"),
    selectionBackground: cssVar("--term-selection", "rgba(255, 255, 255, 0.16)"),
    selectionForeground: cssVar("--term-selection-foreground", "#eeeeee"),
    black: cssVar("--ansi-black", "#101010"),
    brightBlack: cssVar("--ansi-bright-black", "#707070"),
    red: cssVar("--ansi-red", "#fc533a"),
    brightRed: cssVar("--ansi-bright-red", "#faa494"),
    green: cssVar("--ansi-green", "#12c905"),
    brightGreen: cssVar("--ansi-bright-green", "#96ec8e"),
    yellow: cssVar("--ansi-yellow", "#fbb73c"),
    brightYellow: cssVar("--ansi-bright-yellow", "#fcd53a"),
    blue: cssVar("--ansi-blue", "#9dbefe"),
    brightBlue: cssVar("--ansi-bright-blue", "#2090f5"),
    magenta: cssVar("--ansi-magenta", "#edb2f1"),
    brightMagenta: cssVar("--ansi-bright-magenta", "#ff9ae2"),
    cyan: cssVar("--ansi-cyan", "#93e9f6"),
    brightCyan: cssVar("--ansi-bright-cyan", "#56b6c2"),
    white: cssVar("--ansi-white", "#ededed"),
    brightWhite: cssVar("--ansi-bright-white", "#ffffff"),
  }
}
