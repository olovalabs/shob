import { useEffect, useRef } from "react"
import { nativeApi } from "@/services/native"
import type {
  ElectronOpencodeEventEnvelope,
  ElectronOpencodeEventSubscription,
} from "@/electron"

interface UseOpenCodeStreamParams {
  project: { path: string } | null
}

export const useOpenCodeStream = ({ project }: UseOpenCodeStreamParams) => {
  const opencodeEventSubscriptionRef = useRef<ElectronOpencodeEventSubscription | null>(null)

  useEffect(() => {
    if (!project?.path) return

    let disposed = false
    let subscription: ElectronOpencodeEventSubscription | null = null
    let unlisten: (() => void) | null = null

    void nativeApi.invoke("opencode_event_subscribe", {
      directory: project.path,
      global: true,
    }).then(async (created) => {
      if (disposed) {
        void nativeApi.invoke("opencode_event_unsubscribe", { id: created.id }).catch(() => undefined)
        return
      }

      subscription = created
      opencodeEventSubscriptionRef.current = created
      const cleanup = await nativeApi.listen<ElectronOpencodeEventEnvelope>(created.channel, ({ payload }) => {
        if (payload.type === "error") {
          console.warn("OpenCode event stream error:", payload.error)
        }
      })
      if (disposed) {
        cleanup()
        void nativeApi.invoke("opencode_event_unsubscribe", { id: created.id }).catch(() => undefined)
        return
      }
      unlisten = cleanup
    }).catch((error) => {
      console.warn("Failed to start OpenCode event stream:", error)
    })

    return () => {
      disposed = true
      if (unlisten) unlisten()
      if (opencodeEventSubscriptionRef.current?.id === subscription?.id) {
        opencodeEventSubscriptionRef.current = null
      }
      if (subscription) {
        void nativeApi.invoke("opencode_event_unsubscribe", { id: subscription.id }).catch(() => undefined)
      }
    }
  }, [project?.path])
}
