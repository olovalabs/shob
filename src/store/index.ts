import { create } from 'zustand';
import { platform } from '@tauri-apps/plugin-os';
import { api } from '../services/api';
import { STORAGE_KEYS } from '../constants/storage';
import {
  getStoredValue,
  setStoredValue,
  sanitizeSessionName,
  inferSessionCreatedAt,
  inferSessionLastActiveAt,
  normalizeSessionCounter,
  normalizeOptionalDuration,
} from '../utils';
import { CLI_CATALOG, DEFAULT_CLI_ID, type CliProbeResult } from '../config/check';
import type { Project, Session, CliTool } from '../types';

const SESSION_CLEANUP_IDLE_MS = 1000 * 60 * 60 * 24 * 30;
const SESSION_CLEANUP_MAX_PER_PROJECT = 40;
const SESSION_CLEANUP_ALWAYS_KEEP = 5;
const SESSION_ACTIVITY_PERSIST_THROTTLE_MS = 15_000;

const buildCatalogCliTools = (probeResults: CliProbeResult[] = []): CliTool[] => {
  const resultById = new Map(probeResults.map((result) => [result.id, result]));

  return CLI_CATALOG.map((item): CliTool => {
    const probe = resultById.get(item.id);

    return {
      id: item.id,
      label: item.label,
      iconKey: item.iconKey,
      default: item.default,
      priority: item.priority,
      installed: probe?.installed ?? false,
      resolvedPath: probe?.resolvedPath ?? null,
      matchedCommand: probe?.matchedCommand ?? null,
      installCommand: item.installCommand,
    };
  }).sort((left, right) => {
    if (left.installed !== right.installed) {
      return left.installed ? -1 : 1;
    }

    return left.priority - right.priority;
  });
};

const normalizeProjects = (projects: Project[]): Project[] =>
  projects.map((project) => ({
    ...project,
    color: project.color ?? null,
    logoPath: project.logoPath ?? null,
    sessions: project.sessions.map((session) => ({
      ...session,
      name: sanitizeSessionName(session.name) || session.name,
      createdAt: inferSessionCreatedAt(session),
      lastActiveAt: inferSessionLastActiveAt(session),
      commandCount: normalizeSessionCounter(session.commandCount),
      startupDurationMs: normalizeOptionalDuration(session.startupDurationMs),
    })),
  }));

const findProjectBySessionId = (projects: Project[], sessionId: string | null) => {
  if (!sessionId) return { project: null, session: null };

  for (const project of projects) {
    const session = project.sessions.find((item) => item.id === sessionId);
    if (session) {
      return { project, session };
    }
  }

  return { project: null, session: null };
};

export type CliLaunchMode = 'new-tab' | 'replace-current';

interface AppState {
  projects: Project[];
  currentProjectId: string | null;
  activeSessionId: string | null;
  preferredCliId: string | null;
  preferredShell: string | null;
  cliLaunchMode: CliLaunchMode;
  cliTools: CliTool[];
  availableShells: string[];
  isLoading: boolean;
  
  loadProjects: () => Promise<void>;
  addProject: (name: string, path: string) => Promise<Project>;
  updateProject: (projectId: string, updates: Partial<Project>) => Promise<void>;
  deleteProject: (id: string) => Promise<void>;
  setCurrentProject: (id: string | null) => void;
  
  addSession: (projectId: string, shell: string) => Promise<Session>;
  launchCliSession: (projectId: string, cliId?: string | null) => Promise<Session>;
  renameSession: (projectId: string, sessionId: string, name: string) => Promise<void>;
  updateSession: (projectId: string, sessionId: string, updates: Partial<Session>) => Promise<void>;
  removeSession: (projectId: string, sessionId: string) => Promise<void>;
  setActiveSession: (sessionId: string | null) => void;
  recordSessionActivity: (projectId: string, sessionId: string, at?: number) => Promise<void>;
  recordSessionCommand: (projectId: string, sessionId: string, at?: number) => Promise<void>;
  recordSessionStartup: (projectId: string, sessionId: string, startupDurationMs: number, at?: number) => Promise<void>;
  
  loadCliTools: () => Promise<void>;
  loadAvailableShells: () => Promise<void>;
  getDefaultCliTool: () => CliTool | null;
  getCurrentCliTool: () => CliTool | null;
  setPreferredCliTool: (cliId: string | null) => void;
  setPreferredShell: (shell: string | null) => void;
  setCliLaunchMode: (mode: CliLaunchMode) => void;
  installCliTool: (cliId: string, installCommand?: string | null) => Promise<Session>;
}

export const useStore = create<AppState>((set, get) => ({
  projects: [],
  currentProjectId: getStoredValue(STORAGE_KEYS.currentProjectId),
  activeSessionId: getStoredValue(STORAGE_KEYS.activeSessionId),
  preferredCliId: getStoredValue(STORAGE_KEYS.preferredCliId),
  preferredShell: getStoredValue(STORAGE_KEYS.preferredShell),
  cliLaunchMode: getStoredValue(STORAGE_KEYS.cliLaunchMode) === 'replace-current' ? 'replace-current' : 'new-tab',
  cliTools: buildCatalogCliTools(),
  availableShells: [],
  isLoading: true,
  
  loadProjects: async () => {
    try {
      const normalizedProjects = normalizeProjects(await api.getProjects());
      const storedProjectId = getStoredValue(STORAGE_KEYS.currentProjectId);
      const storedSessionId = getStoredValue(STORAGE_KEYS.activeSessionId);
      const now = Date.now();
      const projects = normalizedProjects.map((project) => {
        if (project.sessions.length <= SESSION_CLEANUP_ALWAYS_KEEP) {
          return project;
        }

        const sortedSessions = [...project.sessions].sort((left, right) => {
          const leftActive = left.lastActiveAt ?? left.createdAt ?? 0;
          const rightActive = right.lastActiveAt ?? right.createdAt ?? 0;
          return rightActive - leftActive;
        });

        const sessionIdsToKeep = new Set<string>();
        for (const session of sortedSessions.slice(0, SESSION_CLEANUP_ALWAYS_KEEP)) {
          sessionIdsToKeep.add(session.id);
        }
        if (storedSessionId) {
          sessionIdsToKeep.add(storedSessionId);
        }

        const sessionIdsToRetain = new Set<string>(sessionIdsToKeep);
        sortedSessions.forEach((session, index) => {
          if (sessionIdsToKeep.has(session.id)) return;
          if (index >= SESSION_CLEANUP_MAX_PER_PROJECT) return;

          const activityAt = session.lastActiveAt ?? session.createdAt ?? 0;
          if (activityAt <= 0 || now - activityAt <= SESSION_CLEANUP_IDLE_MS) {
            sessionIdsToRetain.add(session.id);
          }
        });

        const cleanedSessions = project.sessions.filter((session) => sessionIdsToRetain.has(session.id));

        return cleanedSessions.length === project.sessions.length
          ? project
          : {
              ...project,
              sessions: cleanedSessions,
            };
      });

      const cleanupTargets = projects.filter((project, index) => {
        return project.sessions.length !== normalizedProjects[index]?.sessions.length;
      });
      if (cleanupTargets.length > 0) {
        await Promise.all(cleanupTargets.map((project) => api.saveProject(project)));
      }

      const resolvedProjectId =
        storedProjectId && projects.some((project) => project.id === storedProjectId)
          ? storedProjectId
          : projects[0]?.id ?? null;
      const resolvedProject = projects.find((project) => project.id === resolvedProjectId);
      const resolvedSessionId =
        storedSessionId && resolvedProject?.sessions.some((session) => session.id === storedSessionId)
          ? storedSessionId
          : resolvedProject?.sessions[0]?.id ?? null;

      setStoredValue(STORAGE_KEYS.currentProjectId, resolvedProjectId);
      setStoredValue(STORAGE_KEYS.activeSessionId, resolvedSessionId);
      set((state) => ({
        projects,
        currentProjectId:
          state.currentProjectId && projects.some((project) => project.id === state.currentProjectId)
            ? state.currentProjectId
            : resolvedProjectId,
        activeSessionId: resolvedSessionId,
      }));
    } catch (error) {
      console.error('Failed to load projects:', error);
      set({ projects: [], currentProjectId: null, activeSessionId: null });
    } finally {
      set({ isLoading: false });
    }
  },
  
  addProject: async (name: string, path: string) => {
    const project: Project = {
      id: crypto.randomUUID(),
      name,
      path,
      color: null,
      logoPath: null,
      sessions: [],
    };
    const saved = await api.saveProject(project);
    setStoredValue(STORAGE_KEYS.currentProjectId, saved.id);
    setStoredValue(STORAGE_KEYS.activeSessionId, null);
    set((state) => ({
      projects: [...state.projects, saved],
      currentProjectId: saved.id,
      activeSessionId: null,
    }));
    return saved;
  },
  
  deleteProject: async (id: string) => {
    await api.deleteProject(id);
    set((state) => {
      const projects = state.projects.filter((p) => p.id !== id);
      const currentProjectId = state.currentProjectId === id ? projects[0]?.id ?? null : state.currentProjectId;
      const currentProject = projects.find((project) => project.id === currentProjectId);
      const activeSessionId =
        state.currentProjectId === id
          ? currentProject?.sessions[0]?.id ?? null
          : state.activeSessionId;

      setStoredValue(STORAGE_KEYS.currentProjectId, currentProjectId);
      setStoredValue(STORAGE_KEYS.activeSessionId, activeSessionId);

      return {
        projects,
        currentProjectId,
        activeSessionId,
      };
    });
  },

  updateProject: async (projectId: string, updates: Partial<Project>) => {
    const state = get();
    const project = state.projects.find((item) => item.id === projectId);
    if (!project) return;

    const updatedProject = {
      ...project,
      ...updates,
      id: project.id,
      sessions: project.sessions,
    };

    await api.saveProject(updatedProject);

    set((currentState) => ({
      projects: currentState.projects.map((item) =>
        item.id === projectId ? updatedProject : item
      ),
    }));
  },
  
  setCurrentProject: (id) => {
    const project = get().projects.find((item) => item.id === id);
    const nextSessionId = project?.sessions[0]?.id ?? null;
    setStoredValue(STORAGE_KEYS.currentProjectId, id);
    setStoredValue(STORAGE_KEYS.activeSessionId, nextSessionId);
    set({ currentProjectId: id, activeSessionId: nextSessionId });
  },
  
  addSession: async (projectId: string, shell: string) => {
    const createdAt = Date.now();
    const session: Session = {
      id: crypto.randomUUID(),
      name: `Terminal ${createdAt}`,
      shell,
      cliTool: null,
      pendingLaunchCommand: null,
      createdAt,
      lastActiveAt: createdAt,
      commandCount: 0,
      startupDurationMs: null,
    };
    
    const state = get();
    const project = state.projects.find((p) => p.id === projectId);
    if (!project) throw new Error('Project not found');
    
    const updatedProject = {
      ...project,
      sessions: [...project.sessions, session],
    };
    
    await api.saveProject(updatedProject);
    setStoredValue(STORAGE_KEYS.currentProjectId, projectId);
    setStoredValue(STORAGE_KEYS.activeSessionId, session.id);
    
    set((state) => ({
      projects: state.projects.map((p) =>
        p.id === projectId ? updatedProject : p
      ),
      currentProjectId: projectId,
      activeSessionId: session.id,
    }));
    
    return session;
  },

  launchCliSession: async (projectId: string, cliId?: string | null) => {
    const state = get();
    const createdAt = Date.now();
    const shell =
      state.availableShells.find((item) => item === state.preferredShell) ??
      state.availableShells[0] ??
      state.preferredShell ??
      'powershell.exe';
    const installedCliTools = state.cliTools.filter((tool) => tool.installed);
    const selectedCli =
      installedCliTools.find((tool) => tool.id === cliId) ??
      installedCliTools.find((tool) => tool.id === state.preferredCliId) ??
      installedCliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      installedCliTools[0] ??
      null;

    const session: Session = {
      id: crypto.randomUUID(),
      name: `Terminal ${createdAt}`,
      shell,
      cliTool: selectedCli?.id ?? null,
      pendingLaunchCommand: selectedCli?.matchedCommand ?? null,
      createdAt,
      lastActiveAt: createdAt,
      commandCount: 0,
      startupDurationMs: null,
    };

    const project = state.projects.find((p) => p.id === projectId);
    if (!project) throw new Error('Project not found');

    const updatedProject = {
      ...project,
      sessions: [...project.sessions, session],
    };

    await api.saveProject(updatedProject);
    setStoredValue(STORAGE_KEYS.currentProjectId, projectId);
    setStoredValue(STORAGE_KEYS.activeSessionId, session.id);
    setStoredValue(STORAGE_KEYS.preferredCliId, selectedCli?.id ?? null);

    set((currentState) => ({
      projects: currentState.projects.map((p) => (p.id === projectId ? updatedProject : p)),
      currentProjectId: projectId,
      activeSessionId: session.id,
      preferredCliId: selectedCli?.id ?? currentState.preferredCliId,
    }));

    return session;
  },

  renameSession: async (projectId: string, sessionId: string, name: string) => {
    const trimmedName = sanitizeSessionName(name);
    if (!trimmedName) return;

    const state = get();
    const project = state.projects.find((p) => p.id === projectId);
    if (!project) return;
    const currentSession = project.sessions.find((session) => session.id === sessionId);
    if (!currentSession || currentSession.name === trimmedName) return;

    const updatedProject = {
      ...project,
      sessions: project.sessions.map((session) =>
        session.id === sessionId
          ? { ...session, name: trimmedName }
          : session
      ),
    };

    await api.saveProject(updatedProject);

    set((state) => ({
      projects: state.projects.map((project) =>
        project.id === projectId ? updatedProject : project
      ),
    }));
  },

  updateSession: async (projectId: string, sessionId: string, updates: Partial<Session>) => {
    const state = get();
    const project = state.projects.find((p) => p.id === projectId);
    if (!project) return;
    const currentSession = project.sessions.find((session) => session.id === sessionId);
    if (!currentSession) return;

    const sanitizedUpdates =
      typeof updates.name === 'string'
        ? { ...updates, name: sanitizeSessionName(updates.name) || updates.name }
        : updates;
    const updateKeys = Object.keys(sanitizedUpdates) as (keyof Session)[];
    const hasChanges = updateKeys.some((key) => currentSession[key] !== sanitizedUpdates[key]);
    if (!hasChanges) return;

    const updatedProject = {
      ...project,
      sessions: project.sessions.map((session) =>
        session.id === sessionId
          ? { ...session, ...sanitizedUpdates }
          : session
      ),
    };

    await api.saveProject(updatedProject);

    set((state) => ({
      projects: state.projects.map((project) =>
        project.id === projectId ? updatedProject : project
      ),
    }));
  },
  
  removeSession: async (projectId: string, sessionId: string) => {
    const state = get();
    const project = state.projects.find((p) => p.id === projectId);
    if (!project) return;
    if (!project.sessions.some((session) => session.id === sessionId)) return;
    
    const updatedProject = {
      ...project,
      sessions: project.sessions.filter((s) => s.id !== sessionId),
    };
    
    await api.saveProject(updatedProject);
    
    set((state) => {
      const activeSessionId =
        state.activeSessionId === sessionId
          ? updatedProject.sessions[0]?.id ?? null
          : state.activeSessionId;

      setStoredValue(STORAGE_KEYS.activeSessionId, activeSessionId);

      return {
        projects: state.projects.map((p) =>
          p.id === projectId ? updatedProject : p
        ),
        activeSessionId,
      };
    });
  },
  
  setActiveSession: (sessionId) => {
    const state = get();
    const { session: activeSession } = findProjectBySessionId(state.projects, sessionId);
    const resolvedSessionId = activeSession?.id ?? null;

    setStoredValue(STORAGE_KEYS.activeSessionId, resolvedSessionId);
    if (activeSession?.cliTool) {
      setStoredValue(STORAGE_KEYS.preferredCliId, activeSession.cliTool);
    }

    set({
      activeSessionId: resolvedSessionId,
      preferredCliId: activeSession?.cliTool ?? state.preferredCliId,
    });
  },

  recordSessionActivity: async (projectId: string, sessionId: string, at?: number) => {
    const state = get();
    const project = state.projects.find((item) => item.id === projectId);
    if (!project) return;
    const session = project.sessions.find((item) => item.id === sessionId);
    if (!session) return;

    const timestamp = typeof at === 'number' && Number.isFinite(at) ? Math.floor(at) : Date.now();
    const lastActiveAt = session.lastActiveAt ?? 0;
    if (lastActiveAt > 0 && timestamp - lastActiveAt < SESSION_ACTIVITY_PERSIST_THROTTLE_MS) {
      return;
    }

    const updatedProject: Project = {
      ...project,
      sessions: project.sessions.map((item) =>
        item.id === sessionId
          ? {
              ...item,
              lastActiveAt: timestamp,
            }
          : item,
      ),
    };

    await api.saveProject(updatedProject);
    set((currentState) => ({
      projects: currentState.projects.map((item) => (item.id === projectId ? updatedProject : item)),
    }));
  },

  recordSessionCommand: async (projectId: string, sessionId: string, at?: number) => {
    const state = get();
    const project = state.projects.find((item) => item.id === projectId);
    if (!project) return;
    const session = project.sessions.find((item) => item.id === sessionId);
    if (!session) return;

    const timestamp = typeof at === 'number' && Number.isFinite(at) ? Math.floor(at) : Date.now();
    const nextCount = normalizeSessionCounter(session.commandCount) + 1;

    const updatedProject: Project = {
      ...project,
      sessions: project.sessions.map((item) =>
        item.id === sessionId
          ? {
              ...item,
              commandCount: nextCount,
              lastActiveAt: timestamp,
            }
          : item,
      ),
    };

    await api.saveProject(updatedProject);
    set((currentState) => ({
      projects: currentState.projects.map((item) => (item.id === projectId ? updatedProject : item)),
    }));
  },

  recordSessionStartup: async (projectId: string, sessionId: string, startupDurationMs: number, at?: number) => {
    const state = get();
    const project = state.projects.find((item) => item.id === projectId);
    if (!project) return;
    const session = project.sessions.find((item) => item.id === sessionId);
    if (!session) return;
    const normalizedStartup = normalizeOptionalDuration(startupDurationMs);
    if (normalizedStartup === null) return;
    if (typeof session.startupDurationMs === 'number' && session.startupDurationMs >= 0) return;

    const timestamp = typeof at === 'number' && Number.isFinite(at) ? Math.floor(at) : Date.now();
    const updatedProject: Project = {
      ...project,
      sessions: project.sessions.map((item) =>
        item.id === sessionId
          ? {
              ...item,
              startupDurationMs: normalizedStartup,
              lastActiveAt: timestamp,
            }
          : item,
      ),
    };

    await api.saveProject(updatedProject);
    set((currentState) => ({
      projects: currentState.projects.map((item) => (item.id === projectId ? updatedProject : item)),
    }));
  },
  
  loadCliTools: async () => {
    try {
      const probeResults = await api.probeCliTools(
        CLI_CATALOG.map((item) => ({
          id: item.id,
          commands: item.commands,
        })),
      );

      set({ cliTools: buildCatalogCliTools(probeResults) });
    } catch (error) {
      console.error('Failed to probe CLI tools:', error);
      set({ cliTools: buildCatalogCliTools() });
    }
  },
  
  loadAvailableShells: async () => {
    try {
      const availableShells = await api.getAvailableShells();
      const storedPreferredShell = getStoredValue(STORAGE_KEYS.preferredShell);
      const preferredShell = availableShells.find((shell) => shell === storedPreferredShell) ?? availableShells[0] ?? null;

      setStoredValue(STORAGE_KEYS.preferredShell, preferredShell);
      set({ availableShells, preferredShell });
    } catch (error) {
      console.error('Failed to load available shells:', error);
      set({ availableShells: [] });
    }
  },

  getDefaultCliTool: () => {
    const state = get();
    const installedCliTools = state.cliTools.filter((tool) => tool.installed);
    return (
      installedCliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      state.cliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      installedCliTools[0] ??
      state.cliTools[0] ??
      null
    );
  },

  getCurrentCliTool: () => {
    const state = get();
    const installedCliTools = state.cliTools.filter((tool) => tool.installed);
    return (
      installedCliTools.find((tool) => tool.id === state.preferredCliId) ??
      state.cliTools.find((tool) => tool.id === state.preferredCliId) ??
      installedCliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      state.cliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      installedCliTools[0] ??
      state.cliTools[0] ??
      null
    );
  },

  setPreferredCliTool: (cliId) => {
    setStoredValue(STORAGE_KEYS.preferredCliId, cliId);
    set({ preferredCliId: cliId });
  },

  setPreferredShell: (shell) => {
    setStoredValue(STORAGE_KEYS.preferredShell, shell);
    set({ preferredShell: shell });
  },

  setCliLaunchMode: (mode) => {
    setStoredValue(STORAGE_KEYS.cliLaunchMode, mode);
    set({ cliLaunchMode: mode });
  },

  installCliTool: async (cliId: string, installCommand?: string | null) => {
    const state = get();
    const catalogItem = CLI_CATALOG.find((item) => item.id === cliId);
    if (!catalogItem) throw new Error(`CLI tool not found: ${cliId}`);

    let detectedPlatform: string | null = null;
    try {
      detectedPlatform = await Promise.resolve(platform());
    } catch {
      detectedPlatform = null;
    }

    const detectedOs: 'windows' | 'macos' | 'linux' | null =
      detectedPlatform === 'windows' || detectedPlatform === 'macos' || detectedPlatform === 'linux'
        ? detectedPlatform
        : null;

    const installCommandForOs = detectedOs ? catalogItem.installCommandByOs?.[detectedOs] : null;
    const resolvedInstallCommand =
      installCommandForOs?.trim() || installCommand?.trim() || catalogItem.installCommand;

    const preferredInstallShellKeyword = detectedOs === 'windows' ? 'powershell' : 'bash';
    const preferredInstallShell =
      state.availableShells.find((item) => item.toLowerCase().includes(preferredInstallShellKeyword)) ?? null;

    const shell =
      preferredInstallShell ??
      state.availableShells.find((item) => item === state.preferredShell) ??
      state.availableShells[0] ??
      state.preferredShell ??
      (detectedOs === 'windows' ? 'powershell.exe' : 'bash');

    const projectId =
      state.currentProjectId ?? state.projects[0]?.id;

    if (!projectId) {
      throw new Error('No project available. Add a project first.');
    }

    const project = state.projects.find((p) => p.id === projectId);
    if (!project) throw new Error('Project not found');

    const createdAt = Date.now();
    const session: Session = {
      id: crypto.randomUUID(),
      name: `Install ${catalogItem.label}`,
      shell,
      cliTool: null,
      pendingLaunchCommand: resolvedInstallCommand,
      createdAt,
      lastActiveAt: createdAt,
      commandCount: 0,
      startupDurationMs: null,
    };

    const updatedProject = {
      ...project,
      sessions: [...project.sessions, session],
    };

    await api.saveProject(updatedProject);
    setStoredValue(STORAGE_KEYS.currentProjectId, projectId);
    setStoredValue(STORAGE_KEYS.activeSessionId, session.id);

    set((currentState) => ({
      projects: currentState.projects.map((p) => (p.id === projectId ? updatedProject : p)),
      currentProjectId: projectId,
      activeSessionId: session.id,
    }));

    return session;
  },
}));
