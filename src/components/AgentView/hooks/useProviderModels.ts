import { useEffect, useState } from "react"
import { nativeApi } from "@/services/native"
import {
  buildConnectedOpenCodeModelOptions,
  pickOpenCodeModel,
  type OpenCodeModelOption,
} from "@/utils/opencode-models"

interface UseProviderModelsParams {
  isActive: boolean
  project: { path: string } | null
  session: {
    opencodeProviderId?: string | null
    opencodeModelId?: string | null
  } | null
  preferredOpencodeProviderId: string | undefined
  preferredOpencodeModelId: string | undefined
  preferredOpencodeVariant: string | undefined
}

export const useProviderModels = ({
  isActive,
  project,
  session,
  preferredOpencodeProviderId,
  preferredOpencodeModelId,
  preferredOpencodeVariant,
  visibleOpencodeModels = [],
}: UseProviderModelsParams & { visibleOpencodeModels?: string[] }) => {
  const [modelOptions, setModelOptions] = useState<OpenCodeModelOption[]>([])
  const [providerStatus, setProviderStatus] = useState<"idle" | "loading" | "ready" | "error">("idle")
  const [selectedModel, setSelectedModel] = useState("")
  const [modelPower, setModelPower] = useState(preferredOpencodeVariant || "high")

  useEffect(() => {
    setModelPower(preferredOpencodeVariant || "high")
  }, [preferredOpencodeVariant])

  useEffect(() => {
    if (!isActive || !project?.path) return

    let cancelled = false
    setProviderStatus("loading")
    nativeApi.invoke("opencode_provider_list", { directory: project.path })
      .then((providers) => {
        if (cancelled) return
        const options = buildConnectedOpenCodeModelOptions(providers)
        setModelOptions(options)
        setSelectedModel((current) => {
          const picked = pickOpenCodeModel({
            options,
            providers,
            currentValue: current,
            preferredProviderID: preferredOpencodeProviderId,
            preferredModelID: preferredOpencodeModelId,
            sessionProviderID: session?.opencodeProviderId,
            sessionModelID: session?.opencodeModelId,
          })
          return picked?.value ?? ""
        })
        setProviderStatus("ready")
      })
      .catch((error) => {
        if (cancelled) return
        console.error("Failed to load OpenCode providers:", error)
        setModelOptions([])
        setSelectedModel("")
        setProviderStatus("error")
      })

    return () => {
      cancelled = true
    }
  }, [
    isActive,
    preferredOpencodeModelId,
    preferredOpencodeProviderId,
    project?.path,
    session?.opencodeModelId,
    session?.opencodeProviderId,
    visibleOpencodeModels,
  ])

  return { modelOptions, providerStatus, selectedModel, setSelectedModel, modelPower, setModelPower }
}
