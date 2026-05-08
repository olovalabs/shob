type BaseModel = {
  id: string
  name: string
  reasoning: boolean
}

type BaseProvider = {
  id: string
  name: string
  env: string[]
  models: Record<string, BaseModel>
}

export function withAntigravityModels<TProvider extends BaseProvider & Record<string, any>>(result: Record<string, TProvider>) {
  const google = result.google
  if (!google) return result

  const fallback = google.models["gemini-3.1-pro-preview"] ?? Object.values(google.models).at(0)
  if (!fallback) return result

  const make = (id: string, seedId?: string, opts?: { reasoning?: boolean }): TProvider["models"][string] => {
    const seed = (seedId ? google.models[seedId] : undefined) ?? fallback
    return {
      ...seed,
      id,
      name:
        id === "gemini-3.1-pro-high"
          ? "Gemini 3 Pro High"
          : id === "gemini-3.1-pro-low"
            ? "Gemini 3 Pro Low"
            : id === "gemini-3-flash"
              ? "Gemini 3 Flash"
              : id === "claude-sonnet-4-6"
                ? "Claude Sonnet 4.6"
                : id === "claude-opus-4-6-thinking"
                  ? "Claude Opus 4.6 Thinking"
                  : "GPT OSS 120B Medium",
      reasoning: opts?.reasoning ?? seed.reasoning,
    } as TProvider["models"][string]
  }

  const models: TProvider["models"] = {
    "gemini-3.1-pro-high": make("gemini-3.1-pro-high", "gemini-3.1-pro-preview"),
    "gemini-3.1-pro-low": make("gemini-3.1-pro-low", "gemini-3.1-pro-preview"),
    "gemini-3-flash": make("gemini-3-flash", "gemini-3-flash-preview", { reasoning: false }),
    "claude-sonnet-4-6": make("claude-sonnet-4-6", "gemini-3.1-pro-preview"),
    "claude-opus-4-6-thinking": make("claude-opus-4-6-thinking", "gemini-3.1-pro-preview"),
    "gpt-oss-120b-medium": make("gpt-oss-120b-medium", "gemini-3.1-pro-preview"),
  } as TProvider["models"]

  const base = result.antigravity ?? google
  return {
    ...result,
    antigravity: {
      ...base,
      id: "antigravity",
      name: "Antigravity",
      env: [],
      models,
    },
  }
}
