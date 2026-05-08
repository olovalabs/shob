export interface Project {
  id: string;
  name: string;
  path: string;
  color?: string | null;
  logoPath?: string | null;
  sessions: Session[];
}

export type SessionKind = 'terminal' | 'agent';

export interface AgentMessage {
  id: string;
  role: 'user' | 'assistant';
  content: string;
  createdAt: number;
  toolCalls?: Array<{
    id?: string | null;
    callID?: string | null;
    tool: string;
    status: "pending" | "running" | "completed" | "error" | string;
    title?: string | null;
    input?: unknown;
    output?: string | null;
    error?: string | null;
    startedAt?: number | null;
    endedAt?: number | null;
  }> | null;
}

export interface Session {
  id: string;
  name: string;
  shell: string;
  kind?: SessionKind | null;
  cliTool?: string | null;
  pendingLaunchCommand?: string | null;
  createdAt?: number | null;
  lastActiveAt?: number | null;
  commandCount?: number | null;
  startupDurationMs?: number | null;
  agentMessages?: AgentMessage[] | null;
  opencodeSessionId?: string | null;
  opencodeProviderId?: string | null;
  opencodeModelId?: string | null;
  opencodeModelVariant?: string | null;
}

export interface CliTool {
  id: string;
  label: string;
  iconKey: string;
  default: boolean;
  priority: number;
  installed: boolean;
  resolvedPath: string | null;
  matchedCommand: string | null;
  installCommand: string;
}
