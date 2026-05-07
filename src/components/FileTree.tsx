import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  memo,
} from "react"
import { nativeApi } from "../services/native"
import {
  ChevronDown,
  ChevronRight,
} from "lucide-react"
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
const DIRECTORY_BASE_PADDING = 4
const FILE_BASE_PADDING = 4
const LOADING_BASE_PADDING = 16
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

/** Compute diff counts for a tree entry, including nested changes for directories. */
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
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const onMouseDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose()
    }
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose()
    }
    document.addEventListener("mousedown", onMouseDown, true)
    document.addEventListener("keydown", onKeyDown)
    return () => {
      document.removeEventListener("mousedown", onMouseDown, true)
      document.removeEventListener("keydown", onKeyDown)
    }
  }, [onClose])

  const relativePath = useMemo(() => {
    const normalized = normalizePath(state.entry.path)
    const base = normalizePath(projectPath).replace(/\/+$/, "")
    return normalized.startsWith(base) ? normalized.slice(base.length + 1) : normalized
  }, [state.entry.path, projectPath])

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
    { label: "Copy Relative Path", action: () => void copyText(relativePath) },
    { label: "Copy Filename", action: () => void copyText(state.entry.name) },
    null,
    { label: "Reveal in File Manager", action: () => void revealInFinder() },
  ]

  return (
    <div
      ref={ref}
      role="menu"
      className="fixed z-50 min-w-[190px] rounded-md border bg-popover py-1 text-[13px] shadow-md"
      style={{ top: state.y, left: state.x }}
    >
      {menuItems.map((item, i) =>
        item === null ? (
          <div key={`sep-${i}`} className="my-1 border-t border-border" />
        ) : (
          <button
            key={item.label}
            role="menuitem"
            className="flex w-full items-center px-3 py-1.5 text-left text-popover-foreground hover:bg-accent hover:text-accent-foreground focus:bg-accent focus:outline-none"
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
// TreeNode — memoized to avoid subtree re-renders on unrelated state changes
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
  onContextMenu: (e: React.MouseEvent, entry: FileTreeEntry) => void
}

const TreeNode = memo(function TreeNode({
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
  const isExpanded = expandedPaths[entry.path]
  const children = childrenByPath[entry.path] ?? []
  const isLoading = loadingPaths[entry.path]

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
          onContextMenu={(e) => onContextMenu(e, entry)}
          variant="ghost"
          className="group/filetree flex h-6 w-full items-center justify-start gap-x-1.5 rounded-md px-1.5 py-0 text-left text-[12px] font-medium transition-colors hover:bg-muted/45 active:bg-accent/70"
          style={{ paddingLeft: getNodePadding(depth, DIRECTORY_BASE_PADDING) }}
        >
          <span className="inline-flex h-4 w-4 items-center justify-center text-muted-foreground">
            {isExpanded
              ? <ChevronDown className="h-3.5 w-3.5" strokeWidth={1.8} />
              : <ChevronRight className="h-3.5 w-3.5" strokeWidth={1.8} />}
          </span>
          <img
            src={getFolderIcon(Boolean(isExpanded), Boolean(isRoot))}
            alt=""
            className="h-4 w-4 shrink-0 opacity-95"
            onError={(e) => {
              e.currentTarget.src = iconPath(isRoot ? "default_root_folder.svg" : "default_folder.svg")
            }}
          />
          <span className={`min-w-0 flex-1 truncate ${changedTextClass || "text-foreground/85"} ${hasNestedChange ? "font-medium" : ""}`}>
            {entry.name}
          </span>
          {hasDiffCounts && (
            <span className="ml-auto inline-flex shrink-0 items-center gap-1 text-[11px]">
              {additions > 0 && <span className="text-[#22c55e]">+{additions}</span>}
              {deletions > 0 && <span className="text-[#ef4444]">-{deletions}</span>}
            </span>
          )}
        </Button>

        {isExpanded && (
          <div className="ml-[14px] border-l border-border/45">
            {isLoading ? (
              <div
                className="py-1 text-[12px] text-muted-foreground"
                style={{ paddingLeft: getNodePadding(depth, LOADING_BASE_PADDING) }}
              >
                Loading...
              </div>
            ) : (
              children.map((child) => (
                <TreeNode
                  key={child.path}
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
              ))
            )}
          </div>
        )}
      </div>
    )
  }

  return (
    <Button
      type="button"
      onClick={() => onFileSelect(entry.path)}
      onContextMenu={(e) => onContextMenu(e, entry)}
      variant="ghost"
      className={`group/filetree flex h-6 w-full items-center justify-start gap-x-1.5 rounded-md px-1.5 py-0 text-left text-[12px] font-medium transition-colors ${selectedFilePath === entry.path
        ? "bg-accent/80 text-accent-foreground"
        : statusCode
          ? "bg-transparent text-foreground/85 hover:bg-muted/45 active:bg-accent/70"
          : "text-foreground/70 hover:bg-muted/45 active:bg-accent/70"
        }`}
      style={{ paddingLeft: getNodePadding(depth, FILE_BASE_PADDING) }}
    >
      <span className="h-4 w-4 shrink-0" />
      <img
        src={getFileIcon(entry.name, iconLookup)}
        alt=""
        className="h-4 w-4 shrink-0 opacity-95"
        onError={(e) => { e.currentTarget.src = iconPath("default_file.svg") }}
      />
      <span className={`min-w-0 flex-1 truncate ${changedTextClass || "text-inherit"}`}>{entry.name}</span>
      {hasDiffCounts && (
        <span className="ml-auto inline-flex shrink-0 items-center gap-1 text-[11px]">
          {additions > 0 && <span className="text-[#22c55e]">+{additions}</span>}
          {deletions > 0 && <span className="text-[#ef4444]">-{deletions}</span>}
        </span>
      )}
    </Button>
  )
})

// ─────────────────────────────────────────────────────────────────────────────
// FileTree
// ─────────────────────────────────────────────────────────────────────────────

export function FileTree({ selectedFilePath, onFileSelect }: FileTreeProps) {
  const { projects, currentProjectId } = useStore()
  const currentProject = useMemo(
    () => projects.find((p) => p.id === currentProjectId) ?? null,
    [projects, currentProjectId],
  )

  const [rootEntries, setRootEntries] = useState<FileTreeEntry[]>([])
  const [childrenByPath, setChildrenByPath] = useState<Record<string, FileTreeEntry[]>>({})
  const [expandedPaths, setExpandedPaths] = useState<Record<string, boolean>>({})
  const [loadingPaths, setLoadingPaths] = useState<Record<string, boolean>>({})
  const [error, setError] = useState<string | null>(null)
  const [gitStatus, setGitStatus] = useState<GitStatusSummary | null>(null)
  const [availableIconNames, setAvailableIconNames] = useState<string[]>([])
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null)

  // ── Refs ─────────────────────────────────────────────────────────────────
  // Key fix: store expandedPaths in a ref so refreshTree doesn't need it as a
  // dep, preventing the FS event listener from re-registering on every expand.
  const expandedPathsRef = useRef<Record<string, boolean>>({})

  /** Synced setter — keeps both state (for renders) and ref (for callbacks) in step. */
  const setExpandedPathsSynced = useCallback(
    (updater: Record<string, boolean> | ((prev: Record<string, boolean>) => Record<string, boolean>)) => {
      setExpandedPaths((prev) => {
        const next = typeof updater === "function" ? updater(prev) : updater
        expandedPathsRef.current = next
        return next
      })
    },
    [],
  )

  // ── Derived ───────────────────────────────────────────────────────────────

  const iconLookup = useMemo(() => buildIconLookup(availableIconNames), [availableIconNames])

  const changeMap = useMemo(
    () =>
      Object.fromEntries(
        (gitStatus?.changedFiles ?? []).map((c) => [
          normalizePath(c.absolutePath).toLowerCase(),
          c,
        ]),
      ),
    [gitStatus],
  )

  const allChanges = useMemo(() => Object.values(changeMap), [changeMap])

  // ── Sorting / merging helpers ─────────────────────────────────────────────

  const entryHasChanges = useCallback(
    (entryPath: string) => isChangedPath(entryPath, changeMap),
    [changeMap],
  )

  const sortEntries = useCallback(
    (entries: FileTreeEntry[]) =>
      [...entries].sort((a, b) => {
        const ac = entryHasChanges(a.path)
        const bc = entryHasChanges(b.path)
        if (ac !== bc) return ac ? -1 : 1
        if (a.isDirectory !== b.isDirectory) return a.isDirectory ? -1 : 1
        return a.name.localeCompare(b.name)
      }),
    [entryHasChanges],
  )

  const mergeDeletedEntries = useCallback(
    (entries: FileTreeEntry[], dirPath: string) => {
      const normalizedDir = normalizePath(dirPath).replace(/\/+$/, "")
      const existingByName = new Map(entries.map((e) => [e.name.toLowerCase(), e]))
      const synthetic = new Map<string, FileTreeEntry>()

      for (const change of gitStatus?.changedFiles ?? []) {
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
    },
    [gitStatus],
  )

  // ── Data loading ──────────────────────────────────────────────────────────

  const loadDirectory = useCallback(
    async (path: string) => {
      setLoadingPaths((p) => ({ ...p, [path]: true }))
      try {
        const entries = await nativeApi.invoke("list_directory", { path }).catch(() => []) as FileTreeEntry[]
        const sorted = sortEntries(mergeDeletedEntries(entries, path))
        setChildrenByPath((p) => ({ ...p, [path]: sorted }))
        return sorted
      } finally {
        setLoadingPaths((p) => ({ ...p, [path]: false }))
      }
    },
    [mergeDeletedEntries, sortEntries],
  )

  const loadGitStatus = useCallback(async () => {
    if (!currentProject?.path) { setGitStatus(null); return }
    try {
      const summary = await nativeApi.invoke("get_git_status", { path: currentProject.path }) as GitStatusSummary
      setGitStatus(summary)
    } catch {
      setGitStatus(null)
    }
  }, [currentProject?.path])

  /**
   * Refresh the tree without resetting expanded state.
   * Uses `expandedPathsRef` instead of state to avoid re-creation on every expand.
   */
  const refreshTree = useCallback(
    async (options?: { resetExpanded?: boolean }) => {
      if (!currentProject?.path) {
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

        const rawRoot = await nativeApi.invoke("list_directory", { path: currentProject.path }) as FileTreeEntry[]
        const root = sortEntries(mergeDeletedEntries(rawRoot, currentProject.path))

        // Re-load all currently expanded directories using the ref (stable reference)
        const expandedList = Object.entries(expandedPathsRef.current)
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
    },
    // expandedPaths deliberately excluded — we use the ref instead
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [currentProject?.path, selectedFilePath, onFileSelect, mergeDeletedEntries, sortEntries],
  )

  // ── Effects ───────────────────────────────────────────────────────────────

  // Load icon manifest
  useEffect(() => {
    let disposed = false
    fetch("/vscode-icons/manifest.json")
      .then((r) => r.ok ? r.json() : [])
      .then((payload) => {
        if (!disposed && Array.isArray(payload)) {
          setAvailableIconNames(payload.filter((x): x is string => typeof x === "string"))
        }
      })
      .catch(() => { if (!disposed) setAvailableIconNames([]) })
    return () => { disposed = true }
  }, [])

  // Reset tree when project changes
  useEffect(() => {
    void refreshTree({ resetExpanded: true })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [currentProject?.path])

  // Load git status when project changes
  useEffect(() => {
    void loadGitStatus()
  }, [loadGitStatus])

  // Start/stop filesystem watch (debounced to prevent OS errors on rapid changes)
  useEffect(() => {
    let timeoutId: number | null = null
    let isCancelled = false

    const setupWatcher = async () => {
      // Small delay to batch rapid path changes
      await new Promise(resolve => { timeoutId = window.setTimeout(resolve, 50) })
      if (isCancelled) return
      void nativeApi.invoke("set_project_watch", { path: currentProject?.path ?? null }).catch(console.error)
    }

    void setupWatcher()

    return () => {
      isCancelled = true
      if (timeoutId) window.clearTimeout(timeoutId)
      void nativeApi.invoke("set_project_watch", { path: null }).catch(console.error)
    }
  }, [currentProject?.path])

  // Listen for FS events and debounce refresh
  useEffect(() => {
    if (!currentProject?.path) return

    let fsTimer: number | null = null
    let gitTimer: number | null = null

    const unlistenPromise = nativeApi.listen<ProjectFsEvent>("project-fs-event", (event) => {
      if (event.payload.projectPath !== currentProject.path) return
      const relevant = event.payload.paths.filter((p) => !isIgnoredProjectEventPath(p))
      if (relevant.length === 0) return

      if (fsTimer) window.clearTimeout(fsTimer)
      if (gitTimer) window.clearTimeout(gitTimer)

      fsTimer = window.setTimeout(() => { fsTimer = null; void refreshTree() }, DEBOUNCE_FS_MS)
      gitTimer = window.setTimeout(() => { gitTimer = null; void loadGitStatus() }, DEBOUNCE_GIT_MS)
    })

    return () => {
      if (fsTimer) window.clearTimeout(fsTimer)
      if (gitTimer) window.clearTimeout(gitTimer)
      void unlistenPromise.then((u) => u())
    }
  }, [currentProject?.path, refreshTree, loadGitStatus])

  // ── Handlers ──────────────────────────────────────────────────────────────

  const handleToggle = useCallback(async (entry: FileTreeEntry) => {
    if (!entry.isDirectory) return
    const willExpand = !expandedPathsRef.current[entry.path]
    setExpandedPathsSynced((prev) => ({ ...prev, [entry.path]: willExpand }))

    if (willExpand && !childrenByPath[entry.path]) {
      try {
        await loadDirectory(entry.path)
      } catch (err) {
        setError(String(err))
      }
    }
  }, [childrenByPath, loadDirectory, setExpandedPathsSynced])

  const handleContextMenu = useCallback((e: React.MouseEvent, entry: FileTreeEntry) => {
    e.preventDefault()
    e.stopPropagation()
    setContextMenu({ x: e.clientX, y: e.clientY, entry })
  }, [])

  const handleCloseContextMenu = useCallback(() => setContextMenu(null), [])

  // ── Render ────────────────────────────────────────────────────────────────

  return (
    <aside className="flex h-full w-[332px] flex-col border-l bg-background text-foreground">
      <div className="min-h-0 flex-1 overflow-y-auto px-[10px] py-2">
        {!currentProject ? (
          <p className="px-2 py-3 text-sm text-muted-foreground">Select a project to see its files.</p>
        ) : error ? (
          <p className="px-2 py-3 text-sm text-destructive">{error}</p>
        ) : rootEntries.length === 0 ? (
          <p className="px-2 py-3 text-sm text-muted-foreground">This folder is empty.</p>
        ) : (
          rootEntries.map((entry) => (
            <TreeNode
              key={entry.path}
              entry={entry}
              depth={0}
              isRoot
              expandedPaths={expandedPaths}
              loadingPaths={loadingPaths}
              childrenByPath={childrenByPath}
              selectedFilePath={selectedFilePath}
              changeMap={changeMap}
              allChanges={allChanges}
              iconLookup={iconLookup}
              onToggle={handleToggle}
              onFileSelect={onFileSelect}
              onContextMenu={handleContextMenu}
            />
          ))
        )}
      </div>

      {/* ── Context menu (portal-free, fixed-position) ────────────────────── */}
      {contextMenu && currentProject && (
        <ContextMenu
          state={contextMenu}
          projectPath={currentProject.path}
          onClose={handleCloseContextMenu}
        />
      )}
    </aside>
  )
}
