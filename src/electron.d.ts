import type { Project } from "./types"
import type { CliProbeResult } from "./config/check"

export interface TerminalHostInfo {
  os: string
  windowsBuildNumber?: number | null
}

export interface ElectronFileTreeEntry {
  name: string
  path: string
  isDirectory: boolean
}

export interface ElectronGitBranchInfo {
  repoName?: string | null
  head: string
  upstream?: string | null
}

export interface ElectronGitStatusSummary {
  repoRoot: string
  changedFiles: Array<{
    path: string
    absolutePath: string
    status: string
    additions: number
    deletions: number
  }>
}

export interface ElectronOpenDialogOptions {
  directory?: boolean
  multiple?: boolean
  title?: string
  filters?: Array<{
    name: string
    extensions: string[]
  }>
}

export interface ElectronTerminalSpawnOptions {
  id?: string
  shell: string
  args?: string[]
  cwd?: string
  rows: number
  cols: number
  env?: Record<string, string>
}

export interface ShobNativeApi {
  platform: "windows" | "macos" | "linux" | string
  invoke<T = unknown>(command: string, payload?: unknown): Promise<T>
  listen<T = unknown>(
    channel: string,
    callback: (event: { payload: T }) => void,
  ): Promise<() => void>
  window: {
    minimize(): Promise<void>
    toggleMaximize(): Promise<boolean>
    isMaximized(): Promise<boolean>
    close(): Promise<void>
    onResized(callback: () => void): Promise<() => void>
  }
  terminal: {
    spawn(options: ElectronTerminalSpawnOptions): Promise<{ id: string }>
    write(id: string, data: string): Promise<void>
    resize(id: string, cols: number, rows: number): Promise<void>
    kill(id: string): Promise<void>
    onData(id: string, callback: (data: string) => void): () => void
    onExit(id: string, callback: () => void): () => void
  }
}

declare global {
  interface Window {
    shob?: ShobNativeApi
  }
}

export interface NativeCommandMap {
  get_projects: { args: undefined; result: Project[] }
  save_project: { args: { project: Project }; result: Project }
  delete_project: { args: { projectId: string }; result: void }
  save_session_output: { args: { sessionId: string; output: string }; result: void }
  load_session_output: { args: { sessionId: string }; result: string }
  read_image_data_url: { args: { path: string }; result: string }
  get_available_shells: { args: undefined; result: string[] }
  get_terminal_host_info: { args: undefined; result: TerminalHostInfo }
  probe_cli_tools: { args: { items: { id: string; commands: string[] }[] }; result: CliProbeResult[] }
  set_project_watch: { args: { path: string | null }; result: void }
  list_directory: { args: { path: string }; result: ElectronFileTreeEntry[] }
  get_git_status: { args: { path: string }; result: ElectronGitStatusSummary }
  get_git_branch: { args: { path: string }; result: ElectronGitBranchInfo }
  cleanup_runtime: { args: undefined; result: void }
  reveal_in_finder: { args: { path: string }; result: void }
  open_with_app: { args: { target: string; path: string }; result: void }
  show_open_dialog: { args: ElectronOpenDialogOptions; result: string | string[] | null }
}
