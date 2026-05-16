import { invokeNative } from './native';
import type { Project } from '../types';
import type { CliProbeResult } from '../config/check';

const toSerializableProject = (project: Project): Project =>
  JSON.parse(JSON.stringify(project)) as Project;

export const api = {
  getProjects: () => invokeNative('get_projects'),
  
  saveProject: (project: Project) =>
    invokeNative('save_project', { project: toSerializableProject(project) }),
  
  deleteProject: (projectId: string) => invokeNative('delete_project', { projectId }),

  saveSessionOutput: (sessionId: string, output: string) => invokeNative('save_session_output', { sessionId, output }),

  loadSessionOutput: (sessionId: string) => invokeNative('load_session_output', { sessionId }),
  
  getAvailableShells: () => invokeNative('get_available_shells'),
  
  probeCliTools: (items: { id: string; commands: string[] }[]) =>
    invokeNative('probe_cli_tools', { items }) as Promise<CliProbeResult[]>,
};
