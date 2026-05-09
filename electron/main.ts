import { app, BrowserWindow, dialog, ipcMain, shell } from "electron";
import path from "node:path";
import fs from "node:fs/promises";
import fsSync from "node:fs";
import os from "node:os";
import { spawn as spawnProcess, execFile } from "node:child_process";
import { promisify } from "node:util";
import pty from "@lydell/node-pty";
import chokidar from "chokidar";
import crypto from "node:crypto";
import { fileURLToPath, pathToFileURL } from "node:url";

const execFileAsync = promisify(execFile);
const isDev = !app.isPackaged;
const MAX_EDITOR_PREVIEW_BYTES = 512 * 1024;
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

type ProjectWatcher = Awaited<ReturnType<typeof chokidar.watch>> | null;
let mainWindow: BrowserWindow | null = null;
let projectWatcher: ProjectWatcher = null;
let lastWatcherOperationAt = 0;
const ptyProcesses = new Map<string, any>();
const ptyOutputQueues = new Map<string, { chunks: string[]; scheduled: boolean }>();

type OpencodeListener = {
  hostname?: string;
  port?: number;
  url?: URL | string;
  stop?: () => Promise<void> | void;
  close?: () => Promise<void> | void;
};

type OpencodeRuntime = {
  listener: OpencodeListener;
  hostname: string;
  port: number;
  url: string;
  username: string;
  password: string;
  modulePath: string;
  startedAt: number;
};

type OpencodeEventSubscription = {
  id: string;
  channel: string;
  controller: AbortController;
  directory?: string | null;
  workspace?: string | null;
  global: boolean;
  startedAt: number;
  queue: OpencodeEventEnvelope[];
  coalesced: Map<string, number>;
  staleDeltas: Set<string>;
  flushTimer?: ReturnType<typeof setTimeout>;
  lastFlushAt: number;
};

type OpencodeEventEnvelope = {
  type: "open" | "event" | "error" | "closed";
  event?: unknown;
  directory?: string | null;
  project?: string | null;
  workspace?: string | null;
  sseEvent?: string;
  id?: string;
  retry?: number;
  error?: string;
  reason?: string;
  time: number;
};

let opencodeRuntime: OpencodeRuntime | null = null;
let opencodeStartPromise: Promise<OpencodeRuntime> | null = null;
const opencodeEventSubscriptions = new Map<string, OpencodeEventSubscription>();

function userDataPath(...parts: string[]) {
  return path.join(app.getPath("userData"), ...parts);
}

async function ensureDataDirs() {
  await fs.mkdir(userDataPath("sessions"), { recursive: true });
}

function getOpencodeServerCandidates() {
  const relative = path.join("vendor", "opencode", "server", "dist", "node", "node.js");
  return [
    path.join(process.cwd(), relative),
    path.join(app.getAppPath(), relative),
    path.join(__dirname, "..", relative),
    path.join(process.resourcesPath ?? "", relative),
  ].filter((item, index, all) => item && all.indexOf(item) === index);
}

function resolveOpencodeServerModulePath() {
  for (const candidate of getOpencodeServerCandidates()) {
    if (fsSync.existsSync(candidate)) return candidate;
  }

  throw new Error(
    `OpenCode server bundle was not found. Expected one of: ${getOpencodeServerCandidates().join(", ")}`
  );
}

async function importOpencodeServer() {
  const modulePath = resolveOpencodeServerModulePath();
  const moduleUrl = pathToFileURL(modulePath).href;
  const serverModule = await import(moduleUrl) as {
    Log: { init: (options: { level: string }) => Promise<void> | void };
    Server: {
      listen: (options: {
        hostname: string;
        port: number;
        username: string;
        password: string;
      }) => Promise<OpencodeListener>;
    };
  };

  return { modulePath, ...serverModule };
}

function prepareOpencodeServerEnv(password: string) {
  Object.assign(process.env, {
    OPENCODE_EXPERIMENTAL_ICON_DISCOVERY: "true",
    OPENCODE_EXPERIMENTAL_FILEWATCHER: "true",
    OPENCODE_CLIENT: "desktop",
    OPENCODE_SERVER_USERNAME: "opencode",
    OPENCODE_SERVER_PASSWORD: password,
    XDG_STATE_HOME: app.getPath("userData"),
  });
}

function getOpencodeAuthHeader(password = opencodeRuntime?.password) {
  if (!password) return null;
  return `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}`;
}

async function checkOpencodeHealth(url: string, password?: string | null) {
  try {
    const healthUrl = new URL("/global/health", url);
    const headers = new Headers();
    const authorization = getOpencodeAuthHeader(password ?? undefined);
    if (authorization) headers.set("authorization", authorization);
    const response = await fetch(healthUrl, {
      method: "GET",
      headers,
      signal: AbortSignal.timeout(3000),
    });
    return response.ok;
  } catch {
    return false;
  }
}

function normalizeOpencodeUrl(listener: OpencodeListener, hostname: string, port: number) {
  const url = listener.url ? String(listener.url) : `http://${hostname}:${port}`;
  return url.replace(/\/$/, "");
}

async function getOpencodeServerStatus(includeCredentials = false) {
  const runtime = opencodeRuntime;
  const healthy = runtime ? await checkOpencodeHealth(runtime.url, runtime.password) : false;

  if (!runtime) {
    return { running: false, healthy: false };
  }

  return {
    running: true,
    healthy,
    url: runtime.url,
    hostname: runtime.hostname,
    port: runtime.port,
    username: runtime.username,
    password: includeCredentials ? runtime.password : undefined,
    modulePath: runtime.modulePath,
    startedAt: runtime.startedAt,
  };
}

async function startOpencodeServer(payload: { hostname?: string; port?: number; password?: string } = {}) {
  if (opencodeStartPromise) return opencodeStartPromise;

  opencodeStartPromise = (async () => {
    if (opencodeRuntime && await checkOpencodeHealth(opencodeRuntime.url, opencodeRuntime.password)) {
      emitOpencodeLog("debug", "Reusing healthy embedded server", { url: opencodeRuntime.url });
      return opencodeRuntime;
    }

    await stopOpencodeServer();
    await ensureDataDirs();

    const hostname = payload.hostname?.trim() || "127.0.0.1";
    const port = Number.isFinite(Number(payload.port)) ? Math.max(0, Number(payload.port)) : 0;
    const password = payload.password?.trim() || crypto.randomBytes(18).toString("base64url");

    prepareOpencodeServerEnv(password);
    emitOpencodeLog("info", "Importing embedded OpenCode server", {
      candidates: getOpencodeServerCandidates(),
    });
    const { modulePath, Log, Server } = await importOpencodeServer();
    await Log.init({ level: "DEBUG" });
    emitOpencodeLog("info", "Starting embedded OpenCode server", {
      hostname,
      port,
      modulePath,
      stateHome: app.getPath("userData"),
    });

    const listener = await Server.listen({
      hostname,
      port,
      username: "opencode",
      password,
    });

    const resolvedPort = Number(listener.port ?? port);
    const runtime: OpencodeRuntime = {
      listener,
      hostname: listener.hostname ?? hostname,
      port: resolvedPort,
      url: normalizeOpencodeUrl(listener, listener.hostname ?? hostname, resolvedPort),
      username: "opencode",
      password,
      modulePath,
      startedAt: Date.now(),
    };

    const startedAt = Date.now();
    while (Date.now() - startedAt < 10_000) {
      if (await checkOpencodeHealth(runtime.url, password)) {
        opencodeRuntime = runtime;
        emitOpencodeLog("info", "Embedded OpenCode server is healthy", {
          url: runtime.url,
          modulePath,
          elapsedMs: Date.now() - startedAt,
        });
        return runtime;
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }

    await stopOpencodeListener(listener);
    emitOpencodeLog("error", "Embedded OpenCode server did not become healthy", { url: runtime.url });
    throw new Error(`OpenCode server started but did not become healthy at ${runtime.url}`);
  })();

  try {
    return await opencodeStartPromise;
  } finally {
    opencodeStartPromise = null;
  }
}

async function stopOpencodeListener(listener?: OpencodeListener | null) {
  if (!listener) return;
  if (typeof listener.stop === "function") {
    await listener.stop();
    return;
  }
  if (typeof listener.close === "function") {
    await listener.close();
  }
}

async function stopOpencodeServer() {
  const runtime = opencodeRuntime;
  opencodeRuntime = null;
  stopAllOpencodeEventStreams("server stopping");
  if (runtime) {
    emitOpencodeLog("info", "Stopping embedded OpenCode server", { url: runtime.url });
    const authorization = getOpencodeAuthHeader(runtime.password);
    const headers = new Headers();
    if (authorization) headers.set("authorization", authorization);
    await fetch(new URL("/global/dispose", runtime.url), {
      method: "POST",
      headers,
      signal: AbortSignal.timeout(3000),
    }).catch(() => undefined);
  }
  await stopOpencodeListener(runtime?.listener);
}

function buildOpencodeUrl(runtime: OpencodeRuntime, endpoint: string, query?: Record<string, unknown>) {
  const url = new URL(endpoint, `${runtime.url}/`);
  for (const [key, value] of Object.entries(query ?? {})) {
    if (value === undefined || value === null || value === "") continue;
    url.searchParams.set(key, String(value));
  }
  return url;
}

async function parseOpencodeResponse(response: Response) {
  if (response.status === 204) return null;
  const text = await response.text();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

function formatOpencodeError(status: number, payload: unknown) {
  if (typeof payload === "string") return payload || `OpenCode request failed (${status})`;
  if (payload && typeof payload === "object") {
    const data = payload as any;
    const nested =
      data?.error?.message
      ?? data?.data?.message
      ?? data?.message
      ?? (typeof data?.error === "string" ? data.error : undefined);
    if (nested) return nested;
    try {
      return JSON.stringify(data);
    } catch {
      return `OpenCode request failed (${status})`;
    }
  }
  return `OpenCode request failed (${status})`;
}

function formatIpcError(error: unknown) {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (error && typeof error === "object") {
    const data = error as any;
    const nested =
      data?.message
      ?? data?.error?.message
      ?? data?.data?.message
      ?? (typeof data?.error === "string" ? data.error : undefined);
    if (nested) return nested;
    try {
      return JSON.stringify(data);
    } catch {
      return String(error);
    }
  }
  return String(error);
}

function emitNativeEvent(channel: string, payload: unknown) {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  mainWindow.webContents.send("shob:event", { channel, payload });
}

function emitOpencodeLog(level: "debug" | "info" | "warn" | "error", message: string, meta?: unknown) {
  const payload = { level, message, meta, time: Date.now() };
  const method = level === "debug" ? "log" : level;
  console[method](`[opencode] ${message}`, meta ?? "");
  emitNativeEvent("opencode-log", payload);
}

async function opencodeRequest<T = any>(
  method: string,
  endpoint: string,
  options: {
    directory?: string | null;
    workspace?: string | null;
    query?: Record<string, unknown>;
    body?: unknown;
    timeoutMs?: number | null;
  } = {},
): Promise<T> {
  const runtime = await startOpencodeServer();
  const url = buildOpencodeUrl(runtime, endpoint, {
    ...options.query,
    directory: options.directory,
    workspace: options.workspace,
  });
  const headers = new Headers();
  const authorization = getOpencodeAuthHeader(runtime.password);
  if (authorization) headers.set("authorization", authorization);
  if (options.body !== undefined) headers.set("content-type", "application/json");

  const response = await fetch(url, {
    method,
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
    signal: options.timeoutMs && options.timeoutMs > 0 ? AbortSignal.timeout(options.timeoutMs) : undefined,
  });
  const payload = await parseOpencodeResponse(response);
  if (!response.ok) throw new Error(formatOpencodeError(response.status, payload));
  return payload as T;
}

function stopAllOpencodeEventStreams(reason = "stopped") {
  for (const subscription of opencodeEventSubscriptions.values()) {
    flushOpencodeEventQueue(subscription);
    subscription.controller.abort(reason);
    emitNativeEvent(subscription.channel, {
      type: "closed",
      reason,
      time: Date.now(),
    });
  }
  opencodeEventSubscriptions.clear();
}

function stopOpencodeEventStream(id: string, reason = "unsubscribed") {
  const subscription = opencodeEventSubscriptions.get(id);
  if (!subscription) return false;
  flushOpencodeEventQueue(subscription);
  subscription.controller.abort(reason);
  opencodeEventSubscriptions.delete(id);
  emitNativeEvent(subscription.channel, {
    type: "closed",
    reason,
    time: Date.now(),
  });
  emitOpencodeLog("debug", "Stopped OpenCode event stream", {
    id,
    directory: subscription.directory,
    reason,
  });
  return true;
}

function normalizeComparablePath(value?: string | null) {
  if (!value) return "";
  const normalized = path.normalize(value).replace(/[\\/]+$/, "");
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

function isSameOpencodeDirectory(left?: string | null, right?: string | null) {
  if (!left || !right) return true;
  return normalizeComparablePath(left) === normalizeComparablePath(right);
}

function parseSseFrame(frame: string) {
  const dataLines: string[] = [];
  let eventName: string | undefined;
  let eventID: string | undefined;
  let retry: number | undefined;

  for (const line of frame.split("\n")) {
    if (line.startsWith("data:")) {
      dataLines.push(line.replace(/^data:\s*/, ""));
    } else if (line.startsWith("event:")) {
      eventName = line.replace(/^event:\s*/, "");
    } else if (line.startsWith("id:")) {
      eventID = line.replace(/^id:\s*/, "");
    } else if (line.startsWith("retry:")) {
      const parsed = Number.parseInt(line.replace(/^retry:\s*/, ""), 10);
      if (!Number.isNaN(parsed)) retry = parsed;
    }
  }

  if (dataLines.length === 0) return undefined;
  const raw = dataLines.join("\n");
  let data: unknown = raw;
  try {
    data = JSON.parse(raw);
  } catch {
    // Text SSE payloads are valid too.
  }

  return { data, event: eventName, id: eventID, retry };
}

function unwrapOpencodeStreamEvent(data: unknown, subscription: OpencodeEventSubscription) {
  const record = data && typeof data === "object" ? data as Record<string, unknown> : null;
  if (record && "payload" in record) {
    return {
      event: record.payload,
      directory: typeof record.directory === "string" ? record.directory : null,
      project: typeof record.project === "string" ? record.project : null,
      workspace: typeof record.workspace === "string" ? record.workspace : null,
    };
  }

  return {
    event: data,
    directory: subscription.directory ?? null,
    project: null,
    workspace: subscription.workspace ?? null,
  };
}

function opencodeEventType(event: unknown) {
  return event && typeof event === "object" && "type" in event
    ? String((event as { type?: unknown }).type)
    : undefined;
}

function asObjectRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

function opencodeEventQueueKey(envelope: OpencodeEventEnvelope) {
  const event = asObjectRecord(envelope.event);
  const directory = envelope.directory ?? "";
  const properties = asObjectRecord(event?.properties);
  if (!event) return undefined;
  if (event.type === "session.status") return `session.status:${directory}:${properties?.sessionID ?? ""}`;
  if (event.type === "lsp.updated") return `lsp.updated:${directory}`;
  if (event.type === "message.part.updated") {
    const part = asObjectRecord(properties?.part);
    if (part?.messageID && part?.id) return `message.part.updated:${directory}:${part.messageID}:${part.id}`;
  }
  return undefined;
}

function opencodeDeltaKey(envelope: OpencodeEventEnvelope) {
  const event = asObjectRecord(envelope.event);
  if (event?.type !== "message.part.delta") return undefined;
  const props = asObjectRecord(event.properties);
  if (!props?.messageID || !props?.partID) return undefined;
  return `${envelope.directory ?? ""}:${props.messageID}:${props.partID}`;
}

function flushOpencodeEventQueue(subscription: OpencodeEventSubscription) {
  if (subscription.flushTimer) clearTimeout(subscription.flushTimer);
  subscription.flushTimer = undefined;
  if (subscription.queue.length === 0) return;

  const events = subscription.queue;
  const staleDeltas = subscription.staleDeltas.size > 0 ? new Set(subscription.staleDeltas) : undefined;
  subscription.queue = [];
  subscription.coalesced.clear();
  subscription.staleDeltas.clear();
  subscription.lastFlushAt = Date.now();

  for (const envelope of events) {
    const delta = staleDeltas ? opencodeDeltaKey(envelope) : undefined;
    if (delta && staleDeltas?.has(delta)) continue;
    emitNativeEvent(subscription.channel, envelope);
  }
}

function queueOpencodeEvent(subscription: OpencodeEventSubscription, envelope: OpencodeEventEnvelope) {
  if (envelope.type !== "event") {
    emitNativeEvent(subscription.channel, envelope);
    return;
  }

  const key = opencodeEventQueueKey(envelope);
  if (key) {
    const existing = subscription.coalesced.get(key);
    if (existing !== undefined) {
      subscription.queue[existing] = envelope;
      const event = asObjectRecord(envelope.event);
      if (event?.type === "message.part.updated") {
        const part = asObjectRecord(asObjectRecord(event.properties)?.part);
        if (part?.messageID && part?.id) {
          subscription.staleDeltas.add(`${envelope.directory ?? ""}:${part.messageID}:${part.id}`);
        }
      }
    } else {
      subscription.coalesced.set(key, subscription.queue.length);
      subscription.queue.push(envelope);
    }
  } else {
    subscription.queue.push(envelope);
  }

  if (subscription.flushTimer) return;
  const elapsed = Date.now() - subscription.lastFlushAt;
  subscription.flushTimer = setTimeout(
    () => flushOpencodeEventQueue(subscription),
    Math.max(0, 16 - elapsed),
  );
}

async function runOpencodeEventStream(subscription: OpencodeEventSubscription) {
  let attempts = 0;
  let lastEventID: string | undefined;

  while (!subscription.controller.signal.aborted) {
    attempts += 1;
    try {
      const runtime = await startOpencodeServer();
      const url = subscription.global
        ? buildOpencodeUrl(runtime, "/global/event")
        : buildOpencodeUrl(runtime, "/event", {
            directory: subscription.directory,
            workspace: subscription.workspace,
          });
      const headers = new Headers();
      const authorization = getOpencodeAuthHeader(runtime.password);
      if (authorization) headers.set("authorization", authorization);
      if (lastEventID) headers.set("last-event-id", lastEventID);

      emitOpencodeLog("debug", "Opening OpenCode event stream", {
        id: subscription.id,
        url: url.toString(),
        directory: subscription.directory,
        global: subscription.global,
      });

      const response = await fetch(url, {
        headers,
        signal: subscription.controller.signal,
      });
      if (!response.ok) throw new Error(`OpenCode event stream failed: ${response.status} ${response.statusText}`);
      if (!response.body) throw new Error("OpenCode event stream did not return a body");

      queueOpencodeEvent(subscription, {
        type: "open",
        time: Date.now(),
      });

      attempts = 0;
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";

      try {
        while (!subscription.controller.signal.aborted) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          buffer = buffer.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
          const frames = buffer.split("\n\n");
          buffer = frames.pop() ?? "";

          for (const frame of frames) {
            const parsed = parseSseFrame(frame);
            if (!parsed) continue;
            if (parsed.id) lastEventID = parsed.id;

            const streamEvent = unwrapOpencodeStreamEvent(parsed.data, subscription);
            if (!isSameOpencodeDirectory(streamEvent.directory, subscription.directory)) continue;

            const eventType = opencodeEventType(streamEvent.event) ?? parsed.event;
            if (eventType && eventType !== "message.part.delta") {
              emitOpencodeLog("debug", "OpenCode event", {
                id: subscription.id,
                event: eventType,
                directory: streamEvent.directory ?? subscription.directory,
              });
            }

            queueOpencodeEvent(subscription, {
              type: "event",
              event: streamEvent.event,
              directory: streamEvent.directory,
              project: streamEvent.project,
              workspace: streamEvent.workspace,
              sseEvent: parsed.event,
              id: parsed.id,
              retry: parsed.retry,
              time: Date.now(),
            });
          }
        }
      } finally {
        reader.releaseLock();
      }
    } catch (error) {
      if (subscription.controller.signal.aborted) break;
      emitOpencodeLog("warn", "OpenCode event stream interrupted; reconnecting", {
        id: subscription.id,
        directory: subscription.directory,
        error: formatIpcError(error),
      });
      queueOpencodeEvent(subscription, {
        type: "error",
        error: formatIpcError(error),
        time: Date.now(),
      });
      await new Promise((resolve) => setTimeout(resolve, Math.min(1000 + attempts * 250, 5000)));
    }
  }
}

async function startOpencodeEventStream({
  directory,
  workspace,
  global = true,
}: { directory?: string | null; workspace?: string | null; global?: boolean } = {}) {
  await startOpencodeServer();
  const id = crypto.randomUUID();
  const subscription: OpencodeEventSubscription = {
    id,
    channel: `opencode-event:${id}`,
    controller: new AbortController(),
    directory,
    workspace,
    global,
    startedAt: Date.now(),
    queue: [],
    coalesced: new Map(),
    staleDeltas: new Set(),
    lastFlushAt: 0,
  };

  opencodeEventSubscriptions.set(id, subscription);
  emitOpencodeLog("info", "Starting OpenCode event stream", {
    id,
    directory,
    workspace,
    global,
  });
  void runOpencodeEventStream(subscription).finally(() => {
    flushOpencodeEventQueue(subscription);
    opencodeEventSubscriptions.delete(id);
    emitNativeEvent(subscription.channel, {
      type: "closed",
      reason: "ended",
      time: Date.now(),
    });
  });

  return {
    id,
    channel: subscription.channel,
    directory,
    workspace,
    global,
    startedAt: subscription.startedAt,
  };
}

function extractOpenCodeText(parts: any[] | undefined) {
  return (parts ?? [])
    .filter((part) => part?.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("\n")
    .trim();
}

function extractOpenCodeToolCalls(parts: any[] | undefined) {
  return (parts ?? [])
    .filter((part) => part?.type === "tool")
    .map((part) => {
      const state = part?.state ?? {};
      return {
        id: part?.id ?? null,
        callID: part?.callID ?? null,
        tool: part?.tool ?? "tool",
        status: state?.status ?? "pending",
        title: typeof state?.title === "string" ? state.title : null,
        input: state?.input ?? null,
        output: typeof state?.output === "string" ? state.output : null,
        error: typeof state?.error === "string" ? state.error : null,
        raw: typeof state?.raw === "string" ? state.raw : null,
        metadata: state?.metadata && typeof state.metadata === "object" ? state.metadata : null,
        attachments: Array.isArray(state?.attachments) ? state.attachments : null,
        startedAt: typeof state?.time?.start === "number" ? state.time.start : null,
        endedAt: typeof state?.time?.end === "number" ? state.time.end : null,
        compactedAt: typeof state?.time?.compacted === "number" ? state.time.compacted : null,
      };
    });
}

function extractAssistantSnapshot(messages: any[] | undefined, requestMessageID: string) {
  const list = Array.isArray(messages) ? messages : [];
  const match = list
    .filter((item) => item?.info?.role === "assistant" && item?.info?.parentID === requestMessageID)
    .sort((left, right) => Number((right?.info?.time?.created ?? 0)) - Number((left?.info?.time?.created ?? 0)))[0];

  if (!match) {
    return {
      found: false,
      completed: false,
      content: "",
      toolCalls: [],
      error: null,
      assistantMessageID: null,
    };
  }

  return {
    found: true,
    completed: typeof match?.info?.time?.completed === "number",
    content: extractOpenCodeText(match?.parts),
    toolCalls: extractOpenCodeToolCalls(match?.parts),
    error: match?.info?.error ?? null,
    assistantMessageID: match?.info?.id ?? null,
  };
}

function buildOpenCodeMessageID(input?: string | null) {
  const trimmed = input?.trim();
  if (trimmed && trimmed.startsWith("msg")) return trimmed;
  const suffix = trimmed && trimmed.length > 0 ? trimmed : crypto.randomUUID();
  return `msg_${suffix}`;
}

async function readJson<T>(filePath: string, fallback: T): Promise<T> {
  try {
    return JSON.parse(await fs.readFile(filePath, "utf8"));
  } catch {
    return fallback;
  }
}

async function writeJsonAtomic(filePath: string, value: unknown) {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const tempPath = `${filePath}.${process.pid}.tmp`;
  await fs.writeFile(tempPath, JSON.stringify(value, null, 2));
  await fs.rename(tempPath, filePath);
}

function projectsPath() {
  return userDataPath("projects.json");
}

async function loadProjects() {
  return readJson(projectsPath(), []);
}

async function saveProjects(projects: unknown[]) {
  await writeJsonAtomic(projectsPath(), projects);
}

function sessionOutputPath(sessionId: string) {
  const safeId = String(sessionId).replace(/[^a-zA-Z0-9._-]/g, "_");
  return userDataPath("sessions", `${safeId}.log`);
}

function normalizeOs() {
  if (process.platform === "win32") return "windows";
  if (process.platform === "darwin") return "macos";
  if (process.platform === "linux") return "linux";
  return process.platform;
}

function getImageMimeType(filePath: string) {
  switch (path.extname(filePath).toLowerCase()) {
    case ".png": return "image/png";
    case ".jpg":
    case ".jpeg": return "image/jpeg";
    case ".webp": return "image/webp";
    case ".svg": return "image/svg+xml";
    case ".ico": return "image/x-icon";
    case ".gif": return "image/gif";
    case ".bmp": return "image/bmp";
    default: return "application/octet-stream";
  }
}

function splitPathEntries() {
  return (process.env.PATH || "")
    .split(path.delimiter)
    .map((entry) => entry.trim())
    .filter(Boolean);
}

function resolveCommand(command: string) {
  const direct = path.resolve(command);
  if (path.isAbsolute(command) && fsSync.existsSync(command)) return command;
  if (fsSync.existsSync(direct) && path.isAbsolute(command)) return direct;

  const pathExts = process.platform === "win32"
    ? (process.env.PATHEXT || ".COM;.EXE;.BAT;.CMD").split(";").filter(Boolean)
    : [""];
  const hasExt = Boolean(path.extname(command));

  for (const dir of splitPathEntries()) {
    const candidates = process.platform === "win32" && !hasExt
      ? pathExts.map((ext) => path.join(dir, `${command}${ext.toLowerCase()}`))
          .concat(pathExts.map((ext) => path.join(dir, `${command}${ext.toUpperCase()}`)))
      : [path.join(dir, command)];

    for (const candidate of candidates) {
      if (fsSync.existsSync(candidate) && fsSync.statSync(candidate).isFile()) {
        return candidate;
      }
    }
  }

  return null;
}

function detectShells() {
  const shells: string[] = [];
  if (process.platform === "win32") {
    for (const command of ["pwsh.exe", "powershell.exe", "cmd.exe"]) {
      shells.push(resolveCommand(command) || command);
    }
    for (const gitShell of [
      "C:\\Program Files\\Git\\bin\\bash.exe",
      "C:\\Program Files\\Git\\bin\\sh.exe",
    ]) {
      if (fsSync.existsSync(gitShell)) shells.push(gitShell);
    }
  } else {
    for (const command of ["bash", "zsh", "fish", "sh"]) {
      const resolved = resolveCommand(command);
      if (resolved) shells.push(resolved);
    }
  }

  return [...new Set(shells)];
}

async function detectWindowsBuildNumber() {
  if (process.platform !== "win32") return null;
  try {
    const { stdout } = await execFileAsync("cmd", ["/C", "ver"]);
    const token = stdout.split(/\s+/).find((part) => /\d+\.\d+\.\d+/.test(part));
    return token ? Number(token.replace(/\[|\]/g, "").split(".")[2]) || null : null;
  } catch {
    return null;
  }
}

async function gitOutput(args: string[], cwd: string) {
  const { stdout } = await execFileAsync("git", args, { cwd, windowsHide: true, maxBuffer: 20 * 1024 * 1024 });
  return stdout;
}

function parseRepoNameFromRemoteUrl(remoteUrl: string) {
  const trimmed = remoteUrl.trim().replace(/\/$/, "");
  if (!trimmed) return null;
  const tail = trimmed.split(/[/:]/).pop()?.replace(/\.git$/, "").trim();
  return tail || null;
}

async function listDirectory(directoryPath: string) {
  const entries = await fs.readdir(directoryPath, { withFileTypes: true });
  return entries
    .map((entry) => ({
      name: entry.name,
      path: path.join(directoryPath, entry.name),
      isDirectory: entry.isDirectory(),
    }))
    .sort((left, right) => {
      if (left.isDirectory !== right.isDirectory) return left.isDirectory ? -1 : 1;
      return left.name.toLowerCase().localeCompare(right.name.toLowerCase());
    });
}

function shouldForwardProjectWatchPath(changedPath: string) {
  const normalized = changedPath.replace(/\\/g, "/").toLowerCase();
  if (/\/(node_modules|dist|target|\.next|\.turbo|\.cache|coverage)\//.test(normalized)) return false;
  if (normalized.includes("/.git/")) {
    return normalized.endsWith("/.git/head")
      || normalized.endsWith("/.git/index")
      || normalized.endsWith("/.git/refs")
      || normalized.includes("/.git/refs/");
  }
  return true;
}

function emitProjectFsEvent(projectPath: string, paths: string[]) {
  emitNativeEvent("project-fs-event", { projectPath, paths });
}

async function setProjectWatch(watchPath: string | null) {
  if (projectWatcher) {
    await projectWatcher.close();
    projectWatcher = null;
  }

  if (!watchPath) {
    lastWatcherOperationAt = Date.now();
    return;
  }

  const stats = await fs.stat(watchPath);
  if (!stats.isDirectory()) throw new Error("Project watch path is invalid");

  if (process.platform === "win32") {
    const elapsed = Date.now() - lastWatcherOperationAt;
    if (elapsed < 200) await new Promise((resolve) => setTimeout(resolve, 200 - elapsed));
  }

  const pendingPaths = new Set<string>();
  let flushTimer: NodeJS.Timeout | null = null;
  const scheduleFlush = () => {
    if (flushTimer) return;
    flushTimer = setTimeout(() => {
      flushTimer = null;
      const paths = [...pendingPaths];
      pendingPaths.clear();
      if (paths.length) emitProjectFsEvent(watchPath, paths);
    }, 120);
  };

  projectWatcher = chokidar.watch(watchPath, {
    ignoreInitial: true,
    persistent: true,
    awaitWriteFinish: { stabilityThreshold: 80, pollInterval: 25 },
    ignored: /(^|[\\/])(node_modules|dist|target|\.next|\.turbo|\.cache|coverage)([\\/]|$)/,
  });

  projectWatcher.on("all", (_event: string, changedPath: string) => {
    if (!shouldForwardProjectWatchPath(changedPath)) return;
    pendingPaths.add(changedPath);
    scheduleFlush();
  });

  lastWatcherOperationAt = Date.now();
}

function queuePtyOutput(id: string, data: string) {
  const item = ptyOutputQueues.get(id);
  if (!item) return;
  item.chunks.push(data);
  if (item.scheduled) return;
  item.scheduled = true;
  setImmediate(() => {
    item.scheduled = false;
    if (!mainWindow || mainWindow.isDestroyed() || item.chunks.length === 0) return;
    const payload = item.chunks.join("");
    item.chunks.length = 0;
    mainWindow.webContents.send("shob:terminal-data", { id, data: payload });
  });
}

function killPty(id: string) {
  const proc = ptyProcesses.get(id);
  if (!proc) return;
  ptyProcesses.delete(id);
  ptyOutputQueues.delete(id);
  try {
    proc.kill();
  } catch {
    // best effort cleanup
  }
}

function killAllPtys() {
  for (const id of [...ptyProcesses.keys()]) killPty(id);
}

async function revealInFinder(targetPath: string) {
  if (!fsSync.existsSync(targetPath)) throw new Error("Path does not exist");
  shell.showItemInFolder(targetPath);
}

function quotePowerShellLiteral(value: string) {
  return `'${value.replace(/'/g, "''")}'`;
}

function launchDetached(command: string, args: string[], cwd: string) {
  const isWindowsScript = process.platform === "win32" && /\.(cmd|bat)$/i.test(command);
  const child = isWindowsScript
    ? spawnProcess("cmd.exe", ["/d", "/s", "/c", "start", "", command, ...args], {
        cwd,
        detached: true,
        stdio: "ignore",
        windowsHide: true,
      })
    : spawnProcess(command, args, {
        cwd,
        detached: true,
        stdio: "ignore",
        windowsHide: false,
      });

  child.unref();
}

function resolveFirstCommand(commands: string[]) {
  for (const command of commands) {
    const resolved = resolveCommand(command);
    if (resolved) return resolved;
  }

  return null;
}

function openCommand(commands: string[], args: string[], cwd: string) {
  const resolved = resolveFirstCommand(commands);
  if (!resolved) throw new Error(`No matching command found: ${commands.join(", ")}`);
  launchDetached(resolved, args, cwd);
}

function openExternalTerminal(cwd: string) {
  if (process.platform === "win32") {
    const windowsTerminal = resolveCommand("wt.exe") || resolveCommand("wt");
    if (windowsTerminal) {
      launchDetached(windowsTerminal, ["-d", cwd], cwd);
      return;
    }

    const powershell = resolveCommand("pwsh.exe") || resolveCommand("powershell.exe") || "powershell.exe";
    launchDetached(powershell, ["-NoExit", "-Command", `Set-Location -LiteralPath ${quotePowerShellLiteral(cwd)}`], cwd);
    return;
  }

  if (process.platform === "darwin") {
    launchDetached("open", ["-a", "Terminal", cwd], cwd);
    return;
  }

  const linuxTerminal = resolveFirstCommand(["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "xterm"]);
  if (!linuxTerminal) throw new Error("No external terminal found");
  if (path.basename(linuxTerminal).toLowerCase().includes("gnome-terminal")) {
    launchDetached(linuxTerminal, ["--working-directory", cwd], cwd);
    return;
  }
  launchDetached(linuxTerminal, [], cwd);
}

async function openWithApp(target: string, cwd: string) {
  const stats = await fs.stat(cwd);
  if (!stats.isDirectory()) throw new Error("Project path is not a directory");

  switch (target) {
    case "vscode":
      openCommand(["code", "code.cmd", "code.exe"], [cwd], cwd);
      return;
    case "zed":
      openCommand(["zed", "zed.cmd", "zed.exe"], [cwd], cwd);
      return;
    case "antigravity":
      openCommand(["antigravity", "antigravity.cmd", "antigravity.exe"], [cwd], cwd);
      return;
    case "file-explorer":
      await shell.openPath(cwd);
      return;
    case "terminal":
      openExternalTerminal(cwd);
      return;
    case "git-bash": {
      const gitBash = resolveFirstCommand([
        "C:\\Program Files\\Git\\git-bash.exe",
        "C:\\Program Files (x86)\\Git\\git-bash.exe",
        "git-bash.exe",
      ]);
      if (!gitBash) throw new Error("Git Bash was not found");
      launchDetached(gitBash, [`--cd=${cwd}`], cwd);
      return;
    }
    case "wsl": {
      const wsl = resolveFirstCommand(["wsl.exe", "wsl"]);
      if (!wsl) throw new Error("WSL was not found");
      launchDetached(wsl, process.platform === "win32" ? ["--cd", cwd] : [], cwd);
      return;
    }
    default:
      throw new Error(`Unknown open-with target: ${target}`);
  }
}

async function getGitStatus(cwd: string) {
  const repoRoot = (await gitOutput(["rev-parse", "--show-toplevel"], cwd)).trim();
  const statusOutput = await gitOutput(["status", "--porcelain"], cwd);
  let numstatOutput = "";
  try {
    numstatOutput = await gitOutput(["diff", "--numstat", "HEAD"], cwd);
  } catch {
    numstatOutput = "";
  }

  const counts = new Map<string, [number, number]>();
  for (const line of numstatOutput.split(/\r?\n/)) {
    const [additions, deletions, filePath] = line.split("\t");
    if (filePath) counts.set(filePath, [Number(additions) || 0, Number(deletions) || 0]);
  }

  const changedFiles = [];
  for (const line of statusOutput.split(/\r?\n/)) {
    if (line.length < 4) continue;
    const status = line.slice(0, 2).trim();
    const relativePath = line.slice(3).trim().replace(/\\/g, "/");
    const absolutePath = path.join(repoRoot, relativePath.split("/").join(path.sep));
    const [additions, deletions] = status === "??"
      ? [countFileLines(absolutePath), 0]
      : (counts.get(relativePath) || [0, 0]);
    changedFiles.push({ path: relativePath, absolutePath, status, additions, deletions });
  }

  return { repoRoot, changedFiles };
}

function countFileLines(filePath: string) {
  try {
    return fsSync.readFileSync(filePath, "utf8").split(/\r?\n/).length;
  } catch {
    return 0;
  }
}

async function readTextFileWithLimit(filePath: string, maxBytes: number) {
  let size = 0;
  try {
    size = (await fs.stat(filePath)).size;
  } catch {
    return { content: "", size: 0, isLarge: false };
  }
  if (size > maxBytes) return { content: "", size, isLarge: true };
  try {
    return { content: await fs.readFile(filePath, "utf8"), size, isLarge: false };
  } catch {
    return { content: "", size, isLarge: false };
  }
}

async function getGitFileState(filePath: string) {
  const current = await readTextFileWithLimit(filePath, MAX_EDITOR_PREVIEW_BYTES);
  const cwd = path.dirname(filePath);
  let repoRoot: string | null = null;
  try {
    repoRoot = (await gitOutput(["rev-parse", "--show-toplevel"], cwd)).trim();
  } catch {
    return {
      repoRoot: null,
      baseContent: "",
      currentContent: current.content,
      hasChanges: false,
      isLargeFile: current.isLarge,
      fileSizeBytes: current.size,
      status: "",
      additions: 0,
      deletions: 0,
    };
  }

  const relativePath = path.relative(repoRoot, filePath).replace(/\\/g, "/");
  const statusLine = (await gitOutput(["status", "--porcelain", "--", relativePath], repoRoot).catch(() => ""))
    .split(/\r?\n/)[0]
    ?.trim() || "";
  const status = statusLine.length >= 2 ? statusLine.slice(0, 2).trim() : "";
  const numstatLine = (await gitOutput(["diff", "--numstat", "HEAD", "--", relativePath], repoRoot).catch(() => ""))
    .split(/\r?\n/)[0] || "";
  const parts = numstatLine.split("\t");
  let additions = Number(parts[0]) || 0;
  let deletions = Number(parts[1]) || 0;
  if (status === "??") {
    additions = current.content.split(/\r?\n/).length;
    deletions = 0;
  }

  let baseContent = "";
  let baseIsLarge = false;
  try {
    const { stdout } = await execFileAsync("git", ["show", `HEAD:${relativePath}`], {
      cwd: repoRoot,
      windowsHide: true,
      maxBuffer: MAX_EDITOR_PREVIEW_BYTES + 1,
      encoding: "buffer",
    });
    if ((stdout as Buffer).length > MAX_EDITOR_PREVIEW_BYTES) {
      baseIsLarge = true;
    } else {
      baseContent = (stdout as Buffer).toString("utf8");
    }
  } catch {
    baseContent = "";
  }

  return {
    repoRoot,
    baseContent,
    currentContent: current.content,
    hasChanges: Boolean(status),
    isLargeFile: current.isLarge || baseIsLarge,
    fileSizeBytes: current.size,
    status,
    additions,
    deletions,
  };
}

const handlers: Record<string, (payload?: any) => Promise<any> | any> = {
  get_projects: async () => loadProjects(),
  save_project: async ({ project }) => {
    const projects = await loadProjects();
    const index = projects.findIndex((item: any) => item.id === project.id);
    if (index >= 0) projects[index] = project;
    else projects.push(project);
    await saveProjects(projects);
    return project;
  },
  delete_project: async ({ projectId }) => {
    const projects = await loadProjects();
    await saveProjects(projects.filter((project: any) => project.id !== projectId));
  },
  save_session_output: async ({ sessionId, output }) => {
    await fs.writeFile(sessionOutputPath(sessionId), output || "");
  },
  load_session_output: async ({ sessionId }) => {
    try {
      return await fs.readFile(sessionOutputPath(sessionId), "utf8");
    } catch {
      return "";
    }
  },
  read_image_data_url: async ({ path: imagePath }) => {
    const bytes = await fs.readFile(imagePath);
    return `data:${getImageMimeType(imagePath)};base64,${bytes.toString("base64")}`;
  },
  get_available_shells: async () => detectShells(),
  get_terminal_host_info: async () => ({
    os: normalizeOs(),
    windowsBuildNumber: await detectWindowsBuildNumber(),
  }),
  probe_cli_tools: async ({ items }) => items.map((item: any) => {
    let resolvedPath: string | null = null;
    let matchedCommand: string | null = null;
    for (const command of item.commands || []) {
      resolvedPath = resolveCommand(command);
      if (resolvedPath) {
        matchedCommand = command;
        break;
      }
    }
    return { id: item.id, installed: Boolean(resolvedPath), resolvedPath, matchedCommand };
  }),
  set_project_watch: async ({ path: watchPath }) => setProjectWatch(watchPath),
  list_directory: async ({ path: directoryPath }) => listDirectory(directoryPath),
  read_text_file: async ({ path: filePath }) => fs.readFile(filePath, "utf8"),
  get_git_status: async ({ path: cwd }) => getGitStatus(cwd),
  get_git_branch: async ({ path: cwd }) => {
    const head = (await gitOutput(["branch", "--show-current"], cwd)).trim();
    const upstream = (await gitOutput(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"], cwd).catch(() => ""))
      .trim() || null;
    const remoteName = (await gitOutput(["config", "--get", `branch.${head}.remote`], cwd).catch(() => ""))
      .trim() || upstream?.split("/")[0] || (await gitOutput(["remote"], cwd).catch(() => "")).split(/\r?\n/).find(Boolean) || null;
    const remoteUrl = remoteName ? await gitOutput(["remote", "get-url", remoteName], cwd).catch(() => "") : "";
    return { repoName: parseRepoNameFromRemoteUrl(remoteUrl), head, upstream };
  },
  get_git_branches: async ({ path: cwd }) => {
    const output = await gitOutput(["for-each-ref", "--format=%(refname:short)", "refs/heads"], cwd);
    return { branches: output.split(/\r?\n/).map((line) => line.trim()).filter(Boolean).sort() };
  },
  switch_git_branch: async ({ path: cwd, branch }) => {
    try {
      await gitOutput(["switch", branch], cwd);
    } catch {
      await gitOutput(["checkout", branch], cwd);
    }
  },
  get_git_file_base: async ({ path: filePath }) => {
    const repoRoot = (await gitOutput(["rev-parse", "--show-toplevel"], path.dirname(filePath))).trim();
    const relativePath = path.relative(repoRoot, filePath).replace(/\\/g, "/");
    return gitOutput(["show", `HEAD:${relativePath}`], repoRoot).catch(() => "");
  },
  get_git_file_state: async ({ path: filePath }) => getGitFileState(filePath),
  opencode_server_status: async (payload = {}) => getOpencodeServerStatus(Boolean(payload.includeCredentials)),
  opencode_server_start: async (payload = {}) => {
    await startOpencodeServer(payload);
    return getOpencodeServerStatus(Boolean(payload.includeCredentials));
  },
  opencode_server_stop: async () => {
    await stopOpencodeServer();
    return getOpencodeServerStatus();
  },
  opencode_provider_list: async ({ directory, workspace } = {}) =>
    opencodeRequest("GET", "/provider", { directory, workspace, timeoutMs: 30_000 }),
  opencode_provider_auth_methods: async ({ directory, workspace } = {}) =>
    opencodeRequest("GET", "/provider/auth", { directory, workspace, timeoutMs: 30_000 }),
  opencode_event_subscribe: async ({ directory, workspace, global } = {}) =>
    startOpencodeEventStream({ directory, workspace, global }),
  opencode_event_unsubscribe: async ({ id }) => {
    if (!id) throw new Error("OpenCode event subscription ID is required");
    return stopOpencodeEventStream(id);
  },
  opencode_session_status: async ({ directory, workspace } = {}) =>
    opencodeRequest("GET", "/session/status", { directory, workspace, timeoutMs: 30_000 }),
  opencode_global_dispose: async () =>
    opencodeRequest("POST", "/global/dispose", { timeoutMs: 30_000 }),
  opencode_config_get: async ({ directory, workspace } = {}) =>
    opencodeRequest("GET", "/config", { directory, workspace, timeoutMs: 30_000 }),
  opencode_config_update: async ({ directory, workspace, config }) =>
    opencodeRequest("PATCH", "/config", {
      directory,
      workspace,
      body: config,
      timeoutMs: 30_000,
    }),
  opencode_global_config_get: async () =>
    opencodeRequest("GET", "/global/config", { timeoutMs: 30_000 }),
  opencode_global_config_update: async ({ config }) =>
    opencodeRequest("PATCH", "/global/config", {
      body: config,
      timeoutMs: 30_000,
    }),
  opencode_auth_set: async ({ providerID, key, auth }) => {
    if (!providerID) throw new Error("Provider ID is required");
    const body = auth ?? { type: "api", key };
    return opencodeRequest("PUT", `/auth/${encodeURIComponent(providerID)}`, {
      body,
      timeoutMs: 30_000,
    });
  },
  opencode_auth_remove: async ({ providerID }) => {
    if (!providerID) throw new Error("Provider ID is required");
    return opencodeRequest("DELETE", `/auth/${encodeURIComponent(providerID)}`, {
      timeoutMs: 30_000,
    });
  },
  opencode_oauth_authorize: async ({ providerID, method = 0, inputs, directory, workspace }) => {
    if (!providerID) throw new Error("Provider ID is required");
    return opencodeRequest("POST", `/provider/${encodeURIComponent(providerID)}/oauth/authorize`, {
      directory,
      workspace,
      body: { method, inputs },
      timeoutMs: 30_000,
    });
  },
  opencode_oauth_callback: async ({ providerID, method = 0, code, directory, workspace }) => {
    if (!providerID) throw new Error("Provider ID is required");
    return opencodeRequest("POST", `/provider/${encodeURIComponent(providerID)}/oauth/callback`, {
      directory,
      workspace,
      body: { method, code },
      timeoutMs: 30_000,
    });
  },
  opencode_session_create: async ({ directory, workspace, title, parentID, permission } = {}) =>
    opencodeRequest("POST", "/session", {
      directory,
      workspace,
      body: { title, parentID, permission },
      timeoutMs: 30_000,
    }),
  opencode_session_messages: async ({ directory, workspace, sessionID, limit, before }) => {
    if (!sessionID) throw new Error("OpenCode session ID is required");
    return opencodeRequest("GET", `/session/${encodeURIComponent(sessionID)}/message`, {
      directory,
      workspace,
      query: { limit, before },
      timeoutMs: 30_000,
    });
  },
  opencode_session_prompt: async ({
    directory,
    workspace,
    sessionID,
    title,
    prompt,
    parts,
    providerID,
    modelID,
    agent,
    variant,
    messageID,
    noReply,
  }) => {
    if (!prompt && (!Array.isArray(parts) || parts.length === 0)) {
      throw new Error("Prompt text or parts are required");
    }

    let resolvedSessionID = sessionID;
    if (!resolvedSessionID) {
      const created = await opencodeRequest<any>("POST", "/session", {
        directory,
        workspace,
        body: { title },
        timeoutMs: 30_000,
      });
      resolvedSessionID = created?.id;
    }
    if (!resolvedSessionID) throw new Error("OpenCode did not return a session ID");

    const model = providerID && modelID ? { providerID, modelID } : undefined;
    const resolvedMessageID = buildOpenCodeMessageID(messageID);
    const result = await opencodeRequest<any>("POST", `/session/${encodeURIComponent(resolvedSessionID)}/message`, {
      directory,
      workspace,
      body: {
        messageID: resolvedMessageID,
        model,
        agent: agent || "build",
        noReply,
        variant,
        parts: Array.isArray(parts) && parts.length > 0 ? parts : [{ type: "text", text: prompt }],
      },
      timeoutMs: null,
    });

    return {
      sessionID: resolvedSessionID,
      message: result,
      content: extractOpenCodeText(result?.parts),
      toolCalls: extractOpenCodeToolCalls(result?.parts),
      error: result?.info?.error ?? null,
    };
  },
  opencode_session_abort: async ({ directory, workspace, sessionID }) => {
    if (!sessionID) throw new Error("OpenCode session ID is required");
    return opencodeRequest("POST", `/session/${encodeURIComponent(sessionID)}/abort`, {
      directory,
      workspace,
      timeoutMs: 30_000,
    });
  },
  opencode_session_prompt_async: async ({
    directory,
    workspace,
    sessionID,
    title,
    prompt,
    parts,
    providerID,
    modelID,
    agent,
    variant,
    messageID,
    noReply,
  }) => {
    if (!prompt && (!Array.isArray(parts) || parts.length === 0)) {
      throw new Error("Prompt text or parts are required");
    }

    let resolvedSessionID = sessionID;
    if (!resolvedSessionID) {
      const created = await opencodeRequest<any>("POST", "/session", {
        directory,
        workspace,
        body: { title },
        timeoutMs: 30_000,
      });
      resolvedSessionID = created?.id;
    }
    if (!resolvedSessionID) throw new Error("OpenCode did not return a session ID");

    const requestMessageID = buildOpenCodeMessageID(messageID);
    const model = providerID && modelID ? { providerID, modelID } : undefined;
    await opencodeRequest("POST", `/session/${encodeURIComponent(resolvedSessionID)}/prompt_async`, {
      directory,
      workspace,
      body: {
        messageID: requestMessageID,
        model,
        agent: agent || "build",
        noReply,
        variant,
        parts: Array.isArray(parts) && parts.length > 0 ? parts : [{ type: "text", text: prompt }],
      },
      timeoutMs: 30_000,
    });

    return {
      sessionID: resolvedSessionID,
      requestMessageID,
    };
  },
  opencode_session_prompt_status: async ({ directory, workspace, sessionID, requestMessageID }) => {
    if (!sessionID) throw new Error("OpenCode session ID is required");
    if (!requestMessageID) throw new Error("OpenCode request message ID is required");

    const messages = await opencodeRequest<any[]>("GET", `/session/${encodeURIComponent(sessionID)}/message`, {
      directory,
      workspace,
      query: { limit: 120 },
      timeoutMs: 30_000,
    });

    const snapshot = extractAssistantSnapshot(messages, requestMessageID);
    return {
      sessionID,
      requestMessageID,
      ...snapshot,
    };
  },
  cleanup_runtime: async () => {
    await setProjectWatch(null);
    await stopOpencodeServer();
    killAllPtys();
  },
  minimize_window: async () => mainWindow?.minimize(),
  toggle_maximize_window: async () => {
    if (!mainWindow) return false;
    if (mainWindow.isMaximized()) {
      mainWindow.unmaximize();
      return false;
    }
    mainWindow.maximize();
    return true;
  },
  is_window_maximized: async () => Boolean(mainWindow?.isMaximized()),
  close_window: async () => mainWindow?.close(),
  reveal_in_finder: async ({ path: targetPath }) => revealInFinder(targetPath),
  open_external_url: async ({ url }) => {
    if (!url || typeof url !== "string") throw new Error("URL is required");
    await shell.openExternal(url);
  },
  open_with_app: async ({ target, path: projectPath }) => openWithApp(target, projectPath),
  show_open_dialog: async (options) => {
    const result = await dialog.showOpenDialog(mainWindow!, {
      title: options?.title,
      properties: [
        options?.directory ? "openDirectory" : "openFile",
        options?.multiple ? "multiSelections" : null,
      ].filter(Boolean) as ("openDirectory" | "openFile" | "multiSelections")[],
      filters: options?.filters,
    });
    if (result.canceled) return options?.multiple ? [] : null;
    return options?.multiple ? result.filePaths : result.filePaths[0] || null;
  },
};

function registerIpc() {
  ipcMain.handle("shob:invoke", async (_event, command, payload = {}) => {
    const handler = handlers[command];
    if (!handler) throw new Error(`Unknown IPC command: ${command}`);
    try {
      return await handler(payload);
    } catch (error) {
      throw new Error(formatIpcError(error));
    }
  });

  ipcMain.handle("shob:terminal-spawn", async (_event, options) => {
    const id = options.id || crypto.randomUUID();
    killPty(id);
    const proc = pty.spawn(options.shell, options.args || [], {
      name: "xterm-256color",
      cwd: options.cwd || os.homedir(),
      cols: Math.max(2, Number(options.cols) || 80),
      rows: Math.max(2, Number(options.rows) || 24),
      env: { ...process.env, ...(options.env || {}) },
    });

    ptyProcesses.set(id, proc);
    ptyOutputQueues.set(id, { chunks: [], scheduled: false });
    proc.onData((data: string) => queuePtyOutput(id, data));
    proc.onExit(() => {
      ptyProcesses.delete(id);
      ptyOutputQueues.delete(id);
      mainWindow?.webContents.send("shob:terminal-exit", { id });
    });

    return { id };
  });

  ipcMain.handle("shob:terminal-write", (_event, id, data) => {
    const proc = ptyProcesses.get(id);
    if (proc) proc.write(data);
  });

  ipcMain.handle("shob:terminal-resize", (_event, id, cols, rows) => {
    const proc = ptyProcesses.get(id);
    if (proc) proc.resize(Math.max(2, Number(cols) || 80), Math.max(2, Number(rows) || 24));
  });

  ipcMain.handle("shob:terminal-kill", (_event, id) => {
    killPty(id);
  });
}

async function createWindow() {
  await ensureDataDirs();
  mainWindow = new BrowserWindow({
    width: 1280,
    height: 820,
    minWidth: 920,
    minHeight: 600,
    frame: false,
    titleBarStyle: "hidden",
    backgroundColor: "#09090b",
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
    },
  });

  mainWindow.on("maximize", () => mainWindow?.webContents.send("shob:window-state", { maximized: true }));
  mainWindow.on("unmaximize", () => mainWindow?.webContents.send("shob:window-state", { maximized: false }));
  mainWindow.on("closed", () => {
    mainWindow = null;
  });

  if (isDev && process.env.VITE_DEV_SERVER_URL) {
    await mainWindow.loadURL(process.env.VITE_DEV_SERVER_URL);
    mainWindow.webContents.openDevTools({ mode: "detach" });
  } else if (isDev) {
    await mainWindow.loadURL("http://localhost:5173");
  } else {
    await mainWindow.loadFile(path.join(__dirname, "..", "dist-renderer", "index.html"));
  }
}

app.setName("shob");
app.whenReady().then(() => {
  registerIpc();
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("before-quit", async () => {
  await handlers.cleanup_runtime();
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
