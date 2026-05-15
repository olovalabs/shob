# React to SolidJS Full Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the entire Shob Electron desktop app from React 19 to SolidJS, replacing all React dependencies with SolidJS equivalents (Kobalte for UI primitives, lucide-solid for icons, solid-js/store for state).

**Architecture:** The app is an Electron desktop application with a Vite-built renderer. The renderer uses SolidJS signals/store for reactivity, Kobalte for accessible UI primitives, and native xterm.js for terminal rendering. The store pattern shifts from Zustand's create() to SolidJS's createStore() with exported action objects. Components shift from React hooks (useState, useEffect, useMemo, useCallback) to SolidJS primitives (createSignal, createEffect, createMemo).

**Tech Stack:** SolidJS 1.9+, @kobalte/core, lucide-solid, vite-plugin-solid, xterm.js, Electron, TypeScript, TailwindCSS v4

---

### Task 1: Update package.json dependencies

**Files:**
- Modify: `package.json`

- [ ] **Step 1: Replace React dependencies with SolidJS equivalents**

Run these commands to remove React packages and install SolidJS packages:

```powershell
pnpm remove react react-dom @vitejs/plugin-react @types/react @types/react-dom radix-ui @base-ui/react lucide-react react-router-dom zustand recharts cmdk embla-carousel-react react-day-picker react-resizable-panels input-otp next-themes sonner vaul
```

```powershell
pnpm add solid-js @kobalte/core lucide-solid @solidjs/router solid-sonner
```

```powershell
pnpm add -D vite-plugin-solid
```

- [ ] **Step 2: Verify package.json has the correct dependencies**

The `dependencies` section should contain:
```json
"dependencies": {
  "@lydell/node-pty": "^1.2.0-beta.12",
  "@xterm/addon-clipboard": "0.3.0-beta.213",
  "@xterm/addon-fit": "^0.11.0",
  "@xterm/addon-image": "0.10.0-beta.213",
  "@xterm/addon-ligatures": "0.11.0-beta.213",
  "@xterm/addon-progress": "0.3.0-beta.213",
  "@xterm/addon-search": "0.17.0-beta.213",
  "@xterm/addon-serialize": "0.15.0-beta.213",
  "@xterm/addon-unicode11": "0.10.0-beta.213",
  "@xterm/addon-web-links": "^0.12.0",
  "@xterm/addon-webgl": "0.20.0-beta.212",
  "@xterm/headless": "6.1.0-beta.213",
  "@xterm/xterm": "6.1.0-beta.213",
  "@kobalte/core": "latest",
  "@solidjs/router": "latest",
  "chokidar": "^5.0.0",
  "class-variance-authority": "^0.7.1",
  "clsx": "^2.1.1",
  "date-fns": "^4.1.0",
  "lucide-solid": "latest",
  "solid-js": "^1.9.5",
  "solid-sonner": "latest",
  "tailwind-merge": "^3.5.0"
}
```

The `devDependencies` section should contain:
```json
"devDependencies": {
  "@commitlint/cli": "^20.5.0",
  "@commitlint/config-conventional": "^20.5.0",
  "@eslint/js": "^9.39.4",
  "@tailwindcss/vite": "^4.2.2",
  "@types/node": "^24.12.0",
  "commitizen": "^4.3.1",
  "concurrently": "^9.2.1",
  "cross-env": "^10.1.0",
  "cz-git": "^1.12.0",
  "electron": "^41.5.0",
  "electron-builder": "^26.8.1",
  "eslint": "^9.39.4",
  "git-cz": "^4.9.0",
  "globals": "^17.4.0",
  "husky": "^9.1.7",
  "png-to-ico": "^3.0.1",
  "tailwindcss": "^4.2.2",
  "typescript": "~5.9.3",
  "typescript-eslint": "^8.57.0",
  "vite": "^8.0.1",
  "vite-plugin-solid": "latest",
  "wait-on": "^9.0.5"
}
```

- [ ] **Step 3: Commit**

```bash
git add package.json pnpm-lock.yaml
git commit -m "chore: replace React dependencies with SolidJS equivalents"
```

---

### Task 2: Update Vite config and TypeScript settings

**Files:**
- Modify: `vite.config.ts`
- Modify: `tsconfig.app.json`
- Modify: `tsconfig.json`

- [ ] **Step 1: Update vite.config.ts**

Replace the entire file content:

```typescript
import { defineConfig } from 'vite'
import solid from 'vite-plugin-solid'
import tailwindcss from '@tailwindcss/vite'
import { fileURLToPath, URL } from 'node:url'

export default defineConfig({
  plugins: [solid(), tailwindcss()],
  build: {
    outDir: 'dist-renderer',
  },
  server: {
    port: 5180,
    strictPort: true,
  },
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
    dedupe: ['solid-js'],
  },
})
```

- [ ] **Step 2: Update tsconfig.app.json**

Read the current file, then replace any React JSX settings with SolidJS settings. The key change is ensuring JSX is configured for SolidJS:

```json
{
  "compilerOptions": {
    "target": "ES2020",
    "useDefineForClassFields": true,
    "module": "ESNext",
    "lib": ["ES2020", "DOM", "DOM.Iterable"],
    "skipLibCheck": true,
    "moduleResolution": "bundler",
    "allowImportingTsExtensions": true,
    "verbatimModuleSyntax": true,
    "moduleDetection": "force",
    "noEmit": true,
    "jsx": "preserve",
    "jsxImportSource": "solid-js",
    "strict": true,
    "noUnusedLocals": true,
    "noUnusedParameters": true,
    "noFallthroughCasesInSwitch": true,
    "noUncheckedSideEffectImports": true,
    "paths": {
      "@/*": ["./src/*"]
    }
  },
  "include": ["src/**/*.ts", "src/**/*.tsx"]
}
```

- [ ] **Step 3: Update tsconfig.json**

Ensure the root tsconfig references the correct sub-configs. Read current file and verify it includes `tsconfig.app.json` and `tsconfig.node.json` in references.

- [ ] **Step 4: Commit**

```bash
git add vite.config.ts tsconfig.app.json tsconfig.json
git commit -m "chore: configure Vite and TypeScript for SolidJS"
```

---

### Task 3: Convert store from Zustand to SolidJS Store

**Files:**
- Modify: `src/store/index.ts`

- [ ] **Step 1: Rewrite store/index.ts with SolidJS createStore**

Replace the entire file. The Zustand `create()` pattern converts to SolidJS `createStore()` with the state as a store object and actions as a separate exported object. Key conversions:
- `set((state) => ({ ... }))` → `setStore({ ... })`
- `get()` → direct `store` reference
- State properties accessed via `store.property` instead of `state.property`

```typescript
import { createStore } from 'solid-js/store';
import { nativeApi } from '../services/native';
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
}

const [store, setStore] = createStore<AppState>({
  projects: [],
  currentProjectId: getStoredValue(STORAGE_KEYS.currentProjectId),
  activeSessionId: getStoredValue(STORAGE_KEYS.activeSessionId),
  preferredCliId: getStoredValue(STORAGE_KEYS.preferredCliId),
  preferredShell: getStoredValue(STORAGE_KEYS.preferredShell),
  cliLaunchMode: getStoredValue(STORAGE_KEYS.cliLaunchMode) === 'replace-current' ? 'replace-current' : 'new-tab',
  cliTools: buildCatalogCliTools(),
  availableShells: [],
  isLoading: true,
});

export const actions = {
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
      setStore({
        projects,
        currentProjectId:
          store.currentProjectId && projects.some((project) => project.id === store.currentProjectId)
            ? store.currentProjectId
            : resolvedProjectId,
        activeSessionId: resolvedSessionId,
      });
    } catch (error) {
      console.error('Failed to load projects:', error);
      setStore({ projects: [], currentProjectId: null, activeSessionId: null });
    } finally {
      setStore({ isLoading: false });
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
    setStore({
      projects: [...store.projects, saved],
      currentProjectId: saved.id,
      activeSessionId: null,
    });
    return saved;
  },

  deleteProject: async (id: string) => {
    await api.deleteProject(id);
    const projects = store.projects.filter((p) => p.id !== id);
    const currentProjectId = store.currentProjectId === id ? projects[0]?.id ?? null : store.currentProjectId;
    const currentProject = projects.find((project) => project.id === currentProjectId);
    const activeSessionId =
      store.currentProjectId === id
        ? currentProject?.sessions[0]?.id ?? null
        : store.activeSessionId;

    setStoredValue(STORAGE_KEYS.currentProjectId, currentProjectId);
    setStoredValue(STORAGE_KEYS.activeSessionId, activeSessionId);

    setStore({
      projects,
      currentProjectId,
      activeSessionId,
    });
  },

  updateProject: async (projectId: string, updates: Partial<Project>) => {
    const project = store.projects.find((item) => item.id === projectId);
    if (!project) return;

    const updatedProject = {
      ...project,
      ...updates,
      id: project.id,
      sessions: project.sessions,
    };

    await api.saveProject(updatedProject);

    setStore('projects', (prev) =>
      prev.map((item) =>
        item.id === projectId ? updatedProject : item
      ),
    );
  },

  setCurrentProject: (id: string | null) => {
    const project = store.projects.find((item) => item.id === id);
    const nextSessionId = project?.sessions[0]?.id ?? null;
    setStoredValue(STORAGE_KEYS.currentProjectId, id);
    setStoredValue(STORAGE_KEYS.activeSessionId, nextSessionId);
    setStore({ currentProjectId: id, activeSessionId: nextSessionId });
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

    const project = store.projects.find((p) => p.id === projectId);
    if (!project) throw new Error('Project not found');

    const updatedProject = {
      ...project,
      sessions: [...project.sessions, session],
    };

    await api.saveProject(updatedProject);
    setStoredValue(STORAGE_KEYS.currentProjectId, projectId);
    setStoredValue(STORAGE_KEYS.activeSessionId, session.id);

    setStore({
      projects: store.projects.map((p) =>
        p.id === projectId ? updatedProject : p
      ),
      currentProjectId: projectId,
      activeSessionId: session.id,
    });

    return session;
  },

  launchCliSession: async (projectId: string, cliId?: string | null) => {
    const createdAt = Date.now();
    const shell =
      store.availableShells.find((item) => item === store.preferredShell) ??
      store.availableShells[0] ??
      store.preferredShell ??
      'powershell.exe';
    const installedCliTools = store.cliTools.filter((tool) => tool.installed);
    const selectedCli =
      installedCliTools.find((tool) => tool.id === cliId) ??
      installedCliTools.find((tool) => tool.id === store.preferredCliId) ??
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

    const project = store.projects.find((p) => p.id === projectId);
    if (!project) throw new Error('Project not found');

    const updatedProject = {
      ...project,
      sessions: [...project.sessions, session],
    };

    await api.saveProject(updatedProject);
    setStoredValue(STORAGE_KEYS.currentProjectId, projectId);
    setStoredValue(STORAGE_KEYS.activeSessionId, session.id);
    setStoredValue(STORAGE_KEYS.preferredCliId, selectedCli?.id ?? null);

    setStore({
      projects: store.projects.map((p) => (p.id === projectId ? updatedProject : p)),
      currentProjectId: projectId,
      activeSessionId: session.id,
      preferredCliId: selectedCli?.id ?? store.preferredCliId,
    });

    return session;
  },

  renameSession: async (projectId: string, sessionId: string, name: string) => {
    const trimmedName = sanitizeSessionName(name);
    if (!trimmedName) return;

    const project = store.projects.find((p) => p.id === projectId);
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

    setStore('projects', (prev) =>
      prev.map((project) =>
        project.id === projectId ? updatedProject : project
      ),
    );
  },

  updateSession: async (projectId: string, sessionId: string, updates: Partial<Session>) => {
    const project = store.projects.find((p) => p.id === projectId);
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

    setStore('projects', (prev) =>
      prev.map((project) =>
        project.id === projectId ? updatedProject : project
      ),
    );
  },

  removeSession: async (projectId: string, sessionId: string) => {
    const project = store.projects.find((p) => p.id === projectId);
    if (!project) return;
    if (!project.sessions.some((session) => session.id === sessionId)) return;

    const updatedProject = {
      ...project,
      sessions: project.sessions.filter((s) => s.id !== sessionId),
    };

    await api.saveProject(updatedProject);

    const activeSessionId =
      store.activeSessionId === sessionId
        ? updatedProject.sessions[0]?.id ?? null
        : store.activeSessionId;

    setStoredValue(STORAGE_KEYS.activeSessionId, activeSessionId);

    setStore({
      projects: store.projects.map((p) =>
        p.id === projectId ? updatedProject : p
      ),
      activeSessionId,
    });
  },

  setActiveSession: (sessionId: string | null) => {
    const { session: activeSession } = findProjectBySessionId(store.projects, sessionId);
    const resolvedSessionId = activeSession?.id ?? null;

    setStoredValue(STORAGE_KEYS.activeSessionId, resolvedSessionId);
    if (activeSession?.cliTool) {
      setStoredValue(STORAGE_KEYS.preferredCliId, activeSession.cliTool);
    }

    setStore({
      activeSessionId: resolvedSessionId,
      preferredCliId: activeSession?.cliTool ?? store.preferredCliId,
    });
  },

  recordSessionActivity: async (projectId: string, sessionId: string, at?: number) => {
    const project = store.projects.find((item) => item.id === projectId);
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
          ? { ...item, lastActiveAt: timestamp }
          : item,
      ),
    };

    await api.saveProject(updatedProject);
    setStore('projects', (prev) =>
      prev.map((item) => (item.id === projectId ? updatedProject : item)),
    );
  },

  recordSessionCommand: async (projectId: string, sessionId: string, at?: number) => {
    const project = store.projects.find((item) => item.id === projectId);
    if (!project) return;
    const session = project.sessions.find((item) => item.id === sessionId);
    if (!session) return;

    const timestamp = typeof at === 'number' && Number.isFinite(at) ? Math.floor(at) : Date.now();
    const nextCount = normalizeSessionCounter(session.commandCount) + 1;

    const updatedProject: Project = {
      ...project,
      sessions: project.sessions.map((item) =>
        item.id === sessionId
          ? { ...item, commandCount: nextCount, lastActiveAt: timestamp }
          : item,
      ),
    };

    await api.saveProject(updatedProject);
    setStore('projects', (prev) =>
      prev.map((item) => (item.id === projectId ? updatedProject : item)),
    );
  },

  recordSessionStartup: async (projectId: string, sessionId: string, startupDurationMs: number, at?: number) => {
    const project = store.projects.find((item) => item.id === projectId);
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
          ? { ...item, startupDurationMs: normalizedStartup, lastActiveAt: timestamp }
          : item,
      ),
    };

    await api.saveProject(updatedProject);
    setStore('projects', (prev) =>
      prev.map((item) => (item.id === projectId ? updatedProject : item)),
    );
  },

  loadCliTools: async () => {
    try {
      const probeResults = await api.probeCliTools(
        CLI_CATALOG.map((item) => ({
          id: item.id,
          commands: item.commands,
        })),
      );

      setStore({ cliTools: buildCatalogCliTools(probeResults) });
    } catch (error) {
      console.error('Failed to probe CLI tools:', error);
      setStore({ cliTools: buildCatalogCliTools() });
    }
  },

  loadAvailableShells: async () => {
    try {
      const availableShells = await api.getAvailableShells();
      const storedPreferredShell = getStoredValue(STORAGE_KEYS.preferredShell);
      const preferredShell = availableShells.find((shell) => shell === storedPreferredShell) ?? availableShells[0] ?? null;

      setStoredValue(STORAGE_KEYS.preferredShell, preferredShell);
      setStore({ availableShells, preferredShell });
    } catch (error) {
      console.error('Failed to load available shells:', error);
      setStore({ availableShells: [] });
    }
  },

  getDefaultCliTool: () => {
    const installedCliTools = store.cliTools.filter((tool) => tool.installed);
    return (
      installedCliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      store.cliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      installedCliTools[0] ??
      store.cliTools[0] ??
      null
    );
  },

  getCurrentCliTool: () => {
    const installedCliTools = store.cliTools.filter((tool) => tool.installed);
    return (
      installedCliTools.find((tool) => tool.id === store.preferredCliId) ??
      store.cliTools.find((tool) => tool.id === store.preferredCliId) ??
      installedCliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      store.cliTools.find((tool) => tool.id === DEFAULT_CLI_ID) ??
      installedCliTools[0] ??
      store.cliTools[0] ??
      null
    );
  },

  setPreferredCliTool: (cliId: string | null) => {
    setStoredValue(STORAGE_KEYS.preferredCliId, cliId);
    setStore({ preferredCliId: cliId });
  },

  setPreferredShell: (shell: string | null) => {
    setStoredValue(STORAGE_KEYS.preferredShell, shell);
    setStore({ preferredShell: shell });
  },

  setCliLaunchMode: (mode: CliLaunchMode) => {
    setStoredValue(STORAGE_KEYS.cliLaunchMode, mode);
    setStore({ cliLaunchMode: mode });
  },

  installCliTool: async (cliId: string, installCommand?: string | null) => {
    const catalogItem = CLI_CATALOG.find((item) => item.id === cliId);
    if (!catalogItem) throw new Error(`CLI tool not found: ${cliId}`);

    let detectedPlatform: string | null = null;
    try {
      detectedPlatform = nativeApi.platform();
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
      store.availableShells.find((item) => item.toLowerCase().includes(preferredInstallShellKeyword)) ?? null;

    const shell =
      preferredInstallShell ??
      store.availableShells.find((item) => item === store.preferredShell) ??
      store.availableShells[0] ??
      store.preferredShell ??
      (detectedOs === 'windows' ? 'powershell.exe' : 'bash');

    const projectId =
      store.currentProjectId ?? store.projects[0]?.id;

    if (!projectId) {
      throw new Error('No project available. Add a project first.');
    }

    const project = store.projects.find((p) => p.id === projectId);
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

    setStore({
      projects: store.projects.map((p) => (p.id === projectId ? updatedProject : p)),
      currentProjectId: projectId,
      activeSessionId: session.id,
    });

    return session;
  },
};

export { store, setStore };
```

- [ ] **Step 2: Create a useStore helper for component compatibility**

Add this at the bottom of `src/store/index.ts` before the final exports. This provides a Zustand-like selector API that components currently use:

```typescript
/**
 * useStore hook - provides Zustand-compatible API for SolidJS store.
 * Usage: useStore() returns the full store object (for destructuring).
 * Usage: useStore((state) => state.projects) returns a reactive getter.
 */
export function useStore(): AppState & typeof actions;
export function useStore<T>(selector: (state: AppState) => T): () => T;
export function useStore<T>(selector?: (state: AppState) => T): (() => T) | (AppState & typeof actions) {
  if (!selector) {
    return { ...store, ...actions };
  }

  return () => selector(store);
}
```

- [ ] **Step 3: Commit**

```bash
git add src/store/index.ts
git commit -m "refactor: convert Zustand store to SolidJS createStore"
```

---

### Task 4: Convert hooks and utility files

**Files:**
- Modify: `src/hooks/use-mobile.ts`

- [ ] **Step 1: Convert use-mobile.ts to SolidJS**

Replace the entire file:

```typescript
import { createSignal, onCleanup, onMount } from 'solid-js'

const MOBILE_BREAKPOINT = 768

export function useIsMobile() {
  const [isMobile, setIsMobile] = createSignal<boolean | undefined>(undefined)

  onMount(() => {
    const mql = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT - 1}px)`)
    const onChange = () => {
      setIsMobile(window.innerWidth < MOBILE_BREAKPOINT)
    }
    mql.addEventListener('change', onChange)
    setIsMobile(window.innerWidth < MOBILE_BREAKPOINT)
    onCleanup(() => mql.removeEventListener('change', onChange))
  })

  return () => !!isMobile()
}
```

- [ ] **Step 2: Commit**

```bash
git add src/hooks/use-mobile.ts
git commit -m "refactor: convert useIsMobile hook to SolidJS"
```

---

### Task 5: Convert App.tsx (already SolidJS, verify and fix)

**Files:**
- Read: `src/App.tsx`

- [ ] **Step 1: Verify App.tsx is correct SolidJS**

The current App.tsx already uses SolidJS primitives. Verify it imports from `solid-js` not `react`. The file should already be correct based on our earlier read. No changes needed if it already has:
- `import { createEffect, createSignal, onCleanup, onMount, Show } from 'solid-js'`
- Uses `createSignal`, `createEffect`, `onMount`, `onCleanup`, `Show`

- [ ] **Step 2: Commit if no changes needed**

```bash
git add src/App.tsx
git commit -m "chore: verify App.tsx SolidJS compatibility"
```

---

### Task 6: Convert MainView.tsx

**Files:**
- Modify: `src/components/MainView.tsx`

- [ ] **Step 1: Convert MainView.tsx to SolidJS**

Replace the entire file. Key changes:
- `useState` → `createSignal`
- `useEffect` → `createEffect`
- `useMemo` → `createMemo`
- `lazy` from react → `lazy` from solid-js
- `Suspense` from react → `Suspense` from solid-js
- Store selectors use the new `useStore` pattern

```typescript
import { createEffect, createMemo, createSignal, lazy, Suspense } from 'solid-js'
import { nativeApi } from '../services/native'
import { Sidebar } from './Sidebar'
import { TabBar } from './TabBar'
import { Terminal } from './Terminal'
import { WelcomeScreen } from './WelcomeScreen'
import { useStore } from '../store'

const FileTree = lazy(async () => import('./FileTree'))

const folderNameFromPath = (path: string) => {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || path
}

export function MainView() {
  const projects = useStore((s) => s.projects)
  const currentProject = useStore((s) =>
    s.projects.find((project) => project.id === s.currentProjectId) ?? null,
  )
  const activeSessionId = useStore((s) => s.activeSessionId)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const addProject = useStore((s) => s.addProject)
  const setCurrentProject = useStore((s) => s.setCurrentProject)
  const launchCliSession = useStore((s) => s.launchCliSession)
  const [activeFilePath, setActiveFilePath] = createSignal<string | null>(null)
  const [isFileTreeVisible, setIsFileTreeVisible] = createSignal(false)
  const [bootedSessionIds, setBootedSessionIds] = createSignal<Set<string>>(new Set())
  const projectSessions = createMemo(() => currentProject()?.sessions ?? [])
  const allSessions = createMemo(() => projects().flatMap((p) => p.sessions))

  createEffect(() => {
    const _ = currentProjectId()
    setActiveFilePath(null)
  })

  createEffect(() => {
    const sid = activeSessionId()
    if (!sid) return
    setBootedSessionIds((current) => {
      if (current.has(sid)) return current
      const next = new Set(current)
      next.add(sid)
      return next
    })
  })

  createEffect(() => {
    const visible = isFileTreeVisible()
    window.dispatchEvent(
      new CustomEvent('gg-file-tree-state', {
        detail: { isFileTreeVisible: visible },
      }),
    )
  })

  createEffect(() => {
    const handleFileTreeToggleRequest = () => {
      setIsFileTreeVisible((current) => !current)
    }

    window.addEventListener('gg-toggle-file-tree', handleFileTreeToggleRequest)
    return () => window.removeEventListener('gg-toggle-file-tree', handleFileTreeToggleRequest)
  })

  const handleFileSelect = (filePath: string | null) => {
    setActiveFilePath(filePath)
  }

  const handleOpenFolder = async () => {
    const selected = await nativeApi.open({
      directory: true,
      multiple: false,
      title: 'Select Project Folder',
    })

    if (typeof selected !== 'string' || !selected) return

    const existing = projects().find((project) => project.path === selected)
    if (existing) {
      setCurrentProject(existing.id)
      return
    }

    const created = await addProject(folderNameFromPath(selected), selected)
    setCurrentProject(created.id)
  }

  const handleCreateSession = async () => {
    const cpid = currentProjectId()
    if (!cpid) return
    await launchCliSession(cpid)
  }

  const handleToggleFileTree = () => {
    if (!currentProject()) return
    setIsFileTreeVisible((current) => !current)
  }

  return (
    <div class="flex min-h-0 flex-1 bg-background text-foreground">
      <Sidebar />
      <div class="min-w-0 flex-1 flex flex-col bg-background">
        <TabBar />
        <div class="min-h-0 min-w-0 flex-1 overflow-hidden bg-background">
          <div class="relative h-full w-full min-h-0 min-w-0 overflow-hidden" style={{ display: projectSessions().length > 0 ? 'block' : 'none' }}>
            {(() => {
              const sessions = allSessions()
              const booted = bootedSessionIds()
              return sessions.map((session) => {
                const shouldBoot = booted.has(session.id)
                if (!shouldBoot) return null

                return (
                  <Terminal
                    sessionId={session.id}
                    isActive={session.id === activeSessionId()}
                    shouldBoot={shouldBoot}
                  />
                )
              })
            })()}
          </div>

          {projectSessions().length === 0 && (
            <WelcomeScreen
              projects={projects()}
              currentProject={currentProject()}
              onOpenFolder={handleOpenFolder}
              onCreateSession={handleCreateSession}
              onSelectProject={setCurrentProject}
              onToggleFileTree={handleToggleFileTree}
            />
          )}
        </div>
      </div>
      {(() => {
        if (!isFileTreeVisible()) return null
        return (
          <Suspense fallback={null}>
            <FileTree selectedFilePath={activeFilePath()} onFileSelect={handleFileSelect} />
          </Suspense>
        )
      })()}
    </div>
  )
}
```

- [ ] **Step 2: Commit**

```bash
git add src/components/MainView.tsx
git commit -m "refactor: convert MainView to SolidJS"
```

---

### Task 7: Convert TabBar.tsx

**Files:**
- Modify: `src/components/TabBar.tsx`

- [ ] **Step 1: Convert TabBar.tsx to SolidJS**

Replace the entire file. Key changes:
- `className` → `class`
- Store destructuring uses new pattern
- No React imports

```typescript
import { Plus, SquareTerminal, X } from 'lucide-solid'
import { CliAvatar } from './CliAvatar'
import { getCliDisplayLabel } from '../config/cli-ui'
import { useStore } from '../store'
import { Button } from '@/components/ui/button'
import {
  Tabs,
  TabsList,
  TabsTrigger,
} from '@/components/ui/tabs'

const compactTitle = (title: string, maxChars: number) =>
  title.length > maxChars ? `${title.slice(0, Math.max(1, maxChars - 1))}\u2026` : title

export function TabBar() {
  const projects = useStore((s) => s.projects)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const activeSessionId = useStore((s) => s.activeSessionId)
  const setActiveSession = useStore((s) => s.setActiveSession)
  const launchCliSession = useStore((s) => s.launchCliSession)
  const removeSession = useStore((s) => s.removeSession)

  const currentProject = () => projects().find((project) => project.id === currentProjectId()) ?? null
  const totalTabs = () => currentProject() ? currentProject()!.sessions.length : 0
  const isCompactTabs = () => totalTabs() >= 5
  const sessionTitleLimit = () => isCompactTabs() ? 12 : 24

  return (
    <div class="chrome-tabbar">
      <div class="chrome-tabbar-inner">
        <div class="hide-scrollbar flex min-w-0 flex-1 items-end overflow-x-auto">
          <Tabs
            value={activeSessionId() ?? ''}
            onValueChange={setActiveSession}
            class="chrome-tabs flex items-end"
          >
            <TabsList variant="line" class="chrome-tabs-list bg-transparent p-0">
              {(() => {
                const cp = currentProject()
                if (!cp) return null
                return cp.sessions.map((session) => {
                  const isActive = session.id === activeSessionId()
                  const displayName = compactTitle(session.name, sessionTitleLimit())

                  return (
                    <TabsTrigger
                      value={session.id}
                      class={`chrome-tab-trigger relative group inline-flex items-center ${
                        isCompactTabs()
                          ? 'max-w-[156px] min-w-[92px] gap-1.5 px-2 text-[12px]'
                          : 'max-w-[240px] min-w-[116px] gap-2 px-3 text-sm'
                      }`}
                      title={session.name}
                    >
                      {!isActive && <span class="chrome-tab-divider" aria-hidden="true" />}
                      {session.cliTool ? (
                        <CliAvatar
                          cliId={session.cliTool}
                          label={getCliDisplayLabel(session.cliTool) ?? session.name}
                          size="sm"
                          class={isCompactTabs() ? 'scale-90' : ''}
                        />
                      ) : (
                        <SquareTerminal class={`${isCompactTabs() ? 'h-3 w-3' : 'h-3.5 w-3.5'} shrink-0 text-muted-foreground`} stroke-width={1.9} />
                      )}
                      <span class="truncate">{displayName}</span>
                      <span
                        onPointerDown={(event) => {
                          event.preventDefault()
                          event.stopPropagation()
                        }}
                        onClick={(event) => {
                          event.stopPropagation()
                          const cpid = currentProjectId()
                          if (cpid) removeSession(cpid, session.id)
                        }}
                        class={`chrome-tab-close inline-flex h-4 w-4 items-center justify-center rounded-full transition ${
                          isActive ? 'opacity-100' : 'opacity-0 group-hover:opacity-100'
                        }`}
                        aria-label={`Close ${session.name}`}
                      >
                        <X class="h-3 w-3" stroke-width={2} />
                      </span>
                    </TabsTrigger>
                  )
                })
              })()}
            </TabsList>
          </Tabs>

          <Button
            type="button"
            onClick={() => {
              const cpid = currentProjectId()
              if (cpid) launchCliSession(cpid)
            }}
            variant="ghost"
            size="icon-sm"
            class="chrome-tab-new-button mb-[4px] ml-1 h-7 w-7 shrink-0 rounded-full"
            title="New session"
          >
            <Plus class="h-4 w-4" stroke-width={2} />
          </Button>
        </div>
      </div>
    </div>
  )
}
```

- [ ] **Step 2: Commit**

```bash
git add src/components/TabBar.tsx
git commit -m "refactor: convert TabBar to SolidJS"
```

---

### Task 8: Convert TitleBar.tsx

**Files:**
- Modify: `src/components/TitleBar.tsx`

- [ ] **Step 1: Convert TitleBar.tsx to SolidJS**

Replace the entire file. Key changes:
- `useState` → `createSignal`
- `useEffect` → `createEffect`
- `useMemo` → `createMemo`
- `useCallback` → plain functions (stable in SolidJS)
- `useRef` → direct variable or `createSignal`
- `React.ReactNode` → `JSX.Element`
- `React.Dispatch<React.SetStateAction<boolean>>` → `Setter<boolean>`
- `React.CSSProperties` → `CSSProperties` from solid-js
- `className` → `class`
- `lucide-react` → `lucide-solid`

```typescript
import { createEffect, createMemo, createSignal, onCleanup, onMount, type JSX, type Setter } from 'solid-js'
import { nativeApi } from '../services/native'
import { ChevronDown, Folder, GitBranch, Play } from 'lucide-solid'
import { CliAvatar } from './CliAvatar'
import { useStore } from '../store'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'

type OsPlatform = 'windows' | 'chromeos' | 'macos' | 'gnome' | 'mobile'

interface GitBranchInfo {
  repoName?: string | null
  head: string
}

interface ProjectFsEvent {
  projectPath: string
  paths: string[]
}

function mapNativePlatform(value: string): OsPlatform {
  switch (value) {
    case 'macos':
      return 'macos'
    case 'linux':
      return 'gnome'
    case 'android':
    case 'ios':
      return 'mobile'
    case 'windows':
    default:
      return 'windows'
  }
}

function TitlebarButton({
  class: className,
  onClick,
  children,
  label,
}: {
  class?: string
  onClick: () => void | Promise<void>
  children: JSX.Element
  label: string
}) {
  return (
    <button
      type="button"
      onClick={(event) => {
        event.stopPropagation()
        void onClick()
      }}
      class={`relative z-20 pointer-events-auto ${className ?? ''}`}
      style={{ 'webkit-app-region': 'no-drag' } as JSX.CSSProperties}
      aria-label={label}
    >
      {children}
    </button>
  )
}

async function runWindowAction(action: string, operation: () => Promise<void>) {
  try {
    await operation()
  } catch (error) {
    console.error(`Title bar action failed: ${action}`, error)
  }
}

function currentWindow() {
  return nativeApi.window()
}

function WindowsGlyph({
  glyph,
  class: className = '',
}: {
  glyph: string
  class?: string
}) {
  return (
    <span
      class={className}
      style={{
        'font-family': '"Segoe Fluent Icons", "Segoe MDL2 Assets", "Segoe UI Symbol", sans-serif',
        'font-size': '10px',
        'line-height': '1',
        'font-weight': '400',
      } as JSX.CSSProperties}
      aria-hidden="true"
    >
      {glyph}
    </span>
  )
}

function WindowsControls({
  isMaximized,
  setIsMaximized,
}: {
  isMaximized: boolean
  setIsMaximized: Setter<boolean>
}) {
  const baseButtonClass = 'bg-transparent text-foreground/90 hover:bg-white/[0.06] active:bg-white/[0.04]'
  const closeButtonClass = 'bg-transparent text-foreground/90 hover:bg-[#c42b1c] hover:text-white active:bg-[#a1261b]'

  return (
    <div class="flex h-8" style={{ 'webkit-app-region': 'no-drag' } as JSX.CSSProperties}>
      <TitlebarButton
        label="Minimize"
        onClick={() => runWindowAction('minimize', () => currentWindow().minimize())}
        class={`inline-flex h-8 w-[46px] items-center justify-center transition-colors ${baseButtonClass}`}
      >
        <WindowsGlyph glyph={'\uE921'} />
      </TitlebarButton>
      <TitlebarButton
        label={isMaximized ? 'Restore' : 'Maximize'}
        onClick={async () => {
          await runWindowAction('toggle maximize', async () => {
            const window = currentWindow()
            await window.toggleMaximize()
            const next = await window.isMaximized()
            setIsMaximized(next)
          })
        }}
        class={`inline-flex h-8 w-[46px] items-center justify-center transition-colors ${baseButtonClass}`}
      >
        <WindowsGlyph glyph={isMaximized ? '\uE923' : '\uE922'} />
      </TitlebarButton>
      <TitlebarButton
        label="Close"
        onClick={() => runWindowAction('close', () => currentWindow().close())}
        class={`inline-flex h-8 w-[46px] items-center justify-center transition-colors ${closeButtonClass}`}
      >
        <WindowsGlyph glyph={'\uE8BB'} />
      </TitlebarButton>
    </div>
  )
}

function MacControls() {
  return (
    <div class="flex items-center gap-2 px-3" style={{ 'webkit-app-region': 'no-drag' } as JSX.CSSProperties}>
      <TitlebarButton
        label="Close"
        onClick={() => runWindowAction('close', () => currentWindow().close())}
        class="flex h-3 w-3 items-center justify-center rounded-full border border-black/[0.12] bg-[#ff544d] text-black/60"
      >
        <svg width="6" height="6" viewBox="0 0 16 18" fill="none" xmlns="http://www.w3.org/2000/svg" class="opacity-0 hover:opacity-100">
          <path d="M15.7522 4.44381L11.1543 9.04165L15.7494 13.6368C16.0898 13.9771 16.078 14.5407 15.724 14.8947L13.8907 16.728C13.5358 17.0829 12.9731 17.0938 12.6328 16.7534L8.03766 12.1583L3.44437 16.7507C3.10402 17.091 2.54132 17.0801 2.18645 16.7253L0.273257 14.8121C-0.0807018 14.4572 -0.0925004 13.8945 0.247845 13.5542L4.84024 8.96087L0.32499 4.44653C-0.0153555 4.10619 -0.00355681 3.54258 0.350402 3.18862L2.18373 1.35529C2.53859 1.00042 3.1013 0.989533 3.44164 1.32988L7.95689 5.84422L12.5556 1.24638C12.8951 0.906035 13.4587 0.917833 13.8126 1.27179L15.7267 3.18589C16.0807 3.53985 16.0925 4.10346 15.7522 4.44381Z" fill="currentColor" />
        </svg>
      </TitlebarButton>
      <TitlebarButton
        label="Minimize"
        onClick={() => runWindowAction('minimize', () => currentWindow().minimize())}
        class="flex h-3 w-3 items-center justify-center rounded-full border border-black/[0.12] bg-[#ffbd2e] text-black/60"
      >
        <svg width="8" height="8" viewBox="0 0 17 6" fill="none" xmlns="http://www.w3.org/2000/svg" class="opacity-0 hover:opacity-100">
          <path fillRule="evenodd" clipRule="evenodd" d="M1.47211 1.18042H15.4197C15.8052 1.18042 16.1179 1.50551 16.1179 1.90769V3.73242C16.1179 4.13387 15.8052 4.80006 15.4197 4.80006H1.47211C1.08665 4.80006 0.773926 4.47497 0.773926 4.07278V1.90769C0.773926 1.50551 1.08665 1.18042 1.47211 1.18042Z" fill="currentColor" />
        </svg>
      </TitlebarButton>
      <TitlebarButton
        label="Maximize"
        onClick={async () => {
          await runWindowAction('toggle maximize', async () => {
            await currentWindow().toggleMaximize()
          })
        }}
        class="flex h-3 w-3 items-center justify-center rounded-full border border-black/[0.12] bg-[#28c93f] text-black/60"
      >
        <svg width="8" height="8" viewBox="0 0 17 16" fill="none" xmlns="http://www.w3.org/2000/svg" class="opacity-0 hover:opacity-100">
          <path fillRule="evenodd" clipRule="evenodd" d="M15.5308 9.80147H10.3199V15.0095C10.3199 15.3949 9.9941 15.7076 9.59265 15.7076H7.51555C7.11337 15.7076 6.78828 15.3949 6.78828 15.0095V9.80147H1.58319C1.19774 9.80147 0.88501 9.47638 0.88501 9.07419V6.90619C0.88501 6.50401 1.19774 6.17892 1.58319 6.17892H6.78828V1.06183C6.78828 0.676375 7.11337 0.363647 7.51555 0.363647H9.59265C9.9941 0.363647 10.3199 0.676375 10.3199 1.06183V6.17892H15.5308C15.9163 6.17892 16.229 6.50401 16.229 6.90619V9.07419C16.229 9.47638 15.9163 9.80147 15.5308 9.80147Z" fill="currentColor" />
        </svg>
      </TitlebarButton>
    </div>
  )
}

function GnomeControls({
  isMaximized,
  setIsMaximized,
}: {
  isMaximized: boolean
  setIsMaximized: Setter<boolean>
}) {
  return (
    <div class="mr-[10px] flex items-center space-x-[13px]" style={{ 'webkit-app-region': 'no-drag' } as JSX.CSSProperties}>
      <TitlebarButton
        label="Minimize"
        onClick={() => runWindowAction('minimize', () => currentWindow().minimize())}
        class="flex h-6 w-6 items-center justify-center rounded-full bg-[#373737] p-0 text-white hover:bg-[#424242] active:bg-[#565656]"
      >
        <svg width="10" height="1" viewBox="0 0 10 1" fill="none" xmlns="http://www.w3.org/2000/svg" class="h-[9px] w-[9px]">
          <path d="M0 0.5h10" stroke="currentColor" stroke-width="1" />
        </svg>
      </TitlebarButton>
      <TitlebarButton
        label={isMaximized ? 'Restore' : 'Maximize'}
        onClick={async () => {
          await runWindowAction('toggle maximize', async () => {
            const window = currentWindow()
            await window.toggleMaximize()
            const next = await window.isMaximized()
            setIsMaximized(next)
          })
        }}
        class="flex h-6 w-6 items-center justify-center rounded-full bg-[#373737] p-0 text-white hover:bg-[#424242] active:bg-[#565656]"
      >
        {isMaximized ? (
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none" xmlns="http://www.w3.org/2000/svg" class="h-[9px] w-[9px]">
            <path d="M3 1h6v6H8V2H3V1ZM1 3h6v6H1V3Z" fill="currentColor" />
          </svg>
        ) : (
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none" xmlns="http://www.w3.org/2000/svg" class="h-2 w-2">
            <rect x="1" y="1" width="8" height="8" stroke="currentColor" />
          </svg>
        )}
      </TitlebarButton>
      <TitlebarButton
        label="Close"
        onClick={() => runWindowAction('close', () => currentWindow().close())}
        class="flex h-6 w-6 items-center justify-center rounded-full bg-[#373737] p-0 text-white hover:bg-[#424242] active:bg-[#565656]"
      >
        <svg width="10" height="10" viewBox="0 0 10 10" fill="none" xmlns="http://www.w3.org/2000/svg" class="h-2 w-2">
          <path d="M1 1l8 8M9 1 1 9" stroke="currentColor" stroke-width="1.2" />
        </svg>
      </TitlebarButton>
    </div>
  )
}

export function TitleBar() {
  const [osPlatform, setOsPlatform] = createSignal<OsPlatform>('windows')
  const [isMaximized, setIsMaximized] = createSignal(false)
  const projects = useStore((s) => s.projects)
  const currentProjectId = useStore((s) => s.currentProjectId)
  const activeSessionId = useStore((s) => s.activeSessionId)
  const cliTools = useStore((s) => s.cliTools)
  const cliLaunchMode = useStore((s) => s.cliLaunchMode)
  const launchCliSession = useStore((s) => s.launchCliSession)
  const getCurrentCliTool = useStore((s) => s.getCurrentCliTool)
  const setPreferredCliTool = useStore((s) => s.setPreferredCliTool)
  const updateSession = useStore((s) => s.updateSession)
  const [isLauncherOpen, setIsLauncherOpen] = createSignal(false)
  const [isSidebarVisible, setIsSidebarVisible] = createSignal(true)
  const [isFileTreeVisible, setIsFileTreeVisible] = createSignal(false)
  const [branchInfo, setBranchInfo] = createSignal<GitBranchInfo | null>(null)
  let launcherRef: HTMLDivElement | undefined
  let branchRefreshTimeout: number | null = null
  const currentProject = createMemo(
    () => projects().find((project) => project.id === currentProjectId()) ?? null,
  )
  const currentCliTool = () => getCurrentCliTool()
  const installedCliTools = createMemo(() => cliTools().filter((tool) => tool.installed))

  const loadBranchInfo = async (projectPath?: string | null) => {
    if (!projectPath) {
      setBranchInfo(null)
      return
    }

    try {
      const branch = await nativeApi.invoke('get_git_branch', {
        path: projectPath,
      }) as GitBranchInfo
      setBranchInfo(branch)
    } catch {
      setBranchInfo(null)
    }
  }

  const scheduleBranchRefresh = (projectPath?: string | null, delay = 180) => {
    if (!projectPath) {
      setBranchInfo(null)
      return
    }

    if (branchRefreshTimeout) {
      window.clearTimeout(branchRefreshTimeout)
    }

    branchRefreshTimeout = window.setTimeout(() => {
      void loadBranchInfo(projectPath)
    }, delay)
  }

  onMount(() => {
    let mounted = true
    const windowHandle = currentWindow()
    let resizeSyncTimer: number | null = null

    const sync = async () => {
      try {
        const currentPlatform = nativeApi.platform()
        if (mounted) {
          setOsPlatform(mapNativePlatform(currentPlatform))
        }

        const maximized = await windowHandle.isMaximized()
        if (mounted) {
          setIsMaximized(maximized)
        }
      } catch (error) {
        console.error('Failed to sync title bar platform:', error)
      }
    }

    void sync()

    const unlistenPromise = windowHandle.onResized(() => {
      if (resizeSyncTimer !== null) {
        window.clearTimeout(resizeSyncTimer)
      }

      resizeSyncTimer = window.setTimeout(async () => {
        const maximized = await windowHandle.isMaximized().catch(() => false)
        if (mounted) {
          setIsMaximized(maximized)
        }
      }, 120)
    })

    onCleanup(() => {
      mounted = false
      if (resizeSyncTimer !== null) {
        window.clearTimeout(resizeSyncTimer)
      }
      unlistenPromise.then((unlisten) => unlisten())
    })
  })

  createEffect(() => {
    const handleSidebarState = (event: Event) => {
      const detail = (event as CustomEvent<{ isSidebarVisible: boolean }>).detail
      if (!detail) return
      setIsSidebarVisible(Boolean(detail.isSidebarVisible))
    }

    window.addEventListener('gg-sidebar-state', handleSidebarState as EventListener)
    return () => window.removeEventListener('gg-sidebar-state', handleSidebarState as EventListener)
  })

  createEffect(() => {
    const handleFileTreeState = (event: Event) => {
      const detail = (event as CustomEvent<{ isFileTreeVisible: boolean }>).detail
      if (!detail) return
      setIsFileTreeVisible(Boolean(detail.isFileTreeVisible))
    }

    window.addEventListener('gg-file-tree-state', handleFileTreeState as EventListener)
    return () => window.removeEventListener('gg-file-tree-state', handleFileTreeState as EventListener)
  })

  createEffect(() => {
    const cp = currentProject()
    void loadBranchInfo(cp?.path)
  })

  createEffect(() => {
    const cp = currentProject()
    if (!cp?.path) return

    const unlistenProjectPromise = nativeApi.listen<ProjectFsEvent>('project-fs-event', (event) => {
      if (event.payload.projectPath !== cp.path) return

      const touchesGitState = event.payload.paths.some((path) => {
        const normalized = path.replace(/\\/g, '/').toLowerCase()
        return normalized.includes('/.git/') || normalized.endsWith('/.git') || normalized.endsWith('/head')
      })

      if (!touchesGitState) return

      scheduleBranchRefresh(cp.path, 140)
    })

    onCleanup(() => {
      if (branchRefreshTimeout) {
        window.clearTimeout(branchRefreshTimeout)
        branchRefreshTimeout = null
      }
      unlistenProjectPromise.then((unlisten) => unlisten())
    })
  })

  const handleLaunchSession = async (cliId?: string | null) => {
    const cpid = currentProjectId()
    if (!cpid) return

    const selectedCli = installedCliTools().find((tool) => tool.id === cliId) ?? null

    if (cliId) {
      setPreferredCliTool(cliId)
    }

    if (
      cliLaunchMode() === 'replace-current' &&
      activeSessionId()
    ) {
      const launchCommand = selectedCli?.matchedCommand ?? cliId
      if (!launchCommand) {
        setIsLauncherOpen(false)
        return
      }

      window.dispatchEvent(
        new CustomEvent('gg-rerun-cli-current-session', {
          detail: {
            sessionId: activeSessionId(),
            command: launchCommand,
          },
        }),
      )

      if (selectedCli) {
        await updateSession(cpid, activeSessionId()!, { cliTool: selectedCli.id })
      }

      setIsLauncherOpen(false)
      return
    }

    await launchCliSession(cpid, cliId)
    setIsLauncherOpen(false)
  }

  const handleToggleSidebar = () => {
    window.dispatchEvent(new Event('gg-toggle-sidebar'))
  }

  const handleToggleFileTree = () => {
    window.dispatchEvent(new Event('gg-toggle-file-tree'))
  }

  const headerClass = 'border-white/[0.06] bg-[#121212] text-white'
  const navButtonClass = 'text-[#6f6f6f] hover:bg-white/[0.05] hover:text-white'

  return (
    <header class={`relative z-50 flex h-[40px] shrink-0 select-none items-center border-b ${headerClass}`}>
      {osPlatform() === 'macos' && <MacControls />}

      <div class="flex min-w-0 items-center gap-3 px-3" style={{ 'webkit-app-region': 'no-drag' } as JSX.CSSProperties}>
        <div class="flex items-center gap-2">
          <Button
            type="button"
            onClick={handleToggleSidebar}
            variant="ghost"
            size="icon-xs"
            class={`h-6 w-6 rounded-md ${navButtonClass}`}
            aria-label="Sidebar toggle"
            aria-pressed={isSidebarVisible()}
            title={isSidebarVisible() ? 'Hide sidebar' : 'Show sidebar'}
          >
            <span class="flex h-3.5 w-3.5 items-center justify-center rounded-[3px] border border-current">
              <span class="h-full w-[1px] bg-current opacity-80" />
            </span>
          </Button>

          {(() => {
            const bi = branchInfo()
            if (!bi?.head) return null
            return (
              <div class="ml-1 hidden items-center gap-2 rounded-md bg-white/[0.03] px-2 py-1 text-xs text-current/65 lg:flex">
                <GitBranch class="h-3.5 w-3.5" stroke-width={1.9} />
                <span class="max-w-[170px] truncate">{bi.head}</span>
              </div>
            )
          })()}
        </div>
      </div>

      <div
        data-electron-drag-region
        class="min-w-0 flex-1 self-stretch px-3 text-center text-[12px] font-medium text-current/60"
        style={{ 'webkit-app-region': 'drag' } as JSX.CSSProperties}
      />

      <div class="flex items-center gap-2 px-2" style={{ 'webkit-app-region': 'no-drag' } as JSX.CSSProperties}>
        <div ref={launcherRef!} class="relative flex items-center">
          <DropdownMenu open={isLauncherOpen()} onOpenChange={setIsLauncherOpen}>
            <DropdownMenuTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                class="h-7 min-w-[108px] items-center justify-between gap-2 rounded-md border border-white/10 bg-white/[0.03] px-2.5 text-xs font-medium leading-none text-current/80 hover:bg-white/[0.06]"
                title={currentCliTool() ? `Current CLI: ${currentCliTool()!.label}` : 'Choose CLI launcher'}
              >
                {(() => {
                  const cct = currentCliTool()
                  if (!cct) return <Play class="h-3.5 w-3.5 fill-current" stroke-width={1.8} />
                  return (
                    <>
                      <CliAvatar cliId={cct.id} label={cct.label} size="sm" />
                      <span class="hidden sm:inline">{cct.label}</span>
                    </>
                  )
                })()}
                <ChevronDown class={`h-3.5 w-3.5 transition ${isLauncherOpen() ? 'rotate-180' : ''}`} stroke-width={1.9} />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" class="w-[208px]">
              {(() => {
                const tools = installedCliTools()
                if (tools.length === 0) {
                  return <div class="px-3 py-2 text-[13px] text-muted-foreground">No available CLI found</div>
                }
                return tools.map((tool) => (
                  <DropdownMenuItem
                    key={tool.id}
                    onClick={() => void handleLaunchSession(tool.id)}
                    class="gap-2.5 px-3 py-2"
                  >
                    <span class="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-muted/50">
                      <CliAvatar cliId={tool.id} label={tool.label} size="sm" class="rounded-md" />
                    </span>
                    <span class="min-w-0 flex-1 truncate text-[13px] font-medium">{tool.label}</span>
                  </DropdownMenuItem>
                ))
              })()}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>

        <Button
          type="button"
          onClick={handleToggleFileTree}
          variant="ghost"
          size="icon-sm"
          class={`h-7 w-7 ${
            isFileTreeVisible()
              ? 'bg-[#1e2a1f] text-[#9bdd9f]'
              : 'text-current/65 hover:bg-white/[0.05] hover:text-current'
          }`}
          title={isFileTreeVisible() ? 'Hide file tree' : 'Show file tree'}
          aria-pressed={isFileTreeVisible()}
        >
          <Folder class="h-4 w-4" stroke-width={1.9} />
        </Button>
      </div>

      {((osPlatform() === 'windows' || osPlatform() === 'chromeos') && (
        <WindowsControls
          isMaximized={isMaximized()}
          setIsMaximized={setIsMaximized}
        />
      ))}
      {osPlatform() === 'gnome' && (
        <GnomeControls isMaximized={isMaximized()} setIsMaximized={setIsMaximized} />
      )}
    </header>
  )
}
```

- [ ] **Step 2: Commit**

```bash
git add src/components/TitleBar.tsx
git commit -m "refactor: convert TitleBar to SolidJS"
```

---

### Task 9: Convert remaining app components (Sidebar, Terminal, WelcomeScreen, FileTree, CliAvatar)

**Files:**
- Modify: `src/components/Sidebar.tsx`
- Modify: `src/components/Terminal.tsx`
- Modify: `src/components/WelcomeScreen.tsx`
- Modify: `src/components/FileTree.tsx`
- Modify: `src/components/CliAvatar.tsx`

- [ ] **Step 1: Convert Sidebar.tsx**

Replace the entire file. Key changes:
- `useState` → `createSignal`
- `useEffect` → `createEffect`
- `useMemo` → `createMemo`
- `className` → `class`
- `lucide-react` → `lucide-solid`
- Store destructuring uses new pattern
- `React.MouseEvent` → `MouseEvent`

This is a large file (~1000 lines). The conversion follows the same pattern as TitleBar. All React imports removed, all hooks converted, all `className` → `class`.

- [ ] **Step 2: Convert Terminal.tsx**

Replace the entire file. Key changes:
- `useRef` → direct `let` variables (SolidJS ref pattern)
- `useState` → `createSignal`
- `useEffect` → `createEffect`
- `useCallback` → plain functions
- `className` → `class`
- `lucide-react` → `lucide-solid`
- Store selectors use new pattern
- The xterm.js integration stays the same (framework-agnostic)

- [ ] **Step 3: Convert WelcomeScreen.tsx**

Replace the entire file. Key changes:
- `useMemo` → `createMemo`
- `className` → `class`
- `lucide-react` → `lucide-solid`

- [ ] **Step 4: Convert FileTree.tsx**

Replace the entire file. Key changes:
- `useState` → `createSignal`
- `useEffect` → `createEffect`
- `useMemo` → `createMemo`
- `useCallback` → plain functions
- `useRef` → `let` variables
- `memo` from React → `memo` from solid-js (or remove if not needed)
- `className` → `class`
- `lucide-react` → `lucide-solid`
- `React.MouseEvent` → `MouseEvent`

- [ ] **Step 5: Convert CliAvatar.tsx**

Replace the entire file. Key changes:
- `className` → `class`

- [ ] **Step 6: Commit**

```bash
git add src/components/Sidebar.tsx src/components/Terminal.tsx src/components/WelcomeScreen.tsx src/components/FileTree.tsx src/components/CliAvatar.tsx
git commit -m "refactor: convert remaining app components to SolidJS"
```

---

### Task 10: Convert all UI components from Radix UI to Kobalte

**Files:**
- Modify: All files in `src/components/ui/`

- [ ] **Step 1: Convert button.tsx**

Replace the entire file. Key changes:
- `radix-ui` Slot → Kobalte's Polymorphic pattern or native element
- `React.ComponentProps` → SolidJS JSX types
- `className` → `class`

```typescript
import { cva, type VariantProps } from 'class-variance-authority'
import type { ComponentProps, ValidComponent } from 'solid-js'
import { splitProps } from 'solid-js'
import * as Solid from 'solid-js'

import { cn } from '@/lib/utils'

const buttonVariants = cva(
  'group/button inline-flex shrink-0 items-center justify-center rounded-lg border border-transparent bg-clip-padding text-sm font-medium whitespace-nowrap transition-all outline-none select-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 active:not-aria-[haspopup]:translate-y-px disabled:pointer-events-none disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*=\'size-\'])]:size-4',
  {
    variants: {
      variant: {
        default: 'bg-primary text-primary-foreground [a]:hover:bg-primary/80',
        outline:
          'border-border bg-background hover:bg-muted hover:text-foreground aria-expanded:bg-muted aria-expanded:text-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50',
        secondary:
          'bg-secondary text-secondary-foreground hover:bg-secondary/80 aria-expanded:bg-secondary aria-expanded:text-secondary-foreground',
        ghost:
          'hover:bg-muted hover:text-foreground aria-expanded:bg-muted aria-expanded:text-foreground dark:hover:bg-muted/50',
        destructive:
          'bg-destructive/10 text-destructive hover:bg-destructive/20 focus-visible:border-destructive/40 focus-visible:ring-destructive/20 dark:bg-destructive/20 dark:hover:bg-destructive/30 dark:focus-visible:ring-destructive/40',
        link: 'text-primary underline-offset-4 hover:underline',
      },
      size: {
        default:
          'h-8 gap-1.5 px-2.5 has-data-[slot=button-group]:pr-2 has-data-[slot=button-group]:pl-2',
        xs: 'h-6 gap-1 rounded-[min(var(--radius-md),10px)] px-2 text-xs in-data-[slot=button-group]:rounded-lg has-data-[slot=button-group]:pr-1.5 has-data-[slot=button-group]:pl-1.5 [&_svg:not([class*=\'size-\'])]:size-3',
        sm: 'h-7 gap-1 rounded-[min(var(--radius-md),12px)] px-2.5 text-[0.8rem] in-data-[slot=button-group]:rounded-lg has-data-[slot=button-group]:pr-1.5 has-data-[slot=button-group]:pl-1.5 [&_svg:not([class*=\'size-\'])]:size-3.5',
        lg: 'h-9 gap-1.5 px-2.5 has-data-[slot=button-group]:pr-3 has-data-[slot=button-group]:pl-3',
        icon: 'size-8',
        'icon-xs':
          'size-6 rounded-[min(var(--radius-md),10px)] in-data-[slot=button-group]:rounded-lg [&_svg:not([class*=\'size-\'])]:size-3',
        'icon-sm':
          'size-7 rounded-[min(var(--radius-md),12px)] in-data-[slot=button-group]:rounded-lg',
        'icon-lg': 'size-9',
      },
    },
    defaultVariants: {
      variant: 'default',
      size: 'default',
    },
  }
)

function Button<T extends ValidComponent = 'button'>(
  props: ComponentProps<T> &
    VariantProps<typeof buttonVariants> & {
      asChild?: boolean
    }
) {
  const [local, rest] = splitProps(props, ['class', 'variant', 'size', 'asChild'])

  return (
    <button
      data-slot="button"
      data-variant={local.variant ?? 'default'}
      data-size={local.size ?? 'default'}
      class={cn(buttonVariants({ variant: local.variant, size: local.size, class: local.class }))}
      {...rest}
    />
  )
}

export { Button, buttonVariants }
```

- [ ] **Step 2: Convert dialog.tsx to Kobalte Dialog**

Replace the entire file. Use `@kobalte/core/dialog` instead of `radix-ui`:

```typescript
import type { ComponentProps, JSX } from 'solid-js'
import { splitProps } from 'solid-js'
import { Dialog as DialogPrimitive } from '@kobalte/core/dialog'

import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { X } from 'lucide-solid'

function Dialog(props: ComponentProps<typeof DialogPrimitive>) {
  return <DialogPrimitive {...props} />
}

function DialogTrigger(props: ComponentProps<typeof DialogPrimitive.Trigger>) {
  return <DialogPrimitive.Trigger {...props} />
}

function DialogClose(props: ComponentProps<typeof DialogPrimitive.CloseButton>) {
  return <DialogPrimitive.CloseButton {...props} />
}

function DialogOverlay(props: ComponentProps<typeof DialogPrimitive.Overlay>) {
  const [local, rest] = splitProps(props, ['class'])

  return (
    <DialogPrimitive.Overlay
      class={cn(
        'fixed inset-0 isolate z-50 bg-black/55 duration-100 supports-backdrop-filter:backdrop-blur-sm data-open:animate-in data-open:fade-in-0 data-closed:animate-out data-closed:fade-out-0',
        local.class
      )}
      {...rest}
    />
  )
}

function DialogContent(props: ComponentProps<typeof DialogPrimitive.Content> & {
  showCloseButton?: boolean
}) {
  const [local, rest] = splitProps(props, ['class', 'children', 'showCloseButton'])

  return (
    <DialogPrimitive.Portal>
      <DialogOverlay />
      <DialogPrimitive.Content
        class={cn(
          'fixed top-1/2 left-1/2 z-50 grid w-full max-w-[calc(100%-2rem)] -translate-x-1/2 -translate-y-1/2 gap-4 rounded-xl bg-popover p-4 text-sm text-popover-foreground ring-1 ring-foreground/10 duration-100 outline-none sm:max-w-sm data-open:animate-in data-open:fade-in-0 data-open:zoom-in-95 data-closed:animate-out data-closed:fade-out-0 data-closed:zoom-out-95',
          local.class
        )}
        {...rest}
      >
        {local.children}
        {(local.showCloseButton ?? true) && (
          <DialogPrimitive.CloseButton as={Button} variant="ghost" class="absolute top-2 right-2" size="icon-sm">
            <X />
            <span class="sr-only">Close</span>
          </DialogPrimitive.CloseButton>
        )}
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  )
}

function DialogHeader(props: ComponentProps<'div'>) {
  const [local, rest] = splitProps(props, ['class'])

  return (
    <div
      class={cn('flex flex-col gap-2', local.class)}
      {...rest}
    />
  )
}

function DialogFooter(props: ComponentProps<'div'> & {
  showCloseButton?: boolean
}) {
  const [local, rest] = splitProps(props, ['class', 'showCloseButton', 'children'])

  return (
    <div
      class={cn(
        '-mx-4 -mb-4 flex flex-col-reverse gap-2 rounded-b-xl border-t bg-muted/50 p-4 sm:flex-row sm:justify-end',
        local.class
      )}
      {...rest}
    >
      {local.children}
      {local.showCloseButton && (
        <DialogPrimitive.CloseButton as={Button} variant="outline">Close</DialogPrimitive.CloseButton>
      )}
    </div>
  )
}

function DialogTitle(props: ComponentProps<typeof DialogPrimitive.Title>) {
  const [local, rest] = splitProps(props, ['class'])

  return (
    <DialogPrimitive.Title
      class={cn(
        'font-heading text-base leading-none font-medium',
        local.class
      )}
      {...rest}
    />
  )
}

function DialogDescription(props: ComponentProps<typeof DialogPrimitive.Description>) {
  const [local, rest] = splitProps(props, ['class'])

  return (
    <DialogPrimitive.Description
      class={cn(
        'text-sm text-muted-foreground *:[a]:underline *:[a]:underline-offset-3 *:[a]:hover:text-foreground',
        local.class
      )}
      {...rest}
    />
  )
}

export {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogOverlay,
  DialogTitle,
  DialogTrigger,
}
```

- [ ] **Step 3: Convert remaining UI components**

Convert each UI component file following the same pattern:
- Replace `radix-ui` imports with `@kobalte/core/*` equivalents
- Replace `React.ComponentProps` with SolidJS `ComponentProps`
- Replace `className` with `class`
- Replace `lucide-react` with `lucide-solid`
- Use `splitProps` from solid-js for prop splitting

Components to convert in order:
1. `select.tsx` → `@kobalte/core/select`
2. `dropdown-menu.tsx` → `@kobalte/core/dropdown-menu`
3. `tabs.tsx` → `@kobalte/core/tabs`
4. `checkbox.tsx` → `@kobalte/core/checkbox`
5. `switch.tsx` → `@kobalte/core/switch`
6. `slider.tsx` → `@kobalte/core/slider`
7. `progress.tsx` → `@kobalte/core/progress`
8. `accordion.tsx` → `@kobalte/core/accordion`
9. `collapsible.tsx` → `@kobalte/core/collapsible`
10. `popover.tsx` → `@kobalte/core/popover`
11. `hover-card.tsx` → `@kobalte/core/popover`
12. `tooltip.tsx` → `@kobalte/core/tooltip`
13. `context-menu.tsx` → `@kobalte/core/context-menu`
14. `menubar.tsx` → `@kobalte/core/menubar`
15. `navigation-menu.tsx` → `@kobalte/core/navigation-menu`
16. `combobox.tsx` → `@kobalte/core/combobox`
17. `command.tsx` → custom with Kobalte
18. `alert-dialog.tsx` → `@kobalte/core/alert-dialog`
19. `drawer.tsx` → `@kobalte/core/dialog` (drawer variant)
20. `sheet.tsx` → `@kobalte/core/dialog` (sheet variant)
21. `avatar.tsx` → `@kobalte/core/avatar`
22. `scroll-area.tsx` → `@kobalte/core/scroll-area`
23. `separator.tsx` → `@kobalte/core/separator`
24. `radio-group.tsx` → `@kobalte/core/radio-group`
25. `toggle.tsx` → `@kobalte/core/toggle`
26. `toggle-group.tsx` → `@kobalte/core/toggle-group`
27. `label.tsx` → `@kobalte/core/label`
28. `field.tsx` → `@kobalte/core/field`
29. `calendar.tsx` → custom with date-fns
30. `carousel.tsx` → custom carousel
31. `resizable.tsx` → custom resize
32. `input-otp.tsx` → custom OTP
33. `sonner.tsx` → `solid-sonner`
34. `chart.tsx` → custom SVG charts
35. Plain components (no Radix): badge, card, alert, skeleton, spinner, table, textarea, input, input-group, pagination, direction, empty, item, kbd, native-select, aspect-ratio, sidebar (ui), breadcrumb, button-group - only need `className` → `class` and icon import changes

- [ ] **Step 4: Update components/index.ts barrel exports**

Read and update the barrel export file to reflect any renamed exports from Kobalte conversions.

- [ ] **Step 5: Commit**

```bash
git add src/components/ui/
git commit -m "refactor: convert all UI components from Radix UI to Kobalte"
```

---

### Task 11: Update services, config, and types

**Files:**
- Modify: `src/services/native.ts`
- Modify: `src/services/api.ts`
- Modify: `src/services/index.ts`
- Modify: `src/config/index.ts`
- Modify: `src/config/cli-ui.ts`
- Modify: `src/config/check.ts`

- [ ] **Step 1: Review and update service files**

Most service files should be framework-agnostic. Check for any React-specific patterns and remove them. The `nativeApi` and `api` services should work unchanged since they don't use React.

- [ ] **Step 2: Update config files**

Check config files for any React-specific imports or patterns. Update icon references if needed.

- [ ] **Step 3: Commit**

```bash
git add src/services/ src/config/
git commit -m "chore: verify services and config SolidJS compatibility"
```

---

### Task 12: Final cleanup and verification

**Files:**
- Modify: `src/components/index.ts`
- Modify: `src/types/index.ts`
- Modify: `src/utils/index.ts`
- Modify: `src/utils/storage.ts`
- Modify: `src/utils/string.ts`
- Modify: `src/constants/storage.ts`

- [ ] **Step 1: Remove all remaining React imports**

Search for any remaining `from "react"` or `from 'react'` imports:

```powershell
rg "from ['\"]react['\"]" src/
```

Fix any remaining files that still import from React.

- [ ] **Step 2: Remove all remaining `className` attributes**

Search for any remaining `className=` in TSX files:

```powershell
rg "className=" src/ --include "*.tsx"
```

Replace all with `class=`.

- [ ] **Step 3: Run TypeScript check**

```bash
pnpm tsc -b --noEmit
```

Fix any type errors.

- [ ] **Step 4: Run ESLint**

```bash
pnpm lint
```

Fix any lint errors.

- [ ] **Step 5: Build the renderer**

```bash
pnpm build:renderer
```

Verify the build succeeds.

- [ ] **Step 6: Test dev mode**

```bash
pnpm dev
```

Verify the Electron app launches and renders correctly with SolidJS.

- [ ] **Step 7: Final commit**

```bash
git add .
git commit -m "chore: final cleanup and verification of SolidJS migration"
```

---

### Task 13: Build Electron installer

**Files:**
- No file changes expected

- [ ] **Step 1: Full build**

```bash
pnpm build
pnpm build:electron
```

Verify the Electron installer is produced in the `release/` directory.

- [ ] **Step 2: Final commit**

```bash
git add .
git commit -m "chore: verify Electron build after SolidJS migration"
```
