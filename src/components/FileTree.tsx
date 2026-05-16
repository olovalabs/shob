import {
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
  For,
  Show,
} from "solid-js"
import { nativeApi } from "../services/native"
import {
  Check,
  ChevronDown,
  ChevronRight,
  ChevronsUpDown,
  Copy,
  GitBranch,
  RefreshCw,
  Search,
  X,
} from "lucide-solid"
import { useStore } from "../store"
import { Button } from "@/components/ui/button"

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

interface FileTreeEntry {
  name: string
  path: string
  isDirectory: boolean
  isVirtual?: boolean
  isDeleted?: boolean
}

interface GitFileChange {
  path: string
  absolutePath: string
  status: string
  additions: number
  deletions: number
}

interface GitStatusSummary {
  repoRoot: string
  changedFiles: GitFileChange[]
}

interface ProjectFsEvent {
  projectPath: string
  paths: string[]
}

interface FileTreeProps {
  selectedFilePath: string | null
  onFileSelect: (filePath: string | null) => void
}

interface IconLookup {
  exact: Record<string, string>
  base: Record<string, string>
  normalized: Record<string, string>
  tokens: string[]
}

interface ContextMenuState {
  x: number
  y: number
  entry: FileTreeEntry
}

interface DiffCounts {
  additions: number
  deletions: number
}

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

const ICON_BASE = "/vscode-icons"
const INDENT_STEP = 12
const DIRECTORY_BASE_PADDING = 6
const FILE_BASE_PADDING = 22
const LOADING_BASE_PADDING = 34
const DEBOUNCE_FS_MS = 500
const DEBOUNCE_GIT_MS = 700

const iconPath = (name: string) => `${ICON_BASE}/${name}`
const getNodePadding = (depth: number, base: number) => `${depth * INDENT_STEP + base}px`

const SPECIAL_FILE_ICON_TOKENS: Record<string, string> = {
  ".gitignore": "git",
  ".gitattributes": "git",
  ".gitmodules": "git",
  ".gitkeep": "git",
  "package.json": "npm",
  "package-lock.json": "npm",
  "tsconfig.json": "tsconfig",
  "tsconfig.app.json": "tsconfig",
  "tsconfig.node.json": "tsconfig",
  "vite.config.ts": "vite",
  "cargo.toml": "cargo",
  "cargo.lock": "cargo",
  "readme.md": "markdown",
}

const FILE_EXT_ICON_ALIASES: Record<string, string> = {
  tsx: "reactts",
  jsx: "reactjs",
  ts: "typescript",
  js: "javascript",
  md: "markdown",
  yml: "yaml",
  jpeg: "jpg",
  htm: "html",
}

const STRICT_EXTENSION_TOKENS: Record<string, string> = {
  png: "image",
  jpg: "image",
  jpeg: "image",
  gif: "image",
  webp: "image",
  ico: "image",
  svg: "svg",
  css: "css",
  html: "html",
  htm: "html",
  js: "javascript",
  jsx: "reactjs",
  ts: "typescript",
  tsx: "reactts",
  json: "json",
  md: "markdown",
  yml: "yaml",
  yaml: "yaml",
}

// ─────────────────────────────────────────────────────────────────────────────
// Icon Utilities
// ─────────────────────────────────────────────────────────────────────────────

const toIconToken = (value: string) =>
  value
    .toLowerCase()
    .replace(/^\.+/, "")
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/_+/g, "_")
    .replace(/^_+|_+$/g, "")

const buildIconLookup = (iconNames: string[]): IconLookup => {
  const exact: Record<string, string> = {}
  const base: Record<string, string> = {}
  const normalized: Record<string, string> = {}
  const tokens: string[] = []

  for (const iconName of iconNames) {
    const match = /^file_type_(.+)\.svg$/i.exec(iconName)
    if (!match) continue
    const token = match[1].toLowerCase()
    const baseToken = token.replace(/\d+$/, "")

    if (!exact[token]) exact[token] = iconName
    if (baseToken && !base[baseToken]) base[baseToken] = iconName

    const normalizedToken = token.replace(/[^a-z0-9]+/g, "")
    if (normalizedToken && !normalized[normalizedToken]) normalized[normalizedToken] = iconName
    tokens.push(token)
  }

  return { exact, base, normalized, tokens }
}

const resolveIconFromToken = (token: string, lookup: IconLookup) => {
  const normalized = toIconToken(token)
  if (!normalized) return null
  const byExact = lookup.exact[normalized] ?? lookup.base[normalized]
  if (byExact) return byExact
  const compact = normalized.replace(/_/g, "")
  return lookup.normalized[compact] ?? null
}

const getFileIcon = (name: string, lookup: IconLookup) => {
  const lowerName = name.toLowerCase()
  const stem = lowerName.replace(/\.[^.]+$/, "")
  const parts = lowerName.split(".")
  const extension = parts.length > 1 ? parts[parts.length - 1] : ""

  if (extension) {
    const strictToken = STRICT_EXTENSION_TOKENS[extension]
    if (strictToken) {
      const strictIcon = resolveIconFromToken(strictToken, lookup)
      if (strictIcon) return iconPath(strictIcon)
    }
  }

  const candidates = new Set<string>()
  const specialToken = SPECIAL_FILE_ICON_TOKENS[lowerName]
  if (specialToken) candidates.add(specialToken)
  if (parts.length > 1) candidates.add(parts.slice(1).join("_"))
  for (const part of lowerName.split(/[^a-z0-9]+/g)) {
    if (part) candidates.add(part)
  }
  if (extension) {
    candidates.add(extension)
    const alias = FILE_EXT_ICON_ALIASES[extension]
    if (alias) candidates.add(alias)
  }
  candidates.add(stem)
  candidates.add(lowerName.replace(/\./g, "_"))

  for (const candidate of candidates) {
    const iconName = resolveIconFromToken(candidate, lookup)
    if (iconName) return iconPath(iconName)
  }

  return iconPath("default_file.svg")
}

const getFolderIcon = (isExpanded: boolean, isRoot = false) => {
  if (isRoot) {
    return iconPath(isExpanded ? "default_root_folder_opened.svg" : "default_root_folder.svg")
  }
  return iconPath(isExpanded ? "default_folder_opened.svg" : "default_folder.svg")
}

// ─────────────────────────────────────────────────────────────────────────────
// Path & Git Utilities
// ─────────────────────────────────────────────────────────────────────────────

const normalizePath = (path: string) => path.replace(/\\/g, "/")

const isIgnoredProjectEventPath = (path: string) => {
  const n = normalizePath(path).toLowerCase()
  return (
    n.includes("/node_modules/") ||
    n.includes("/dist/") ||
    n.includes("/target/") ||
    n.includes("/.next/") ||
    n.includes("/.turbo/") ||
    n.includes("/.cache/") ||
    n.includes("/coverage/")
  )
}

const isChangedPath = (entryPath: string, changeMap: Record<string, GitFileChange>) => {
  const normalizedEntry = normalizePath(entryPath).toLowerCase()
  return Object.values(changeMap).some((change) => {
    const normalizedChange = normalizePath(change.absolutePath).toLowerCase()
    return (
      normalizedChange === normalizedEntry ||
      normalizedChange.startsWith(`${normalizedEntry}/`)
    )
  })
}

const getStatusTone = (statusCode: string) => {
  if (statusCode === "A" || statusCode === "?") return "text-[#8bd5a3]"
  if (statusCode === "D") return "text-[#ef7d7d]"
  if (statusCode) return "text-[#f3d56b]"
  return ""
}

function getDiffCounts(
  entry: FileTreeEntry,
  changeMap: Record<string, GitFileChange>,
  allChanges: GitFileChange[],
): DiffCounts & { statusCode: string; effectiveChange: GitFileChange | null } {
  const normalizedPath = entry.path.replace(/\\/g, "/").toLowerCase()
  const directChange = changeMap[normalizedPath]
  const nestedChanges = entry.isDirectory
    ? allChanges.filter((c) =>
      c.absolutePath.replace(/\\/g, "/").toLowerCase().startsWith(`${normalizedPath}/`),
    )
    : []

  const additions =
    (directChange?.additions ?? 0) +
    (entry.isDirectory ? nestedChanges.reduce((t, c) => t + c.additions, 0) : 0)
  const deletions =
    (directChange?.deletions ?? 0) +
    (entry.isDirectory ? nestedChanges.reduce((t, c) => t + c.deletions, 0) : 0)

  const effectiveChange = directChange ?? nestedChanges[0] ?? null
  const statusCode = effectiveChange?.status?.trim()?.charAt(0) ?? ""

  return { additions, deletions, statusCode, effectiveChange }
}

function flattenLoadedTree(
  entries: FileTreeEntry[],
  childrenByPath: Record<string, FileTreeEntry[]>,
): FileTreeEntry[] {
  const result: FileTreeEntry[] = []
  const visit = (items: FileTreeEntry[]) => {
    for (const item of items) {
      result.push(item)
      if (item.isDirectory && childrenByPath[item.path]) {
        visit(childrenByPath[item.path])
      }
    }
  }
  visit(entries)
  return result
}

// ─────────────────────────────────────────────────────────────────────────────
// ContextMenu
// ─────────────────────────────────────────────────────────────────────────────

function ContextMenu({
  state,
  projectPath,
  onClose,
}: {
  state: ContextMenuState
  projectPath: string
  onClose: () => void
}) {
  let ref: HTMLDivElement | undefined

  onMount(() => {
    const onMouseDown = (e: MouseEvent) => {
      if (ref && !ref.contains(e.target as Node)) onClose()
    }
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose()
    }
    document.addEventListener("mousedown", onMouseDown, true)
    document.addEventListener("keydown", onKeyDown)
    onCleanup(() => {
      document.removeEventListener("mousedown", onMouseDown, true)
      document.removeEventListener("keydown", onKeyDown)
    })
  })

  const relativePath = createMemo(() => {
    const normalized = normalizePath(state.entry.path)
    const base = normalizePath(projectPath).replace(/\/+$/, "")
    return normalized.startsWith(base) ? normalized.slice(base.length + 1) : normalized
  })

  const copyText = async (text: string) => {
    try { await navigator.clipboard.writeText(text) } catch { /* silent */ }
    onClose()
  }

  const revealInFinder = async () => {
    try { await nativeApi.invoke("reveal_in_finder", { path: state.entry.path }) } catch { /* silent */ }
    onClose()
  }

  type MenuItem = { label: string; action: () => void } | null

  const menuItems: MenuItem[] = [
    { label: "Copy Absolute Path", action: () => void copyText(state.entry.path) },
    { label: "Copy Relative Path", action: () => void copyText(relativePath()) },
    { label: "Copy Filename", action: () => void copyText(state.entry.name) },
    null,
    { label: "Reveal in File Manager", action: () => void revealInFinder() },
  ]

  return (
    <div
      ref={ref}
      role="menu"
      class="fixed z-50 min-w-[190px] rounded-md border bg-popover py-1 text-[13px] shadow-md"
      style={{ top: `${state.y}px`, left: `${state.x}px` }}
    >
      {menuItems.map((item) =>
        item === null ? (
          <div class="my-1 border-t border-border" />
        ) : (
          <button
            role="menuitem"
            class="flex w-full items-center px-3 py-1.5 text-left text-popover-foreground hover:bg-accent hover:text-accent-foreground focus:bg-accent focus:outline-none"
            onClick={item.action}
          >
            {item.label}
          </button>
        ),
      )}
    </div>
  )
}

// ─────────────────────────────────────────────────────────────────────────────
// SearchResults
// ─────────────────────────────────────────────────────────────────────────────

function SearchResults({
  query,
  rootEntries,
  childrenByPath,
  selectedFilePath,
  changeMap,
  allChanges,
  iconLookup,
  projectPath,
  onFileSelect,
  onContextMenu,
}: {
  query: string
  rootEntries: FileTreeEntry[]
  childrenByPath: Record<string, FileTreeEntry[]>
  selectedFilePath: string | null
  changeMap: Record<string, GitFileChange>
  allChanges: GitFileChange[]
  iconLookup: IconLookup
  projectPath: string
  onFileSelect: (path: string | null) => void
  onContextMenu: (e: MouseEvent, entry: FileTreeEntry) => void
}) {
  const normalizedProject = normalizePath(projectPath).replace(/\/+$/, "")
  const lowerQuery = query.toLowerCase()

  const matches = createMemo(() => {
    const all = flattenLoadedTree(rootEntries, childrenByPath)
    return all
      .filter((e) => !e.isDirectory && e.name.toLowerCase().includes(lowerQuery))
      .slice(0, 200)
  })

  return (
    <Show when={matches().length > 0} fallback={
      <p class="px-3 py-4 text-[12px] text-muted-foreground">
        No files matching <span class="font-medium">"{query}"</span> in loaded tree.
      </p>
    }>
      <div class="flex flex-col gap-px py-1">
        <For each={matches()}>
          {(entry) => {
            const { additions, deletions, statusCode } = getDiffCounts(entry, changeMap, allChanges)
            const changedTextClass = getStatusTone(statusCode)
            const hasDiff = additions > 0 || deletions > 0
            const relDir = normalizePath(entry.path)
              .replace(`${normalizedProject}/`, "")
              .replace(`/${entry.name}`, "") || "."

            return (
              <Button
                type="button"
                variant="ghost"
                onClick={() => onFileSelect(entry.path)}
                onContextMenu={(e: MouseEvent) => onContextMenu(e, entry)}
                class={`flex min-h-8 w-full flex-col items-start gap-0 rounded-[6px] px-3 py-1 text-left text-[13px] hover:bg-accent/50 ${selectedFilePath === entry.path ? "bg-accent text-accent-foreground" : ""
                  }`}
              >
                <span class={`flex w-full items-center gap-1.5 truncate font-medium ${changedTextClass || "text-foreground"}`}>
                  <img
                    src={getFileIcon(entry.name, iconLookup)}
                    alt=""
                    class="h-3.5 w-3.5 shrink-0 opacity-90"
                    onError={(e) => { e.currentTarget.src = iconPath("default_file.svg") }}
                  />
                  {entry.name}
                  <Show when={hasDiff}>
                    <span class="ml-auto inline-flex shrink-0 items-center gap-1 text-[11px] font-normal">
                      <Show when={additions > 0}><span class="text-[#22c55e]">+{additions}</span></Show>
                      <Show when={deletions > 0}><span class="text-[#ef4444]">-{deletions}</span></Show>
                    </span>
                  </Show>
                </span>
                <span class="max-w-full truncate text-[11px] text-muted-foreground">{relDir}</span>
              </Button>
            )
          }}
        </For>
        <Show when={matches().length === 200}>
          <p class="px-3 py-1.5 text-[11px] text-muted-foreground">
            Showing first 200 results — refine your query to see more.
          </p>
        </Show>
      </div>
    </Show>
  )
}

// ─────────────────────────────────────────────────────────────────────────────
// TreeNode
// ─────────────────────────────────────────────────────────────────────────────

interface TreeNodeProps {
  entry: FileTreeEntry
  depth: number
  isRoot?: boolean
  expandedPaths: Record<string, boolean>
  loadingPaths: Record<string, boolean>
  childrenByPath: Record<string, FileTreeEntry[]>
  selectedFilePath: string | null
  changeMap: Record<string, GitFileChange>
  allChanges: GitFileChange[]
  iconLookup: IconLookup
  onToggle: (entry: FileTreeEntry) => void
  onFileSelect: (filePath: string | null) => void
  onContextMenu: (e: MouseEvent, entry: FileTreeEntry) => void
}

function TreeNode({
  entry,
  depth,
  isRoot,
  expandedPaths,
  loadingPaths,
  childrenByPath,
  selectedFilePath,
  changeMap,
  allChanges,
  iconLookup,
  onToggle,
  onFileSelect,
  onContextMenu,
}: TreeNodeProps) {
  const isExpanded = () => expandedPaths[entry.path]
  const children = () => childrenByPath[entry.path] ?? []
  const isLoading = () => loadingPaths[entry.path]

  const { additions, deletions, statusCode } = getDiffCounts(entry, changeMap, allChanges)
  const hasDiffCounts = additions > 0 || deletions > 0
  const changedTextClass = getStatusTone(statusCode)
  const hasNestedChange = entry.isDirectory &&
    allChanges.some((c) =>
      c.absolutePath.replace(/\\/g, "/").toLowerCase()
        .startsWith(`${entry.path.replace(/\\/g, "/").toLowerCase()}/`),
    )

  if (entry.isDirectory) {
    return (
      <div>
        <Button
          type="button"
          onClick={() => onToggle(entry)}
          onContextMenu={(e: MouseEvent) => onContextMenu(e, entry)}
          variant="ghost"
          class="group flex min-h-7 w-full items-center gap-1 justify-start py-[3px] pr-2 text-left text-[13px] hover:bg-accent/50"
          style={{ "padding-left": getNodePadding(depth, DIRECTORY_BASE_PADDING) }}
        >
          <span class="inline-flex w-4 justify-center text-muted-foreground">
            {isExpanded()
              ? <ChevronDown class="h-3.5 w-3.5" stroke-width={1.8} />
              : <ChevronRight class="h-3.5 w-3.5" stroke-width={1.8} />}
          </span>
          <img
            src={getFolderIcon(Boolean(isExpanded()), Boolean(isRoot))}
            alt=""
            class="h-4 w-4 shrink-0 opacity-95"
            onError={(e) => {
              e.currentTarget.src = iconPath(isRoot ? "default_root_folder.svg" : "default_folder.svg")
            }}
          />
          <span class={`truncate ${changedTextClass || "text-inherit"} ${hasNestedChange ? "font-medium" : ""}`}>
            {entry.name}
          </span>
          <Show when={hasDiffCounts}>
            <span class="ml-auto inline-flex shrink-0 items-center gap-1 text-[11px]">
              <Show when={additions > 0}><span class="text-[#22c55e]">+{additions}</span></Show>
              <Show when={deletions > 0}><span class="text-[#ef4444]">-{deletions}</span></Show>
            </span>
          </Show>
        </Button>

        <Show when={isExpanded()}>
          <div class="ml-[15px] border-l border-border/50">
            <Show when={isLoading()} fallback={
              <For each={children()}>
                {(child) => (
                  <TreeNode
                    entry={child}
                    depth={depth + 1}
                    isRoot={false}
                    expandedPaths={expandedPaths}
                    loadingPaths={loadingPaths}
                    childrenByPath={childrenByPath}
                    selectedFilePath={selectedFilePath}
                    changeMap={changeMap}
                    allChanges={allChanges}
                    iconLookup={iconLookup}
                    onToggle={onToggle}
                    onFileSelect={onFileSelect}
                    onContextMenu={onContextMenu}
                  />
                )}
              </For>
            }>
              <div
                class="py-1 text-[12px] text-muted-foreground"
                style={{ "padding-left": getNodePadding(depth, LOADING_BASE_PADDING) }}
              >
                Loading...
              </div>
            </Show>
          </div>
        </Show>
      </div>
    )
  }

  return (
    <Button
      type="button"
      onClick={() => onFileSelect(entry.path)}
      onContextMenu={(e: MouseEvent) => onContextMenu(e, entry)}
      variant="ghost"
      class={`group flex min-h-7 w-full items-center gap-1 rounded-[6px] justify-start py-[3px] pr-2 text-left text-[13px] ${selectedFilePath === entry.path
        ? "bg-accent text-accent-foreground"
        : statusCode
          ? "bg-transparent text-foreground/80 hover:bg-accent/50"
          : "text-muted-foreground hover:bg-accent/50"
        }`}
      style={{ "padding-left": getNodePadding(depth, FILE_BASE_PADDING) }}
    >
      <img
        src={getFileIcon(entry.name, iconLookup)}
        alt=""
        class="h-4 w-4 shrink-0 opacity-95"
        onError={(e) => { e.currentTarget.src = iconPath("default_file.svg") }}
      />
      <span class={`truncate ${changedTextClass || "text-inherit"}`}>{entry.name}</span>
      <Show when={hasDiffCounts}>
        <span class="ml-auto inline-flex shrink-0 items-center gap-1 text-[11px]">
          <Show when={additions > 0}><span class="text-[#22c55e]">+{additions}</span></Show>
          <Show when={deletions > 0}><span class="text-[#ef4444]">-{deletions}</span></Show>
        </span>
      </Show>
    </Button>
  )
}

// ─────────────────────────────────────────────────────────────────────────────
// FileTree
// ─────────────────────────────────────────────────────────────────────────────

export function FileTree({ selectedFilePath, onFileSelect }: FileTreeProps) {
  const projects = useStore((s) => s.projects)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const currentProject = createMemo(
    () => projects().find((p) => p.id === currentProjectId()) ?? null,
  )

  const [rootEntries, setRootEntries] = createSignal<FileTreeEntry[]>([])
  const [childrenByPath, setChildrenByPath] = createSignal<Record<string, FileTreeEntry[]>>({})
  const [expandedPaths, setExpandedPaths] = createSignal<Record<string, boolean>>({})
  const [loadingPaths, setLoadingPaths] = createSignal<Record<string, boolean>>({})
  const [error, setError] = createSignal<string | null>(null)
  const [gitStatus, setGitStatus] = createSignal<GitStatusSummary | null>(null)
  const [copiedPath, setCopiedPath] = createSignal(false)
  const [availableIconNames, setAvailableIconNames] = createSignal<string[]>([])
  const [searchQuery, setSearchQuery] = createSignal("")
  const [searchOpen, setSearchOpen] = createSignal(false)
  const [isRefreshing, setIsRefreshing] = createSignal(false)
  const [contextMenu, setContextMenu] = createSignal<ContextMenuState | null>(null)

  let expandedPathsRef: Record<string, boolean> = {}
  let searchInputRef: HTMLInputElement | undefined

  const setExpandedPathsSynced = (
    updater: Record<string, boolean> | ((prev: Record<string, boolean>) => Record<string, boolean>),
  ) => {
    setExpandedPaths((prev) => {
      const next = typeof updater === "function" ? updater(prev) : updater
      expandedPathsRef = next
      return next
    })
  }

  const iconLookup = createMemo(() => buildIconLookup(availableIconNames()))

  const changeMap = createMemo(
    () =>
      Object.fromEntries(
        (gitStatus()?.changedFiles ?? []).map((c) => [
          normalizePath(c.absolutePath).toLowerCase(),
          c,
        ]),
      ),
  )

  const allChanges = createMemo(() => Object.values(changeMap()))

  const changedFileCount = () => gitStatus()?.changedFiles.length ?? 0

  const entryHasChanges = (entryPath: string) => isChangedPath(entryPath, changeMap())

  const sortEntries = (entries: FileTreeEntry[]) =>
    [...entries].sort((a, b) => {
      const ac = entryHasChanges(a.path)
      const bc = entryHasChanges(b.path)
      if (ac !== bc) return ac ? -1 : 1
      if (a.isDirectory !== b.isDirectory) return a.isDirectory ? -1 : 1
      return a.name.localeCompare(b.name)
    })

  const mergeDeletedEntries = (entries: FileTreeEntry[], dirPath: string) => {
    const normalizedDir = normalizePath(dirPath).replace(/\/+$/, "")
    const existingByName = new Map(entries.map((e) => [e.name.toLowerCase(), e]))
    const synthetic = new Map<string, FileTreeEntry>()

    for (const change of gitStatus()?.changedFiles ?? []) {
      if (change.status.trim().charAt(0) !== "D") continue
      const normalizedDeleted = normalizePath(change.absolutePath)
      if (!normalizedDeleted.startsWith(normalizedDir)) continue
      const remainder = normalizedDeleted.slice(normalizedDir.length).replace(/^\/+/, "")
      if (!remainder) continue
      const segments = remainder.split("/").filter(Boolean)
      if (segments.length === 0) continue
      const immediateName = segments[0]
      const key = immediateName.toLowerCase()
      if (existingByName.has(key) || synthetic.has(key)) continue
      const isDirectory = segments.length > 1
      synthetic.set(key, {
        name: immediateName,
        path: isDirectory
          ? `${normalizedDir}/${immediateName}`
          : normalizedDeleted,
        isDirectory,
        isVirtual: true,
        isDeleted: !isDirectory,
      })
    }

    return [...entries, ...synthetic.values()]
  }

  const loadDirectory = async (path: string) => {
    setLoadingPaths((p) => ({ ...p, [path]: true }))
    try {
      const entries = await nativeApi.invoke("list_directory", { path }).catch(() => []) as FileTreeEntry[]
      const sorted = sortEntries(mergeDeletedEntries(entries, path))
      setChildrenByPath((p) => ({ ...p, [path]: sorted }))
      return sorted
    } finally {
      setLoadingPaths((p) => ({ ...p, [path]: false }))
    }
  }

  const loadGitStatus = async () => {
    const project = currentProject()
    if (!project?.path) { setGitStatus(null); return }
    try {
      const summary = await nativeApi.invoke("get_git_status", { path: project.path }) as GitStatusSummary
      setGitStatus(summary)
    } catch {
      setGitStatus(null)
    }
  }

  const refreshTree = async (options?: { resetExpanded?: boolean }) => {
    const project = currentProject()
    if (!project?.path) {
      setRootEntries([])
      setChildrenByPath({})
      setExpandedPathsSynced({})
      setError(null)
      return
    }

    try {
      setError(null)

      if (options?.resetExpanded) {
        setExpandedPathsSynced({})
        setChildrenByPath({})
      }

      const rawRoot = await nativeApi.invoke("list_directory", { path: project.path }) as FileTreeEntry[]
      const root = sortEntries(mergeDeletedEntries(rawRoot, project.path))

      const expandedList = Object.entries(expandedPathsRef)
        .filter(([, v]) => v)
        .map(([path]) => path)

      const nextChildren: Record<string, FileTreeEntry[]> = {}
      await Promise.all(
        expandedList.map(async (path) => {
          const entries = await nativeApi.invoke("list_directory", { path }).catch(() => []) as FileTreeEntry[]
          nextChildren[path] = sortEntries(mergeDeletedEntries(entries, path))
        }),
      )

      setRootEntries(root)
      setChildrenByPath(nextChildren)

      if (selectedFilePath) {
        const visible = new Set<string>()
        const collect = (items: FileTreeEntry[]) => {
          for (const item of items) {
            visible.add(item.path)
            if (item.isDirectory && nextChildren[item.path]) collect(nextChildren[item.path])
          }
        }
        collect(root)
        if (!visible.has(selectedFilePath)) onFileSelect(null)
      }
    } catch (err) {
      setRootEntries([])
      setError(String(err))
    }
  }

  const handleManualRefresh = async () => {
    setIsRefreshing(true)
    await Promise.all([refreshTree(), loadGitStatus()])
    setIsRefreshing(false)
  }

  // Load icon manifest
  onMount(() => {
    let disposed = false
    fetch("/vscode-icons/manifest.json")
      .then((r) => r.ok ? r.json() : [])
      .then((payload) => {
        if (!disposed && Array.isArray(payload)) {
          setAvailableIconNames(payload.filter((x): x is string => typeof x === "string"))
        }
      })
      .catch(() => { if (!disposed) setAvailableIconNames([]) })
    onCleanup(() => { disposed = true })
  })

  // Reset tree when project changes
  createEffect(() => {
    currentProject()?.path // track dependency
    void refreshTree({ resetExpanded: true })
  })

  // Load git status when project changes
  createEffect(() => {
    currentProject()?.path // track dependency
    void loadGitStatus()
  })

  // Start/stop filesystem watch
  onMount(() => {
    let timeoutId: number | null = null
    let isCancelled = false

    const setupWatcher = async () => {
      await new Promise(resolve => { timeoutId = window.setTimeout(resolve, 50) })
      if (isCancelled) return
      void nativeApi.invoke("set_project_watch", { path: currentProject()?.path ?? null }).catch(console.error)
    }

    void setupWatcher()

    onCleanup(() => {
      isCancelled = true
      if (timeoutId) window.clearTimeout(timeoutId)
      void nativeApi.invoke("set_project_watch", { path: null }).catch(console.error)
    })
  })

  // Listen for FS events and debounce refresh
  onMount(() => {
    let fsTimer: number | null = null
    let gitTimer: number | null = null

    const unlistenPromise = nativeApi.listen<ProjectFsEvent>("project-fs-event", (event) => {
      const project = currentProject()
      if (!project || event.payload.projectPath !== project.path) return
      const relevant = event.payload.paths.filter((p) => !isIgnoredProjectEventPath(p))
      if (relevant.length === 0) return

      if (fsTimer) window.clearTimeout(fsTimer)
      if (gitTimer) window.clearTimeout(gitTimer)

      fsTimer = window.setTimeout(() => { fsTimer = null; void refreshTree() }, DEBOUNCE_FS_MS)
      gitTimer = window.setTimeout(() => { gitTimer = null; void loadGitStatus() }, DEBOUNCE_GIT_MS)
    })

    onCleanup(() => {
      if (fsTimer) window.clearTimeout(fsTimer)
      if (gitTimer) window.clearTimeout(gitTimer)
      void unlistenPromise.then((u) => u())
    })
  })

  // Copy path toast reset
  createEffect(() => {
    if (!copiedPath()) return
    const t = window.setTimeout(() => setCopiedPath(false), 1400)
    onCleanup(() => window.clearTimeout(t))
  })

  // Focus search input when opened
  createEffect(() => {
    if (searchOpen()) searchInputRef?.focus()
    else setSearchQuery("")
  })

  const handleToggle = async (entry: FileTreeEntry) => {
    if (!entry.isDirectory) return
    const willExpand = !expandedPathsRef[entry.path]
    setExpandedPathsSynced((prev) => ({ ...prev, [entry.path]: willExpand }))

    if (willExpand && !childrenByPath()[entry.path]) {
      try {
        await loadDirectory(entry.path)
      } catch (err) {
        setError(String(err))
      }
    }
  }

  const handleCollapseAll = () => {
    setExpandedPathsSynced({})
  }

  const handleExpandChanged = async () => {
    const project = currentProject()
    const changes = allChanges()
    if (!project?.path || changes.length === 0) return

    const dirsToExpand = new Set<string>()

    for (const change of changes) {
      const changedPath = normalizePath(change.absolutePath)
      const projectRoot = normalizePath(project.path).replace(/\/+$/, "")
      if (!changedPath.startsWith(projectRoot)) continue

      const relative = changedPath.slice(projectRoot.length + 1)
      const segments = relative.split("/")

      for (let i = 1; i < segments.length; i++) {
        dirsToExpand.add(`${projectRoot}/${segments.slice(0, i).join("/")}`)
      }
    }

    const nextExpanded = { ...expandedPathsRef }
    for (const dir of dirsToExpand) nextExpanded[dir] = true
    setExpandedPathsSynced(nextExpanded)

    await Promise.all(
      [...dirsToExpand].map(async (dir) => {
        if (!childrenByPath()[dir]) {
          try { await loadDirectory(dir) } catch { /* silent */ }
        }
      }),
    )
  }

  const handleCopyProjectPath = async () => {
    const project = currentProject()
    if (!project?.path) return
    try {
      await navigator.clipboard.writeText(project.path)
      setCopiedPath(true)
    } catch {
      setCopiedPath(false)
    }
  }

  const handleContextMenu = (e: MouseEvent, entry: FileTreeEntry) => {
    e.preventDefault()
    e.stopPropagation()
    setContextMenu({ x: e.clientX, y: e.clientY, entry })
  }

  const handleCloseContextMenu = () => setContextMenu(null)

  const isSearching = () => searchOpen() && searchQuery().trim().length > 0

  return (
    <aside class="flex h-full w-[332px] flex-col border-l bg-muted/40 text-foreground">

      {/* ── Toolbar ───────────────────────────────────────────────────────── */}
      <div class="flex items-center gap-1 border-b px-2 py-1.5">
        <span class="flex-1 truncate text-[12px] font-medium text-foreground/80">
          Explorer
        </span>

        <Show when={changedFileCount() > 0 && !searchOpen()}>
          <span
            class="inline-flex items-center gap-1 text-[11px] text-muted-foreground"
            title={`${changedFileCount()} changed file${changedFileCount() !== 1 ? "s" : ""}`}
          >
            <GitBranch class="h-3 w-3" />
            {changedFileCount()}
          </span>
        </Show>

        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          class="h-6 w-6"
          title={searchOpen() ? "Close search" : "Search files (Ctrl+F)"}
          onClick={() => setSearchOpen((v) => !v)}
        >
          <Show when={searchOpen()} fallback={<Search class="h-3.5 w-3.5" />}>
            <X class="h-3.5 w-3.5" />
          </Show>
        </Button>

        <Show when={changedFileCount() > 0}>
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            class="h-6 w-6"
            title="Expand changed files"
            onClick={() => void handleExpandChanged()}
          >
            <GitBranch class="h-3.5 w-3.5" />
          </Button>
        </Show>

        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          class="h-6 w-6"
          title="Collapse all"
          onClick={handleCollapseAll}
        >
          <ChevronsUpDown class="h-3.5 w-3.5" />
        </Button>

        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          class={`h-6 w-6 ${isRefreshing() ? "animate-spin" : ""}`}
          title="Refresh"
          disabled={isRefreshing()}
          onClick={() => void handleManualRefresh()}
        >
          <RefreshCw class="h-3.5 w-3.5" />
        </Button>
      </div>

      {/* ── Search bar ────────────────────────────────────────────────────── */}
      <Show when={searchOpen()}>
        <div class="border-b px-2 py-1.5">
          <div class="relative flex items-center">
            <Search class="pointer-events-none absolute left-2 h-3 w-3 text-muted-foreground" />
            <input
              ref={searchInputRef}
              type="text"
              value={searchQuery()}
              onInput={(e) => setSearchQuery(e.currentTarget.value)}
              onKeyDown={(e) => { if (e.key === "Escape") setSearchOpen(false) }}
              placeholder="Filter files…"
              class="w-full rounded-md border bg-background py-1 pl-6 pr-2 text-[13px] outline-none focus:ring-1 focus:ring-ring"
            />
            <Show when={searchQuery()}>
              <button
                class="absolute right-2 text-muted-foreground hover:text-foreground"
                onClick={() => setSearchQuery("")}
                tabIndex={-1}
              >
                <X class="h-3 w-3" />
              </button>
            </Show>
          </div>
        </div>
      </Show>

      {/* ── Tree / Search results ─────────────────────────────────────────── */}
      <div class="min-h-0 flex-1 overflow-y-auto px-[10px] py-2">
        <Show when={currentProject()} fallback={
          <p class="px-2 py-3 text-sm text-muted-foreground">Select a project to see its files.</p>
        }>
          <Show when={!error()} fallback={
            <p class="px-2 py-3 text-sm text-destructive">{error()}</p>
          }>
            <Show when={isSearching()} fallback={
              <Show when={rootEntries().length > 0} fallback={
                <p class="px-2 py-3 text-sm text-muted-foreground">This folder is empty.</p>
              }>
                <For each={rootEntries()}>
                  {(entry) => (
                    <TreeNode
                      entry={entry}
                      depth={0}
                      isRoot
                      expandedPaths={expandedPaths()}
                      loadingPaths={loadingPaths()}
                      childrenByPath={childrenByPath()}
                      selectedFilePath={selectedFilePath}
                      changeMap={changeMap()}
                      allChanges={allChanges()}
                      iconLookup={iconLookup()}
                      onToggle={handleToggle}
                      onFileSelect={onFileSelect}
                      onContextMenu={handleContextMenu}
                    />
                  )}
                </For>
              </Show>
            }>
              <SearchResults
                query={searchQuery().trim()}
                rootEntries={rootEntries()}
                childrenByPath={childrenByPath()}
                selectedFilePath={selectedFilePath}
                changeMap={changeMap()}
                allChanges={allChanges()}
                iconLookup={iconLookup()}
                projectPath={currentProject()!.path}
                onFileSelect={onFileSelect}
                onContextMenu={handleContextMenu}
              />
            </Show>
          </Show>
        </Show>
      </div>

      {/* ── Footer ────────────────────────────────────────────────────────── */}
      <div class="flex items-center gap-2 border-t px-3 py-2.5">
        <span
          class="min-w-0 flex-1 truncate text-[12px] text-muted-foreground"
          title={currentProject()?.path ?? ""}
        >
          {currentProject()?.path ?? "No project selected"}
        </span>
        <Button
          type="button"
          onClick={() => void handleCopyProjectPath()}
          disabled={!currentProject()?.path}
          variant="ghost"
          size="icon-xs"
          class="h-5 w-5 shrink-0"
          title={copiedPath() ? "Copied!" : "Copy path"}
        >
          <Show when={copiedPath()} fallback={<Copy class="h-3.5 w-3.5" stroke-width={1.9} />}>
            <Check class="h-3.5 w-3.5" stroke-width={2} />
          </Show>
        </Button>
      </div>

      {/* ── Context menu ──────────────────────────────────────────────────── */}
      <Show when={contextMenu() && currentProject()}>
        <ContextMenu
          state={contextMenu()!}
          projectPath={currentProject()!.path}
          onClose={handleCloseContextMenu}
        />
      </Show>
    </aside>
  )
}
