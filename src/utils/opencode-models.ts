import type { ElectronOpencodeProvider, ElectronOpencodeProviderList } from "@/electron"

export type OpenCodeModelOption = {
  value: string
  label: string
  shortLabel: string
  providerID: string
  providerName: string
  modelID: string
  modelName: string
  source?: ElectronOpencodeProvider["source"]
}

export const makeOpenCodeModelValue = (providerID: string, modelID: string) =>
  `${encodeURIComponent(providerID)}:${encodeURIComponent(modelID)}`

export const parseOpenCodeModelValue = (value: string, options: OpenCodeModelOption[] = []) => {
  const option = options.find((item) => item.value === value)
  if (option) return { providerID: option.providerID, modelID: option.modelID }

  const [provider, model] = value.split(":")
  if (provider && model) {
    return {
      providerID: decodeURIComponent(provider),
      modelID: decodeURIComponent(model),
    }
  }

  const [providerID, ...modelParts] = value.split("/")
  return {
    providerID: providerID || "",
    modelID: modelParts.join("/") || value,
  }
}

export const buildConnectedOpenCodeModelOptions = (
  providers: ElectronOpencodeProviderList | null | undefined,
): OpenCodeModelOption[] => {
  if (!providers?.all?.length) return []

  const connected = new Set(providers.connected ?? [])
  return providers.all
    .filter((provider) => connected.has(provider.id))
    .flatMap((provider) =>
      Object.values(provider.models ?? {}).map((model) => {
        const modelName = model.name || model.id
        return {
          value: makeOpenCodeModelValue(provider.id, model.id),
          label: `${provider.name} · ${modelName}`,
          shortLabel: modelName,
          providerID: provider.id,
          providerName: provider.name,
          modelID: model.id,
          modelName,
          source: provider.source,
        }
      }),
    )
    .sort((left, right) => left.label.localeCompare(right.label))
}

export const pickOpenCodeModel = ({
  options,
  providers,
  currentValue,
  preferredProviderID,
  preferredModelID,
  sessionProviderID,
  sessionModelID,
}: {
  options: OpenCodeModelOption[]
  providers?: ElectronOpencodeProviderList | null
  currentValue?: string | null
  preferredProviderID?: string | null
  preferredModelID?: string | null
  sessionProviderID?: string | null
  sessionModelID?: string | null
}) => {
  const find = (providerID?: string | null, modelID?: string | null) =>
    providerID && modelID
      ? options.find((item) => item.providerID === providerID && item.modelID === modelID)
      : undefined

  const current = currentValue ? options.find((item) => item.value === currentValue) : undefined
  if (current) return current

  const session = find(sessionProviderID, sessionModelID)
  if (session) return session

  const preferred = find(preferredProviderID, preferredModelID)
  if (preferred) return preferred

  for (const [providerID, modelID] of Object.entries(providers?.default ?? {})) {
    const match = find(providerID, modelID)
    if (match) return match
  }

  return options[0] ?? null
}
