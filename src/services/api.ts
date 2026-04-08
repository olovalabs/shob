import { invoke } from '@tauri-apps/api/core';
import type { Project } from '../types';
import type { CliProbeResult } from '../config/check';

export const api = {
  getProjects: () => invoke<Project[]>('get_projects'),
  
  saveProject: (project: Project) => invoke<Project>('save_project', { project }),
  
  deleteProject: (projectId: string) => invoke('delete_project', { projectId }),

  saveSessionOutput: (sessionId: string, output: string) => invoke<void>('save_session_output', { sessionId, output }),

  loadSessionOutput: (sessionId: string) => invoke<string>('load_session_output', { sessionId }),
  
  getAvailableShells: () => invoke<string[]>('get_available_shells'),
  
  probeCliTools: (items: { id: string; commands: string[] }[]) =>
    invoke<CliProbeResult[]>('probe_cli_tools', { items }),
};
