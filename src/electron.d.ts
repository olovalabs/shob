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

export interface ElectronOpencodeServerStatus {
  running: boolean
  healthy: boolean
  url?: string
  hostname?: string
  port?: number
  username?: string
  password?: string
  modulePath?: string
  startedAt?: number
}

export interface ElectronOpencodeProviderModel {
  id: string
  name?: string
  status?: string
  reasoning?: boolean
  attachment?: boolean
  cost?: {
    input?: number
    output?: number
    cache_read?: number
    cache_write?: number
  }
  variants?: Record<string, Record<string, unknown>>
}

export interface ElectronOpencodeProvider {
  id: string
  name: string
  source?: "env" | "config" | "custom" | "api"
  env?: string[]
  options?: Record<string, unknown>
  models: Record<string, ElectronOpencodeProviderModel>
}

export interface ElectronOpencodeProviderList {
  all: ElectronOpencodeProvider[]
  connected: string[]
  default: Record<string, string>
}

export interface ElectronOpencodePromptResult {
  sessionID: string
  content: string
  error?: unknown
  message?: unknown
  toolCalls?: Array<{
    id?: string | null
    callID?: string | null
    tool: string
    status: "pending" | "running" | "completed" | "error" | string
    title?: string | null
    input?: unknown
    output?: string | null
    error?: string | null
    startedAt?: number | null
    endedAt?: number | null
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
  opencode_server_status: { args: { includeCredentials?: boolean } | undefined; result: ElectronOpencodeServerStatus }
  opencode_server_start: {
    args: { hostname?: string; port?: number; password?: string; includeCredentials?: boolean } | undefined
    result: ElectronOpencodeServerStatus
  }
  opencode_server_stop: { args: undefined; result: ElectronOpencodeServerStatus }
  opencode_provider_list: {
    args: { directory?: string; workspace?: string } | undefined
    result: ElectronOpencodeProviderList
  }
  opencode_provider_auth_methods: {
    args: { directory?: string; workspace?: string } | undefined
    result: Record<string, Array<{ type: "oauth" | "api"; label: string; prompts?: unknown[] }>>
  }
  opencode_global_dispose: { args: undefined; result: unknown }
  opencode_config_get: {
    args: { directory?: string; workspace?: string } | undefined
    result: Record<string, unknown>
  }
  opencode_config_update: {
    args: { directory?: string; workspace?: string; config: Record<string, unknown> }
    result: Record<string, unknown>
  }
  opencode_global_config_get: { args: undefined; result: Record<string, unknown> }
  opencode_global_config_update: {
    args: { config: Record<string, unknown> }
    result: Record<string, unknown>
  }
  opencode_auth_set: {
    args: { providerID: string; key?: string; auth?: unknown }
    result: boolean
  }
  opencode_auth_remove: { args: { providerID: string }; result: boolean }
  opencode_oauth_authorize: {
    args: { providerID: string; method?: number; inputs?: Record<string, string>; directory?: string; workspace?: string }
    result: { url: string; method: "auto" | "code"; instructions: string }
  }
  opencode_oauth_callback: {
    args: { providerID: string; method?: number; code?: string; directory?: string; workspace?: string }
    result: boolean
  }
  opencode_session_create: {
    args: { directory?: string; workspace?: string; title?: string; parentID?: string; permission?: unknown } | undefined
    result: { id: string; title?: string }
  }
  opencode_session_messages: {
    args: { directory?: string; workspace?: string; sessionID: string; limit?: number; before?: string }
    result: Array<{ info: unknown; parts: unknown[] }>
  }
  opencode_session_prompt: {
    args: {
      directory?: string
      workspace?: string
      sessionID?: string | null
      title?: string
      prompt?: string
      parts?: unknown[]
      providerID?: string
      modelID?: string
      agent?: string
      variant?: string
      messageID?: string
      noReply?: boolean
    }
    result: ElectronOpencodePromptResult
  }
  opencode_session_prompt_async: {
    args: {
      directory?: string
      workspace?: string
      sessionID?: string | null
      title?: string
      prompt?: string
      parts?: unknown[]
      providerID?: string
      modelID?: string
      agent?: string
      variant?: string
      messageID?: string
      noReply?: boolean
    }
    result: {
      sessionID: string
      requestMessageID: string
    }
  }
  opencode_session_prompt_status: {
    args: {
      directory?: string
      workspace?: string
      sessionID: string
      requestMessageID: string
    }
    result: {
      sessionID: string
      requestMessageID: string
      found: boolean
      completed: boolean
      content: string
      toolCalls: NonNullable<ElectronOpencodePromptResult["toolCalls"]>
      error?: unknown
      assistantMessageID?: string | null
    }
  }
  opencode_session_abort: {
    args: { directory?: string; workspace?: string; sessionID: string }
    result: unknown
  }
  cleanup_runtime: { args: undefined; result: void }
  reveal_in_finder: { args: { path: string }; result: void }
  open_external_url: { args: { url: string }; result: void }
  open_with_app: { args: { target: string; path: string }; result: void }
  show_open_dialog: { args: ElectronOpenDialogOptions; result: string | string[] | null }
}
