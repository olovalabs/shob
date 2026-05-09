import { contextBridge, ipcRenderer } from "electron";

const allowedCommands = new Set([
  "get_projects",
  "save_project",
  "delete_project",
  "save_session_output",
  "load_session_output",
  "read_image_data_url",
  "get_available_shells",
  "get_terminal_host_info",
  "probe_cli_tools",
  "set_project_watch",
  "list_directory",
  "read_text_file",
  "get_git_branch",
  "get_git_branches",
  "get_git_status",
  "get_git_file_base",
  "get_git_file_state",
  "switch_git_branch",
  "opencode_server_status",
  "opencode_server_start",
  "opencode_server_stop",
  "opencode_provider_list",
  "opencode_provider_auth_methods",
  "opencode_event_subscribe",
  "opencode_event_unsubscribe",
  "opencode_session_status",
  "opencode_global_dispose",
  "opencode_config_get",
  "opencode_config_update",
  "opencode_global_config_get",
  "opencode_global_config_update",
  "opencode_auth_set",
  "opencode_auth_remove",
  "opencode_oauth_authorize",
  "opencode_oauth_callback",
  "opencode_session_create",
  "opencode_session_messages",
  "opencode_session_prompt",
  "opencode_session_prompt_async",
  "opencode_session_prompt_status",
  "opencode_session_abort",
  "cleanup_runtime",
  "minimize_window",
  "toggle_maximize_window",
  "is_window_maximized",
  "close_window",
  "reveal_in_finder",
  "open_external_url",
  "open_with_app",
  "show_open_dialog",
]);

const eventSubscriptions = new Map<string, Set<(data: any) => void>>();
const terminalDataSubscriptions = new Map<string, Set<(data: string) => void>>();
const terminalExitSubscriptions = new Map<string, Set<() => void>>();

ipcRenderer.on("shob:event", (_event, message) => {
  const listeners = eventSubscriptions.get(message.channel);
  if (!listeners) return;
  for (const listener of listeners) listener({ payload: message.payload });
});

ipcRenderer.on("shob:terminal-data", (_event, message) => {
  const listeners = terminalDataSubscriptions.get(message.id);
  if (!listeners) return;
  for (const listener of listeners) listener(message.data);
});

ipcRenderer.on("shob:terminal-exit", (_event, message) => {
  const listeners = terminalExitSubscriptions.get(message.id);
  if (!listeners) return;
  for (const listener of listeners) listener();
});

function subscribe<T>(map: Map<string, Set<(value: T) => void>>, key: string, callback: (value: T) => void) {
  const listeners = map.get(key) || new Set();
  listeners.add(callback);
  map.set(key, listeners);
  return () => {
    listeners.delete(callback);
    if (listeners.size === 0) map.delete(key);
  };
}

contextBridge.exposeInMainWorld("shob", {
  platform: process.platform === "win32" ? "windows" : process.platform === "darwin" ? "macos" : process.platform,
  invoke(command: string, payload: unknown) {
    if (!allowedCommands.has(command)) {
      return Promise.reject(new Error(`IPC command is not allowed: ${command}`));
    }
    return ipcRenderer.invoke("shob:invoke", command, payload);
  },
  listen(channel: string, callback: (message: unknown) => void) {
    return Promise.resolve(subscribe(eventSubscriptions, channel, callback));
  },
  window: {
    minimize: () => ipcRenderer.invoke("shob:invoke", "minimize_window", {}),
    toggleMaximize: () => ipcRenderer.invoke("shob:invoke", "toggle_maximize_window", {}),
    isMaximized: () => ipcRenderer.invoke("shob:invoke", "is_window_maximized", {}),
    close: () => ipcRenderer.invoke("shob:invoke", "close_window", {}),
    onResized: (callback: () => void) => {
      const listener = () => callback();
      ipcRenderer.on("shob:window-state", listener);
      return Promise.resolve(() => ipcRenderer.removeListener("shob:window-state", listener));
    },
  },
  terminal: {
    spawn: (options: unknown) => ipcRenderer.invoke("shob:terminal-spawn", options),
    write: (id: string, data: string) => ipcRenderer.invoke("shob:terminal-write", id, data),
    resize: (id: string, cols: number, rows: number) => ipcRenderer.invoke("shob:terminal-resize", id, cols, rows),
    kill: (id: string) => ipcRenderer.invoke("shob:terminal-kill", id),
    onData: (id: string, callback: (data: string) => void) => subscribe(terminalDataSubscriptions, id, callback),
    onExit: (id: string, callback: () => void) => subscribe(terminalExitSubscriptions, id, callback),
  },
});
