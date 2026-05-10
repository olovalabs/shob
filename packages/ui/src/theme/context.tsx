import { useEffect, useRef, useState } from "react"
import { createSimpleContext } from "../context/helper"
import oc2ThemeJson from "./themes/oc-2.json"
import { resolveThemeVariant, themeToCss } from "./resolve"
import type { DesktopTheme } from "./types"

export type ColorScheme = "light" | "dark" | "system"

const STORAGE_KEYS = {
  THEME_ID: "opencode-theme-id",
  COLOR_SCHEME: "opencode-color-scheme",
  THEME_CSS_LIGHT: "opencode-theme-css-light",
  THEME_CSS_DARK: "opencode-theme-css-dark",
} as const

const THEME_STYLE_ID = "oc-theme"
let files: Record<string, () => Promise<{ default: DesktopTheme }>> | undefined
let ids: string[] | undefined
let known: Set<string> | undefined

function getFiles() {
  if (files) return files
  files = import.meta.glob<{ default: DesktopTheme }>("./themes/*.json")
  return files
}

function themeIDs() {
  if (ids) return ids
  ids = Object.keys(getFiles())
    .map((path) => path.slice("./themes/".length, -".json".length))
    .sort()
  return ids
}

function knownThemes() {
  if (known) return known
  known = new Set(themeIDs())
  return known
}

const names: Record<string, string> = {
  "oc-2": "OC-2",
  amoled: "AMOLED",
  aura: "Aura",
  ayu: "Ayu",
  carbonfox: "Carbonfox",
  catppuccin: "Catppuccin",
  "catppuccin-frappe": "Catppuccin Frappe",
  "catppuccin-macchiato": "Catppuccin Macchiato",
  cobalt2: "Cobalt2",
  cursor: "Cursor",
  dracula: "Dracula",
  everforest: "Everforest",
  flexoki: "Flexoki",
  github: "GitHub",
  gruvbox: "Gruvbox",
  kanagawa: "Kanagawa",
  "lucent-orng": "Lucent Orng",
  material: "Material",
  matrix: "Matrix",
  mercury: "Mercury",
  monokai: "Monokai",
  nightowl: "Night Owl",
  nord: "Nord",
  "one-dark": "One Dark",
  onedarkpro: "One Dark Pro",
  opencode: "OpenCode",
  orng: "Orng",
  "osaka-jade": "Osaka Jade",
  palenight: "Palenight",
  rosepine: "Rose Pine",
  shadesofpurple: "Shades of Purple",
  solarized: "Solarized",
  synthwave84: "Synthwave '84",
  tokyonight: "Tokyonight",
  vercel: "Vercel",
  vesper: "Vesper",
  zenburn: "Zenburn",
}
const oc2Theme = oc2ThemeJson as DesktopTheme

function normalize(id: string | null | undefined) {
  return id === "oc-1" ? "oc-2" : id
}

function read(key: string) {
  if (typeof localStorage !== "object") return null
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

function write(key: string, value: string) {
  if (typeof localStorage !== "object") return
  try {
    localStorage.setItem(key, value)
  } catch {}
}

function drop(key: string) {
  if (typeof localStorage !== "object") return
  try {
    localStorage.removeItem(key)
  } catch {}
}

function clear() {
  drop(STORAGE_KEYS.THEME_CSS_LIGHT)
  drop(STORAGE_KEYS.THEME_CSS_DARK)
}

function ensureThemeStyleElement(): HTMLStyleElement {
  const existing = document.getElementById(THEME_STYLE_ID) as HTMLStyleElement | null
  if (existing) return existing
  const element = document.createElement("style")
  element.id = THEME_STYLE_ID
  document.head.appendChild(element)
  return element
}

function getSystemMode(): "light" | "dark" {
  if (typeof window !== "object") return "light"
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"
}

function applyThemeCss(theme: DesktopTheme, themeId: string, mode: "light" | "dark") {
  const isDark = mode === "dark"
  const variant = isDark ? theme.dark : theme.light
  const tokens = resolveThemeVariant(variant, isDark)
  const css = themeToCss(tokens)

  if (themeId !== "oc-2") {
    write(isDark ? STORAGE_KEYS.THEME_CSS_DARK : STORAGE_KEYS.THEME_CSS_LIGHT, css)
  }

  const fullCss = `:root {
  color-scheme: ${mode};
  --text-mix-blend-mode: ${isDark ? "plus-lighter" : "multiply"};
  ${css}
}`

  document.getElementById("oc-theme-preload")?.remove()
  ensureThemeStyleElement().textContent = fullCss
  document.documentElement.dataset.theme = themeId
  document.documentElement.dataset.colorScheme = mode
}

function cacheThemeVariants(theme: DesktopTheme, themeId: string) {
  if (themeId === "oc-2") return
  for (const mode of ["light", "dark"] as const) {
    const isDark = mode === "dark"
    const variant = isDark ? theme.dark : theme.light
    const tokens = resolveThemeVariant(variant, isDark)
    const css = themeToCss(tokens)
    write(isDark ? STORAGE_KEYS.THEME_CSS_DARK : STORAGE_KEYS.THEME_CSS_LIGHT, css)
  }
}

export const { use: useTheme, provider: ThemeProvider } = createSimpleContext({
  name: "Theme",
  init: (props: { defaultTheme?: string; onThemeApplied?: (theme: DesktopTheme, mode: "light" | "dark") => void }) => {
    const initThemeId = normalize(read(STORAGE_KEYS.THEME_ID) ?? props.defaultTheme) ?? "oc-2"
    const initColorScheme = (read(STORAGE_KEYS.COLOR_SCHEME) as ColorScheme | null) ?? "system"
    const initMode = initColorScheme === "system" ? getSystemMode() : initColorScheme
    const [state, setState] = useState({
      themes: { "oc-2": oc2Theme } as Record<string, DesktopTheme>,
      themeId: initThemeId,
      colorScheme: initColorScheme,
      mode: initMode,
      previewThemeId: null as string | null,
      previewScheme: null as ColorScheme | null,
    })
    const stateRef = useRef(state)
    stateRef.current = state

    const loads = new Map<string, Promise<DesktopTheme | undefined>>()

    const load = (id: string) => {
      const next = normalize(id)
      if (!next) return Promise.resolve(undefined)
      const hit = state.themes[next]
      if (hit) return Promise.resolve(hit)
      const pending = loads.get(next)
      if (pending) return pending
      const file = getFiles()[`./themes/${next}.json`]
      if (!file) return Promise.resolve(undefined)
      const task = file()
        .then((mod) => {
          const theme = mod.default
          setState((prev) => ({ ...prev, themes: { ...prev.themes, [next]: theme } }))
          return theme
        })
        .finally(() => {
          loads.delete(next)
        })
      loads.set(next, task)
      return task
    }

    const applyTheme = (theme: DesktopTheme, themeId: string, mode: "light" | "dark") => {
      applyThemeCss(theme, themeId, mode)
      props.onThemeApplied?.(theme, mode)
    }

    const ids = () => {
      const extra = Object.keys(state.themes)
        .filter((id) => !knownThemes().has(id))
        .sort()
      const all = themeIDs()
      if (extra.length === 0) return all
      return [...all, ...extra]
    }

    const loadThemes = () => Promise.all(themeIDs().map(load)).then(() => state.themes)

    useEffect(() => {
      const onStorage = (e: StorageEvent) => {
        if (e.key === STORAGE_KEYS.THEME_ID && e.newValue) {
          const next = normalize(e.newValue)
          if (!next) return
          if (next !== "oc-2" && !knownThemes().has(next) && !stateRef.current.themes[next]) return
          setState((prev) => ({ ...prev, themeId: next }))
          if (next === "oc-2") {
            clear()
            return
          }
          void load(next).then((theme) => {
            if (!theme || stateRef.current.themeId !== next) return
            cacheThemeVariants(theme, next)
          })
        }
        if (e.key === STORAGE_KEYS.COLOR_SCHEME && e.newValue) {
          const scheme = e.newValue as ColorScheme
          setState((prev) => ({ ...prev, colorScheme: scheme, mode: scheme === "system" ? getSystemMode() : scheme }))
        }
      }

      window.addEventListener("storage", onStorage)

      const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)")
      const onMedia = () => {
        if (stateRef.current.colorScheme !== "system") return
        setState((prev) => ({ ...prev, mode: getSystemMode() }))
      }
      mediaQuery.addEventListener("change", onMedia)

      const rawTheme = read(STORAGE_KEYS.THEME_ID)
      const savedTheme = normalize(rawTheme ?? props.defaultTheme) ?? "oc-2"
      const savedScheme = (read(STORAGE_KEYS.COLOR_SCHEME) as ColorScheme | null) ?? "system"
      if (rawTheme && rawTheme !== savedTheme) {
        write(STORAGE_KEYS.THEME_ID, savedTheme)
        clear()
      }
      if (savedTheme !== stateRef.current.themeId) setState((prev) => ({ ...prev, themeId: savedTheme }))
      if (savedScheme !== stateRef.current.colorScheme) setState((prev) => ({ ...prev, colorScheme: savedScheme }))
      setState((prev) => ({ ...prev, mode: savedScheme === "system" ? getSystemMode() : savedScheme }))
      void load(savedTheme).then((theme) => {
        if (!theme || stateRef.current.themeId !== savedTheme) return
        cacheThemeVariants(theme, savedTheme)
      })

      return () => {
        window.removeEventListener("storage", onStorage)
        mediaQuery.removeEventListener("change", onMedia)
      }
    }, [])

    useEffect(() => {
      const theme = state.themes[state.themeId]
      if (!theme) return
      applyTheme(theme, state.themeId, state.mode)
    })

    const setTheme = (id: string) => {
      const next = normalize(id)
      if (!next) {
        console.warn(`Theme "${id}" not found`)
        return
      }
      if (next !== "oc-2" && !knownThemes().has(next) && !state.themes[next]) {
        console.warn(`Theme "${id}" not found`)
        return
      }
      setState((prev) => ({ ...prev, themeId: next }))
      if (next === "oc-2") {
        write(STORAGE_KEYS.THEME_ID, next)
        clear()
        return
      }
      void load(next).then((theme) => {
        if (!theme || stateRef.current.themeId !== next) return
        cacheThemeVariants(theme, next)
        write(STORAGE_KEYS.THEME_ID, next)
      })
    }

    const setColorScheme = (scheme: ColorScheme) => {
      setState((prev) => ({ ...prev, colorScheme: scheme, mode: scheme === "system" ? getSystemMode() : scheme }))
      write(STORAGE_KEYS.COLOR_SCHEME, scheme)
    }

    return {
      themeId: () => state.themeId,
      colorScheme: () => state.colorScheme,
      mode: () => state.mode,
      ids,
      name: (id: string) => state.themes[id]?.name ?? names[id] ?? id,
      loadThemes,
      themes: () => state.themes,
      setTheme,
      setColorScheme,
      registerTheme: (theme: DesktopTheme) => {
        setState((prev) => ({ ...prev, themes: { ...prev.themes, [theme.id]: theme } }))
      },
      previewTheme: (id: string) => {
        const next = normalize(id)
        if (!next) return
        if (next !== "oc-2" && !knownThemes().has(next) && !state.themes[next]) return
        setState((prev) => ({ ...prev, previewThemeId: next }))
        void load(next).then((theme) => {
          if (!theme || stateRef.current.previewThemeId !== next) return
          const mode = stateRef.current.previewScheme
            ? stateRef.current.previewScheme === "system"
              ? getSystemMode()
              : stateRef.current.previewScheme
            : stateRef.current.mode
          applyTheme(theme, next, mode)
        })
      },
      previewColorScheme: (scheme: ColorScheme) => {
        setState((prev) => ({ ...prev, previewScheme: scheme }))
        const mode = scheme === "system" ? getSystemMode() : scheme
        const id = stateRef.current.previewThemeId ?? stateRef.current.themeId
        void load(id).then((theme) => {
          if (!theme) return
          if ((stateRef.current.previewThemeId ?? stateRef.current.themeId) !== id) return
          if (stateRef.current.previewScheme !== scheme) return
          applyTheme(theme, id, mode)
        })
      },
      commitPreview: () => {
        if (state.previewThemeId) {
          setTheme(state.previewThemeId)
        }
        if (state.previewScheme) {
          setColorScheme(state.previewScheme)
        }
        setState((prev) => ({ ...prev, previewThemeId: null, previewScheme: null }))
      },
      cancelPreview: () => {
        setState((prev) => ({ ...prev, previewThemeId: null, previewScheme: null }))
        void load(state.themeId).then((theme) => {
          if (!theme) return
          applyTheme(theme, state.themeId, state.mode)
        })
      },
    }
  },
})
