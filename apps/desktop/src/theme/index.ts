import { STORAGE_KEYS } from "../constants/storage"

export type ColorScheme = "system" | "light" | "dark"
export type ThemeMode = Exclude<ColorScheme, "system">

type ThemeVariables = Record<`--${string}`, string>

type HexColor = `#${string}`

interface ThemePaletteColors {
  neutral: HexColor
  ink: HexColor
  primary: HexColor
  accent?: HexColor
  success: HexColor
  warning: HexColor
  error: HexColor
  info: HexColor
  interactive?: HexColor
  diffAdd?: HexColor
  diffDelete?: HexColor
}

interface ThemeSeedColors {
  neutral: HexColor
  primary: HexColor
  success: HexColor
  warning: HexColor
  error: HexColor
  info: HexColor
  interactive: HexColor
  diffAdd: HexColor
  diffDelete: HexColor
}

type ThemeVariant = {
  palette?: ThemePaletteColors
  seeds?: ThemeSeedColors
}

interface DesktopTheme {
  id: string
  name: string
  light: ThemeVariant
  dark: ThemeVariant
}

export interface AppearanceTheme {
  id: string
  name: string
  swatches: string[]
  modes: Record<ThemeMode, ThemeVariables>
}

export const DEFAULT_APPEARANCE_THEME_ID = "oc-2"
export const DEFAULT_COLOR_SCHEME: ColorScheme = "system"
export const THEME_CHANGED_EVENT = "shob-theme-change"

const THEME_FILES = import.meta.glob("./themes/*.json", { eager: true }) as Record<string, DesktopTheme>

const hexToRgb = (hex: string) => {
  const value = hex.replace("#", "")
  const normalized = value.length === 3
    ? value.split("").map((ch) => ch + ch).join("")
    : value
  const num = Number.parseInt(normalized, 16)
  return {
    r: (num >> 16) & 255,
    g: (num >> 8) & 255,
    b: num & 255,
  }
}

const rgbToHex = (r: number, g: number, b: number) =>
  `#${[r, g, b]
    .map((value) => Math.max(0, Math.min(255, Math.round(value))).toString(16).padStart(2, "0"))
    .join("")}`

const mix = (a: string, b: string, ratio: number) => {
  const ra = Math.max(0, Math.min(1, ratio))
  const ca = hexToRgb(a)
  const cb = hexToRgb(b)
  return rgbToHex(
    ca.r * (1 - ra) + cb.r * ra,
    ca.g * (1 - ra) + cb.g * ra,
    ca.b * (1 - ra) + cb.b * ra,
  )
}

const withAlpha = (hex: string, alpha: number) => {
  const { r, g, b } = hexToRgb(hex)
  const a = Math.max(0, Math.min(1, alpha))
  return `rgba(${r}, ${g}, ${b}, ${a})`
}

const resolvePalette = (variant: ThemeVariant): ThemePaletteColors => {
  if (variant.palette) {
    return {
      neutral: variant.palette.neutral,
      ink: variant.palette.ink,
      primary: variant.palette.primary,
      accent: variant.palette.accent ?? variant.palette.info,
      success: variant.palette.success,
      warning: variant.palette.warning,
      error: variant.palette.error,
      info: variant.palette.info,
      interactive: variant.palette.interactive ?? variant.palette.primary,
      diffAdd: variant.palette.diffAdd,
      diffDelete: variant.palette.diffDelete,
    }
  }

  const seeds = variant.seeds
  if (!seeds) {
    return {
      neutral: "#101010",
      ink: "#eeeeee",
      primary: "#9dbefe",
      accent: "#fab283",
      success: "#12c905",
      warning: "#fbb73c",
      error: "#fc533a",
      info: "#93e9f6",
      interactive: "#9dbefe",
      diffAdd: "#96ec8e",
      diffDelete: "#faa494",
    }
  }

  return {
    neutral: seeds.neutral,
    ink: seeds.info,
    primary: seeds.primary,
    accent: seeds.info,
    success: seeds.success,
    warning: seeds.warning,
    error: seeds.error,
    info: seeds.info,
    interactive: seeds.interactive,
    diffAdd: seeds.diffAdd,
    diffDelete: seeds.diffDelete,
  }
}

const variantToVariables = (variant: ThemeVariant, mode: ThemeMode): ThemeVariables => {
  const palette = resolvePalette(variant)
  const isDark = mode === "dark"
  const neutral = palette.neutral
  const ink = palette.ink
  const primary = palette.primary
  const accent = palette.accent ?? palette.info
  const success = palette.success
  const warning = palette.warning
  const error = palette.error
  const info = palette.info
  const diffAdd = palette.diffAdd ?? success
  const diffDelete = palette.diffDelete ?? error

  const card = isDark ? mix(neutral, "#ffffff", 0.06) : mix(neutral, "#000000", 0.04)
  const muted = isDark ? mix(neutral, "#ffffff", 0.11) : mix(neutral, "#000000", 0.08)
  const accentBg = isDark ? mix(neutral, accent, 0.2) : mix(neutral, accent, 0.16)
  const border = isDark ? mix(neutral, "#ffffff", 0.16) : mix(neutral, "#000000", 0.12)
  const input = isDark ? mix(neutral, "#ffffff", 0.08) : mix(neutral, "#000000", 0.05)
  const mutedText = isDark ? mix(ink, neutral, 0.5) : mix(ink, neutral, 0.45)

  return {
    "--background": neutral,
    "--foreground": ink,
    "--card": card,
    "--popover": card,
    "--primary": primary,
    "--primary-foreground": isDark ? "#101010" : "#ffffff",
    "--secondary": muted,
    "--muted": muted,
    "--muted-foreground": mutedText,
    "--accent": accentBg,
    "--border": border,
    "--input": input,
    "--ring": primary,
    "--destructive": error,
    "--success": success,
    "--warning": warning,
    "--info": info,
    "--sidebar": isDark ? mix(neutral, "#ffffff", 0.03) : mix(neutral, "#000000", 0.03),
    "--sidebar-border": border,
    "--sidebar-accent": accentBg,
    "--chrome": isDark ? mix(neutral, "#ffffff", 0.02) : mix(neutral, "#000000", 0.02),
    "--chrome-button": muted,
    "--term-bg": neutral,
    "--term-foreground": ink,
    "--term-cursor": ink,
    "--term-selection": withAlpha(primary, isDark ? 0.24 : 0.2),
    "--ansi-black": isDark ? mix(neutral, "#000000", 0.2) : mix(neutral, "#000000", 0.5),
    "--ansi-bright-black": mutedText,
    "--ansi-red": error,
    "--ansi-bright-red": mix(error, "#ffffff", isDark ? 0.25 : 0.15),
    "--ansi-green": success,
    "--ansi-bright-green": mix(success, "#ffffff", isDark ? 0.25 : 0.15),
    "--ansi-yellow": warning,
    "--ansi-bright-yellow": mix(warning, "#ffffff", isDark ? 0.25 : 0.15),
    "--ansi-blue": primary,
    "--ansi-bright-blue": mix(primary, "#ffffff", isDark ? 0.25 : 0.15),
    "--ansi-magenta": accent,
    "--ansi-bright-magenta": mix(accent, "#ffffff", isDark ? 0.25 : 0.15),
    "--ansi-cyan": info,
    "--ansi-bright-cyan": mix(info, "#ffffff", isDark ? 0.25 : 0.15),
    "--ansi-white": ink,
    "--ansi-bright-white": isDark ? "#ffffff" : "#111111",
    "--chart-1": primary,
    "--chart-2": success,
    "--chart-3": warning,
    "--chart-4": info,
    "--chart-5": error,
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
    "--term-scrollbar": isDark ? "rgba(255, 255, 255, 0.18)" : "rgba(0, 0, 0, 0.2)",
    "--term-scrollbar-hover": isDark ? "rgba(255, 255, 255, 0.32)" : "rgba(0, 0, 0, 0.34)",
    "--term-scrollbar-active": isDark ? "rgba(255, 255, 255, 0.46)" : "rgba(0, 0, 0, 0.48)",
    "--term-glow-color": "color-mix(in oklch, var(--ring) 12%, transparent)",
    "--term-cursor-accent": neutral,
    "--term-selection-foreground": ink,
    "--diff-add": diffAdd,
    "--diff-delete": diffDelete,
  }
}

const themeNameFromFile = (path: string) => path.split("/").pop()?.replace(".json", "") ?? path

export const APPEARANCE_THEMES = Object.entries(THEME_FILES)
  .map(([path, raw]) => {
    const id = raw.id || themeNameFromFile(path)
    const darkPalette = resolvePalette(raw.dark)
    return {
      id,
      name: raw.name,
      swatches: [darkPalette.neutral, darkPalette.primary, darkPalette.accent ?? darkPalette.info, darkPalette.success],
      modes: {
        light: variantToVariables(raw.light, "light"),
        dark: variantToVariables(raw.dark, "dark"),
      },
    } satisfies AppearanceTheme
  })
  .sort((a, b) => a.name.localeCompare(b.name)) as readonly AppearanceTheme[]

export type AppearanceThemeId = (typeof APPEARANCE_THEMES)[number]["id"]

const THEME_BY_ID = new Map(APPEARANCE_THEMES.map((item) => [item.id, item]))

export function normalizeAppearanceThemeId(value: string | null | undefined): AppearanceThemeId {
  if (THEME_BY_ID.has(value ?? "")) return value as AppearanceThemeId
  if (THEME_BY_ID.has(DEFAULT_APPEARANCE_THEME_ID)) return DEFAULT_APPEARANCE_THEME_ID as AppearanceThemeId
  return APPEARANCE_THEMES[0]?.id as AppearanceThemeId
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
  const variables = theme.modes[mode]

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
