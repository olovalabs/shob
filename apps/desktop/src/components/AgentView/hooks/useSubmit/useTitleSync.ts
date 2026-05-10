import { nativeApi } from "@/services/native"
import { shouldAdoptOpenCodeTitle } from "../../utils"

interface UseTitleSyncParams {
  project: { id: string; path: string }
  session: { id: string; name?: string | null }
  updateSession: (projectId: string, sessionId: string, updates: Record<string, unknown>) => Promise<void>
}

export const useTitleSync = ({ project, session, updateSession }: UseTitleSyncParams) => {
  const maybeSyncGeneratedTitle = async (opencodeSessionID: string, attempts = 1) => {
    for (let attempt = 0; attempt < attempts; attempt += 1) {
      if (attempt > 0) await new Promise((resolve) => setTimeout(resolve, 450))
      try {
        const info = await nativeApi.invoke("opencode_session_get", {
          directory: project.path,
          sessionID: opencodeSessionID,
        })
        if (shouldAdoptOpenCodeTitle(info.title, session.name ?? undefined) && info.title) {
          await updateSession(project.id, session.id, { name: info.title.trim() })
          return true
        }
      } catch (error) {
        console.warn("Failed to sync OpenCode generated title:", error)
      }
    }
    return false
  }

  return { maybeSyncGeneratedTitle }
}
