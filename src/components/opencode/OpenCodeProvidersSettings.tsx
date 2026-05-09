import { useCallback, useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react"
import {
  ArrowLeft,
  CircleAlert,
  ExternalLink,
  Loader2,
  Plus,
  RefreshCw,
  Search,
  Trash2,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { nativeApi } from "@/services/native"
import type {
  ElectronOpencodeProvider,
  ElectronOpencodeProviderList,
  ElectronOpencodeServerStatus,
} from "@/electron"
import { ProviderIcon } from "./ProviderIcon"

type AuthPrompt =
  | {
      type: "text"
      key: string
      message?: string
      placeholder?: string
      when?: { key: string; op: "eq" | "neq"; value: string }
    }
  | {
      type: "select"
      key: string
      message?: string
      options: Array<{ label: string; value: string; hint?: string }>
      when?: { key: string; op: "eq" | "neq"; value: string }
    }

type AuthMethod = {
  type: "oauth" | "api"
  label?: string
  prompts?: AuthPrompt[]
}

type OAuthAuthorization = {
  url: string
  method: "auto" | "code"
  instructions?: string
}

type ProviderConfig = {
  npm?: string
  name?: string
  options?: Record<string, unknown>
  models?: Record<string, { name?: string }>
}

const POPULAR_PROVIDER_IDS = [
  "opencode",
  "opencode-go",
  "anthropic",
  "github-copilot",
  "openai",
  "google",
  "antigravity",
  "openrouter",
  "kilo",
  "vercel",
]

const PROVIDER_NOTES: Array<{ match: (id: string) => boolean; note: string }> = [
  { match: (id) => id === "opencode", note: "Use OpenCode Zen, credits, and hosted models." },
  { match: (id) => id === "opencode-go", note: "Fast OpenCode account sign-in." },
  { match: (id) => id === "anthropic", note: "Claude models with tool use and coding support." },
  { match: (id) => id.startsWith("github-copilot"), note: "Use your GitHub Copilot subscription." },
  { match: (id) => id === "openai", note: "GPT models through your OpenAI API key." },
  { match: (id) => id === "google" || id === "antigravity", note: "Gemini models and Google auth." },
  { match: (id) => id === "openrouter", note: "One API key for many model providers." },
  { match: (id) => id === "vercel", note: "Vercel AI Gateway provider." },
]

const PROVIDER_ID = /^[a-z0-9][a-z0-9-_]*$/
const OPENAI_COMPATIBLE_NPM = "@ai-sdk/openai-compatible"

const connectedModelFilter = (provider: ElectronOpencodeProvider) =>
  provider.id !== "opencode" || Object.values(provider.models ?? {}).some((model) => model.cost?.input)

const formatError = (value: unknown, fallback = "Request failed") => {
  if (value instanceof Error && value.message) return value.message
  if (typeof value === "string" && value) return value
  if (value && typeof value === "object") {
    const data = value as { data?: { message?: unknown }; error?: unknown; message?: unknown }
    if (typeof data.data?.message === "string" && data.data.message) return data.data.message
    if (data.error) return formatError(data.error, fallback)
    if (typeof data.message === "string" && data.message) return data.message
    try {
      return JSON.stringify(value)
    } catch {
      return fallback
    }
  }
  return fallback
}

const methodLabel = (method?: AuthMethod) => {
  if (!method) return ""
  if (method.type === "api") return "API Key"
  return method.label || "OAuth"
}

const sourceLabel = (source?: ElectronOpencodeProvider["source"]) => {
  if (source === "env") return "Environment"
  if (source === "api") return "API Key"
  if (source === "config") return "Config"
  if (source === "custom") return "Custom"
  return "Other"
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-3">
      <h2 className="px-1 text-[13px] font-semibold text-foreground">{title}</h2>
      <div className="rounded-xl border border-border/70 bg-card/70 p-5">{children}</div>
    </div>
  )
}

function Tag({ children }: { children: ReactNode }) {
  return (
    <span className="inline-flex h-5 items-center rounded-md border border-border/70 bg-muted/55 px-1.5 text-[11px] font-medium text-muted-foreground">
      {children}
    </span>
  )
}

function ProviderRow({
  provider,
  note,
  right,
  tag,
}: {
  provider: Pick<ElectronOpencodeProvider, "id" | "name">
  note?: string
  right: ReactNode
  tag?: ReactNode
}) {
  return (
    <div className="flex items-center justify-between gap-4 border-b border-border/60 py-3 last:border-0">
      <div className="min-w-0 flex flex-col">
        <div className="flex min-w-0 items-center gap-3">
          <ProviderIcon id={provider.id} className="size-5 shrink-0 text-foreground" />
          <span className="truncate text-[13px] font-medium text-foreground">{provider.name}</span>
          {tag}
        </div>
        {note ? <span className="pl-8 text-[12px] leading-5 text-muted-foreground">{note}</span> : null}
      </div>
      <div className="shrink-0">{right}</div>
    </div>
  )
}

function LoadingLine({ children }: { children: ReactNode }) {
  return (
    <div className="flex items-center gap-2 text-[13px] text-muted-foreground">
      <Loader2 className="size-3.5 animate-spin" />
      <span>{children}</span>
    </div>
  )
}

function ConnectProviderDialog({
  provider,
  methods,
  open,
  onOpenChange,
  onConnected,
  onViewAll,
}: {
  provider: ElectronOpencodeProvider | null
  methods: AuthMethod[]
  open: boolean
  onOpenChange: (open: boolean) => void
  onConnected: () => Promise<void>
  onViewAll: () => void
}) {
  const [methodIndex, setMethodIndex] = useState<number | null>(null)
  const [authorization, setAuthorization] = useState<OAuthAuthorization | null>(null)
  const [state, setState] = useState<"idle" | "pending" | "prompt" | "error">("idle")
  const [error, setError] = useState("")
  const [apiKey, setApiKey] = useState("")
  const [code, setCode] = useState("")
  const [promptValues, setPromptValues] = useState<Record<string, string>>({})
  const [promptCursor, setPromptCursor] = useState(0)

  const providerMethods = useMemo<AuthMethod[]>(() => {
    if (!provider) return []
    if (provider.id === "kilo") return methods
    return methods.length > 0 ? methods : [{ type: "api", label: "API Key" }]
  }, [methods, provider])

  const selectedMethod = methodIndex === null ? undefined : providerMethods[methodIndex]

  const resetDialog = useCallback(() => {
    setMethodIndex(null)
    setAuthorization(null)
    setState("idle")
    setError("")
    setApiKey("")
    setCode("")
    setPromptValues({})
    setPromptCursor(0)
  }, [])

  useEffect(() => {
    if (!open) return
    resetDialog()
  }, [open, provider?.id, resetDialog])

  const complete = useCallback(async () => {
    await nativeApi.invoke("opencode_global_dispose").catch(() => undefined)
    await onConnected()
    onOpenChange(false)
  }, [onConnected, onOpenChange])

  const selectMethod = useCallback(
    async (index: number, inputs?: Record<string, string>) => {
      const method = providerMethods[index]
      if (!provider || !method) return
      setMethodIndex(index)
      setAuthorization(null)
      setError("")
      setPromptCursor(0)

      if (method.type !== "oauth") {
        setState("idle")
        return
      }

      if (method.prompts?.length && !inputs) {
        setState("prompt")
        return
      }

      setState("pending")
      try {
        const result = await nativeApi.invoke("opencode_oauth_authorize", {
          providerID: provider.id,
          method: index,
          inputs,
        })
        setAuthorization(result as OAuthAuthorization)
        setState("idle")
      } catch (err) {
        setError(formatError(err))
        setState("error")
      }
    },
    [provider, providerMethods],
  )

  useEffect(() => {
    if (!open || methodIndex !== null || providerMethods.length !== 1) return
    void selectMethod(0)
  }, [methodIndex, open, providerMethods.length, selectMethod])

  useEffect(() => {
    if (!provider || !authorization || authorization.method !== "auto" || methodIndex === null) return
    let cancelled = false
    setState("pending")
    void nativeApi.invoke("opencode_oauth_callback", {
      providerID: provider.id,
      method: methodIndex,
    })
      .then(async () => {
        if (cancelled) return
        await complete()
      })
      .catch((err) => {
        if (cancelled) return
        setError(formatError(err))
        setState("error")
      })
    return () => {
      cancelled = true
    }
  }, [authorization, complete, methodIndex, provider])

  const goBack = () => {
    if (providerMethods.length === 1 && methodIndex !== null) {
      onViewAll()
      return
    }
    if (methodIndex !== null) {
      setMethodIndex(null)
      setAuthorization(null)
      setState("idle")
      setError("")
      return
    }
    onViewAll()
  }

  const saveApiKey = async (event: FormEvent) => {
    event.preventDefault()
    if (!provider || !apiKey.trim()) {
      setError("API key is required.")
      return
    }
    setState("pending")
    setError("")
    try {
      await nativeApi.invoke("opencode_auth_set", {
        providerID: provider.id,
        auth: { type: "api", key: apiKey.trim() },
      })
      await complete()
    } catch (err) {
      setError(formatError(err))
      setState("error")
    }
  }

  const submitCode = async (event: FormEvent) => {
    event.preventDefault()
    if (!provider || methodIndex === null || !code.trim()) {
      setError("Confirmation code is required.")
      return
    }
    setState("pending")
    setError("")
    try {
      await nativeApi.invoke("opencode_oauth_callback", {
        providerID: provider.id,
        method: methodIndex,
        code: code.trim(),
      })
      await complete()
    } catch (err) {
      setError(formatError(err, "Invalid confirmation code."))
      setState("error")
    }
  }

  const openAuthorizationUrl = () => {
    if (!authorization?.url) return
    void nativeApi.invoke("open_external_url", { url: authorization.url })
  }

  const prompts = selectedMethod?.type === "oauth" ? selectedMethod.prompts ?? [] : []
  const matchesPrompt = (prompt: AuthPrompt, values: Record<string, string>) => {
    if (!prompt.when) return true
    const actual = values[prompt.when.key]
    if (actual === undefined) return false
    return prompt.when.op === "eq" ? actual === prompt.when.value : actual !== prompt.when.value
  }
  const currentPrompt = prompts.find((prompt, index) => index >= promptCursor && matchesPrompt(prompt, promptValues))

  const continuePrompt = async (nextValues = promptValues) => {
    if (methodIndex === null || !currentPrompt) return
    const nextIndex = prompts.findIndex((prompt, index) => index > promptCursor && matchesPrompt(prompt, nextValues))
    if (nextIndex !== -1) {
      setPromptCursor(nextIndex)
      return
    }
    await selectMethod(methodIndex, nextValues)
  }

  const submitPrompt = async (event: FormEvent) => {
    event.preventDefault()
    if (!currentPrompt || currentPrompt.type !== "text") return
    const value = promptValues[currentPrompt.key]?.trim()
    if (!value) return
    await continuePrompt()
  }

  const confirmationCode = authorization?.instructions?.includes(":")
    ? authorization.instructions.split(":").slice(1).join(":").trim()
    : authorization?.instructions

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-[520px] gap-0 p-0" showCloseButton={false}>
        <div className="flex flex-col gap-6 px-5 pb-8 pt-4">
          <DialogTitle className="sr-only">{provider ? `Connect ${provider.name}` : "Connect provider"}</DialogTitle>
          <div className="flex items-center gap-3">
            <Button type="button" variant="ghost" size="icon-sm" onClick={goBack} tabIndex={-1}>
              <ArrowLeft className="size-4" />
            </Button>
            {provider ? (
              <>
                <ProviderIcon id={provider.id} className="size-5 shrink-0 text-foreground" />
                <div className="text-base font-medium text-foreground">Connect {provider.name}</div>
              </>
            ) : null}
          </div>

          <div className="px-1">
            {!provider ? <div className="text-sm text-muted-foreground">No provider selected.</div> : null}

            {provider && providerMethods.length === 0 ? (
              <div className="flex flex-col items-start gap-4 text-sm text-muted-foreground">
                <div>Could not load auth methods for {provider.name}.</div>
                <Button type="button" size="sm" variant="outline" onClick={() => onConnected()}>
                  Retry
                </Button>
              </div>
            ) : null}

            {provider && methodIndex === null && providerMethods.length > 0 ? (
              <div className="flex flex-col gap-4">
                <div className="text-sm text-muted-foreground">Select a method to connect {provider.name}.</div>
                <div className="overflow-hidden rounded-lg border border-border/70">
                  {providerMethods.map((method, index) => (
                    <button
                      key={`${method.type}-${method.label ?? index}`}
                      type="button"
                      onClick={() => void selectMethod(index)}
                      className="flex w-full items-center gap-2 border-b border-border/60 px-3 py-2.5 text-left text-sm last:border-0 hover:bg-muted/60"
                    >
                      <span className="flex h-2 w-4 items-center justify-center rounded-[2px] bg-input shadow-xs">
                        <span className="h-0.5 w-2.5 bg-foreground/80" />
                      </span>
                      <span>{methodLabel(method)}</span>
                    </button>
                  ))}
                </div>
              </div>
            ) : null}

            {state === "pending" && (!authorization || authorization.method !== "auto") ? (
              <LoadingLine>Connecting to provider...</LoadingLine>
            ) : null}

            {state === "error" ? (
              <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
                <CircleAlert className="size-4" />
                <span>{error}</span>
              </div>
            ) : null}

            {state === "prompt" && currentPrompt ? (
              <form className="flex flex-col items-start gap-4" onSubmit={submitPrompt}>
                {currentPrompt.type === "text" ? (
                  <>
                    <label className="w-full space-y-1.5">
                      <span className="text-sm text-muted-foreground">{currentPrompt.message}</span>
                      <Input
                        autoFocus
                        value={promptValues[currentPrompt.key] ?? ""}
                        placeholder={currentPrompt.placeholder}
                        onChange={(event) =>
                          setPromptValues((current) => ({
                            ...current,
                            [currentPrompt.key]: event.target.value,
                          }))
                        }
                      />
                    </label>
                    <Button type="submit" size="lg" disabled={!promptValues[currentPrompt.key]?.trim()}>
                      Continue
                    </Button>
                  </>
                ) : (
                  <div className="w-full space-y-2">
                    <div className="text-sm text-muted-foreground">{currentPrompt.message}</div>
                    <div className="overflow-hidden rounded-lg border border-border/70">
                      {currentPrompt.options.map((option) => (
                        <button
                          key={option.value}
                          type="button"
                          onClick={() => {
                            const next = { ...promptValues, [currentPrompt.key]: option.value }
                            setPromptValues(next)
                            void continuePrompt(next)
                          }}
                          className="flex w-full items-center gap-2 border-b border-border/60 px-3 py-2.5 text-left text-sm last:border-0 hover:bg-muted/60"
                        >
                          <span className="font-medium text-foreground">{option.label}</span>
                          {option.hint ? <span className="text-muted-foreground">{option.hint}</span> : null}
                        </button>
                      ))}
                    </div>
                  </div>
                )}
              </form>
            ) : null}

            {selectedMethod?.type === "api" && state !== "pending" ? (
              <div className="flex flex-col gap-6">
                <div className="text-sm leading-6 text-muted-foreground">
                  Enter your {provider?.name} API key. It is saved through OpenCode auth storage.
                </div>
                <form className="flex flex-col items-start gap-4" onSubmit={saveApiKey}>
                  <label className="w-full space-y-1.5">
                    <span className="text-sm text-muted-foreground">{provider?.name} API key</span>
                    <Input
                      autoFocus
                      type="password"
                      value={apiKey}
                      placeholder="sk-..."
                      onChange={(event) => setApiKey(event.target.value)}
                    />
                  </label>
                  <Button type="submit" size="lg" disabled={!apiKey.trim()}>
                    Continue
                  </Button>
                </form>
              </div>
            ) : null}

            {selectedMethod?.type === "oauth" && authorization?.method === "code" ? (
              <div className="flex flex-col gap-6">
                <div className="text-sm leading-6 text-muted-foreground">
                  Open the authorization page, then paste the confirmation code for {provider?.name}.
                </div>
                <Button type="button" variant="outline" size="sm" className="w-fit" onClick={openAuthorizationUrl}>
                  <ExternalLink className="size-3.5" />
                  Open authorization page
                </Button>
                <form className="flex flex-col items-start gap-4" onSubmit={submitCode}>
                  <label className="w-full space-y-1.5">
                    <span className="text-sm text-muted-foreground">Confirmation code</span>
                    <Input autoFocus value={code} onChange={(event) => setCode(event.target.value)} />
                  </label>
                  <Button type="submit" size="lg" disabled={!code.trim()}>
                    Continue
                  </Button>
                </form>
              </div>
            ) : null}

            {selectedMethod?.type === "oauth" && authorization?.method === "auto" ? (
              <div className="flex flex-col gap-6">
                <div className="text-sm leading-6 text-muted-foreground">
                  Open the authorization page and confirm the code for {provider?.name}.
                </div>
                <Button type="button" variant="outline" size="sm" className="w-fit" onClick={openAuthorizationUrl}>
                  <ExternalLink className="size-3.5" />
                  Open authorization page
                </Button>
                {confirmationCode ? (
                  <label className="w-full space-y-1.5">
                    <span className="text-sm text-muted-foreground">Confirmation code</span>
                    <Input className="font-mono" value={confirmationCode} readOnly />
                  </label>
                ) : null}
                <LoadingLine>Waiting for authorization...</LoadingLine>
              </div>
            ) : null}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function SelectProviderDialog({
  open,
  providers,
  connected,
  onOpenChange,
  onConnect,
}: {
  open: boolean
  providers: ElectronOpencodeProvider[]
  connected: Set<string>
  onOpenChange: (open: boolean) => void
  onConnect: (id: string) => void
}) {
  const [query, setQuery] = useState("")
  const filtered = useMemo(() => {
    const value = query.trim().toLowerCase()
    if (!value) return providers
    return providers.filter((provider) => `${provider.name} ${provider.id}`.toLowerCase().includes(value))
  }, [providers, query])

  useEffect(() => {
    if (open) setQuery("")
  }, [open])

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-[560px] p-0" showCloseButton>
        <div className="flex flex-col gap-4 p-5">
          <DialogTitle>Connect Provider</DialogTitle>
          <div className="relative">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search providers..."
              className="pl-8"
            />
          </div>
          <div className="thin-scrollbar max-h-[420px] overflow-y-auto rounded-xl border border-border/70">
            {filtered.map((provider) => (
              <ProviderRow
                key={provider.id}
                provider={provider}
                tag={connected.has(provider.id) ? <Tag>Connected</Tag> : undefined}
                right={
                  <Button
                    type="button"
                    size="sm"
                    variant="secondary"
                    onClick={() => {
                      onOpenChange(false)
                      onConnect(provider.id)
                    }}
                  >
                    <Plus className="size-3.5" />
                    Connect
                  </Button>
                }
              />
            ))}
            {filtered.length === 0 ? (
              <div className="px-3 py-8 text-center text-sm text-muted-foreground">No providers found.</div>
            ) : null}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function CustomOpenAICompatibleDialog({
  open,
  onOpenChange,
  onSaved,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSaved: () => Promise<void>
}) {
  const [providerID, setProviderID] = useState("")
  const [name, setName] = useState("")
  const [baseURL, setBaseURL] = useState("")
  const [apiKey, setApiKey] = useState("")
  const [models, setModels] = useState<Array<{ id: string; name: string; selected: boolean }>>([])
  const [error, setError] = useState("")
  const [fetching, setFetching] = useState(false)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    if (!open) return
    setProviderID("")
    setName("")
    setBaseURL("")
    setApiKey("")
    setModels([])
    setError("")
  }, [open])

  const validateBase = () => {
    if (!providerID.trim()) return "Provider ID is required."
    if (!PROVIDER_ID.test(providerID.trim())) return "Provider ID can only use lowercase letters, numbers, hyphens, and underscores."
    if (!name.trim()) return "Provider name is required."
    if (!/^https?:\/\//.test(baseURL.trim())) return "Base URL must start with http:// or https://."
    if (!apiKey.trim()) return "API key is required."
    return ""
  }

  const fetchModels = async () => {
    const issue = validateBase()
    if (issue) {
      setError(issue)
      return
    }
    setFetching(true)
    setError("")
    try {
      const url = `${baseURL.trim().replace(/\/$/, "")}/v1/models`
      const response = await fetch(url, {
        headers: {
          Authorization: `Bearer ${apiKey.trim()}`,
          "Content-Type": "application/json",
        },
      })
      if (!response.ok) throw new Error(`Failed to fetch models: ${response.status} ${response.statusText}`)
      const data = await response.json() as { data?: Array<{ id?: string; name?: string }> }
      if (!Array.isArray(data.data)) throw new Error("Invalid /v1/models response.")
      setModels(data.data.filter((item) => item.id).map((item) => ({
        id: item.id!,
        name: item.name || item.id!,
        selected: true,
      })))
    } catch (err) {
      setError(formatError(err, "Failed to fetch models."))
    } finally {
      setFetching(false)
    }
  }

  const save = async (event: FormEvent) => {
    event.preventDefault()
    const issue = validateBase()
    if (issue) {
      setError(issue)
      return
    }
    const selectedModels = models.filter((model) => model.selected)
    if (selectedModels.length === 0) {
      setError("Fetch and select at least one model first.")
      return
    }
    setSaving(true)
    setError("")
    try {
      const id = providerID.trim()
      await nativeApi.invoke("opencode_auth_set", {
        providerID: id,
        auth: { type: "api", key: apiKey.trim() },
      })
      await nativeApi.invoke("opencode_global_config_update", {
        config: {
          provider: {
            [id]: {
              npm: OPENAI_COMPATIBLE_NPM,
              name: name.trim(),
              options: { baseURL: baseURL.trim() },
              models: Object.fromEntries(selectedModels.map((model) => [model.id, { name: model.name }])),
            } satisfies ProviderConfig,
          },
        },
      })
      await nativeApi.invoke("opencode_global_dispose").catch(() => undefined)
      await onSaved()
      onOpenChange(false)
    } catch (err) {
      setError(formatError(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[84vh] max-w-[560px] overflow-y-auto p-0" showCloseButton={false}>
        <form className="flex flex-col gap-5 p-5" onSubmit={save}>
          <div className="flex items-center gap-3">
            <Button type="button" variant="ghost" size="icon-sm" onClick={() => onOpenChange(false)} tabIndex={-1}>
              <ArrowLeft className="size-4" />
            </Button>
            <ProviderIcon id="openai" className="size-5 shrink-0 text-foreground" />
            <DialogTitle>OpenAI Compatible</DialogTitle>
          </div>

          <p className="text-sm leading-6 text-muted-foreground">
            Add any OpenAI-compatible API by fetching `/v1/models`, then saving the selected models into OpenCode config.
          </p>

          <div className="grid gap-3">
            <Input value={providerID} onChange={(event) => setProviderID(event.target.value)} placeholder="provider-id" />
            <Input value={name} onChange={(event) => setName(event.target.value)} placeholder="Provider name" />
            <Input value={baseURL} onChange={(event) => setBaseURL(event.target.value)} placeholder="https://api.example.com" />
            <Input type="password" value={apiKey} onChange={(event) => setApiKey(event.target.value)} placeholder="API key" />
          </div>

          {error ? (
            <div className="rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              {error}
            </div>
          ) : null}

          <Button type="button" variant="outline" size="lg" disabled={fetching || saving} onClick={() => void fetchModels()}>
            {fetching ? <Loader2 className="size-3.5 animate-spin" /> : null}
            {fetching ? "Fetching models..." : "Fetch Models"}
          </Button>

          {models.length > 0 ? (
            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <div className="text-[12px] font-medium text-muted-foreground">Models</div>
                <div className="flex gap-1">
                  <Button type="button" variant="ghost" size="xs" onClick={() => setModels((items) => items.map((item) => ({ ...item, selected: true })))}>
                    Select all
                  </Button>
                  <Button type="button" variant="ghost" size="xs" onClick={() => setModels((items) => items.map((item) => ({ ...item, selected: false })))}>
                    Deselect
                  </Button>
                </div>
              </div>
              <div className="thin-scrollbar max-h-52 overflow-y-auto rounded-lg border border-border/70">
                {models.map((model) => (
                  <label key={model.id} className="flex cursor-pointer items-center gap-3 border-b border-border/60 px-3 py-2 last:border-0 hover:bg-muted/50">
                    <input
                      type="checkbox"
                      checked={model.selected}
                      onChange={(event) =>
                        setModels((items) =>
                          items.map((item) => item.id === model.id ? { ...item, selected: event.target.checked } : item),
                        )
                      }
                    />
                    <span className="min-w-0 flex flex-col">
                      <span className="truncate text-[13px] font-medium text-foreground">{model.name}</span>
                      <span className="truncate text-[12px] text-muted-foreground">{model.id}</span>
                    </span>
                  </label>
                ))}
              </div>
            </div>
          ) : null}

          <Button type="submit" size="lg" className="w-fit" disabled={saving || fetching}>
            {saving ? <Loader2 className="size-3.5 animate-spin" /> : null}
            {saving ? "Saving..." : "Save Provider"}
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function CustomProviderDialog({
  open,
  onOpenChange,
  onSaved,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSaved: () => Promise<void>
}) {
  const [providerID, setProviderID] = useState("")
  const [name, setName] = useState("")
  const [baseURL, setBaseURL] = useState("")
  const [apiKey, setApiKey] = useState("")
  const [models, setModels] = useState([{ id: "", name: "" }])
  const [error, setError] = useState("")
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    if (!open) return
    setProviderID("")
    setName("")
    setBaseURL("")
    setApiKey("")
    setModels([{ id: "", name: "" }])
    setError("")
  }, [open])

  const save = async (event: FormEvent) => {
    event.preventDefault()
    const id = providerID.trim()
    const validModels = models.map((model) => ({ id: model.id.trim(), name: model.name.trim() })).filter((model) => model.id)
    if (!id) return setError("Provider ID is required.")
    if (!PROVIDER_ID.test(id)) return setError("Provider ID can only use lowercase letters, numbers, hyphens, and underscores.")
    if (!name.trim()) return setError("Provider name is required.")
    if (!/^https?:\/\//.test(baseURL.trim())) return setError("Base URL must start with http:// or https://.")
    if (validModels.length === 0) return setError("Add at least one model.")

    setSaving(true)
    setError("")
    try {
      if (apiKey.trim()) {
        await nativeApi.invoke("opencode_auth_set", {
          providerID: id,
          auth: { type: "api", key: apiKey.trim() },
        })
      }
      await nativeApi.invoke("opencode_global_config_update", {
        config: {
          provider: {
            [id]: {
              npm: OPENAI_COMPATIBLE_NPM,
              name: name.trim(),
              options: { baseURL: baseURL.trim() },
              models: Object.fromEntries(validModels.map((model) => [model.id, { name: model.name || model.id }])),
            } satisfies ProviderConfig,
          },
        },
      })
      await nativeApi.invoke("opencode_global_dispose").catch(() => undefined)
      await onSaved()
      onOpenChange(false)
    } catch (err) {
      setError(formatError(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[84vh] max-w-[560px] overflow-y-auto p-0" showCloseButton={false}>
        <form className="flex flex-col gap-5 p-5" onSubmit={save}>
          <div className="flex items-center gap-3">
            <Button type="button" variant="ghost" size="icon-sm" onClick={() => onOpenChange(false)} tabIndex={-1}>
              <ArrowLeft className="size-4" />
            </Button>
            <ProviderIcon id="synthetic" className="size-5 shrink-0 text-foreground" />
            <DialogTitle>Custom Provider</DialogTitle>
          </div>

          <p className="text-sm leading-6 text-muted-foreground">
            Add a custom OpenCode provider by defining its base URL and model IDs.
          </p>

          <div className="grid gap-3">
            <Input value={providerID} onChange={(event) => setProviderID(event.target.value)} placeholder="provider-id" />
            <Input value={name} onChange={(event) => setName(event.target.value)} placeholder="Provider name" />
            <Input value={baseURL} onChange={(event) => setBaseURL(event.target.value)} placeholder="https://api.example.com/v1" />
            <Input type="password" value={apiKey} onChange={(event) => setApiKey(event.target.value)} placeholder="API key (optional)" />
          </div>

          <div className="space-y-2">
            <div className="text-[12px] font-medium text-muted-foreground">Models</div>
            <div className="space-y-2">
              {models.map((model, index) => (
                <div key={index} className="flex gap-2">
                  <Input
                    value={model.id}
                    onChange={(event) =>
                      setModels((items) => items.map((item, itemIndex) => itemIndex === index ? { ...item, id: event.target.value } : item))
                    }
                    placeholder="model-id"
                  />
                  <Input
                    value={model.name}
                    onChange={(event) =>
                      setModels((items) => items.map((item, itemIndex) => itemIndex === index ? { ...item, name: event.target.value } : item))
                    }
                    placeholder="Display name"
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon-sm"
                    disabled={models.length <= 1}
                    onClick={() => setModels((items) => items.filter((_, itemIndex) => itemIndex !== index))}
                  >
                    <Trash2 className="size-3.5" />
                  </Button>
                </div>
              ))}
            </div>
            <Button type="button" variant="ghost" size="sm" className="w-fit" onClick={() => setModels((items) => [...items, { id: "", name: "" }])}>
              <Plus className="size-3.5" />
              Add model
            </Button>
          </div>

          {error ? (
            <div className="rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              {error}
            </div>
          ) : null}

          <Button type="submit" size="lg" className="w-fit" disabled={saving}>
            {saving ? <Loader2 className="size-3.5 animate-spin" /> : null}
            {saving ? "Saving..." : "Save Provider"}
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export function OpenCodeProvidersSettings() {
  const [serverStatus, setServerStatus] = useState<ElectronOpencodeServerStatus | null>(null)
  const [providerList, setProviderList] = useState<ElectronOpencodeProviderList | null>(null)
  const [authMethods, setAuthMethods] = useState<Record<string, AuthMethod[]>>({})
  const [loading, setLoading] = useState(false)
  const [message, setMessage] = useState("")
  const [connectProviderId, setConnectProviderId] = useState<string | null>(null)
  const [selectOpen, setSelectOpen] = useState(false)
  const [openAICompatibleOpen, setOpenAICompatibleOpen] = useState(false)
  const [customProviderOpen, setCustomProviderOpen] = useState(false)

  const refresh = useCallback(async () => {
    setLoading(true)
    setMessage("")
    try {
      const status = await nativeApi.invoke("opencode_server_start", {})
      const [providers, methods] = await Promise.all([
        nativeApi.invoke("opencode_provider_list", {}),
        nativeApi.invoke("opencode_provider_auth_methods", {}),
      ])
      setServerStatus(status)
      setProviderList(providers)
      setAuthMethods(methods as Record<string, AuthMethod[]>)
    } catch (err) {
      setMessage(formatError(err, "Failed to load OpenCode providers."))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const connectedIds = useMemo(() => new Set(providerList?.connected ?? []), [providerList])
  const allProviders = useMemo(
    () => [...(providerList?.all ?? [])].sort((left, right) => left.name.localeCompare(right.name)),
    [providerList],
  )
  const connectedProviders = useMemo(
    () => allProviders.filter((provider) => connectedIds.has(provider.id)).filter(connectedModelFilter),
    [allProviders, connectedIds],
  )
  const popularProviders = useMemo(() => {
    const connected = new Set(connectedProviders.map((provider) => provider.id))
    return allProviders
      .filter((provider) => POPULAR_PROVIDER_IDS.includes(provider.id) && !connected.has(provider.id))
      .sort((left, right) => POPULAR_PROVIDER_IDS.indexOf(left.id) - POPULAR_PROVIDER_IDS.indexOf(right.id))
  }, [allProviders, connectedProviders])
  const selectedProvider = useMemo(
    () => allProviders.find((provider) => provider.id === connectProviderId) ?? null,
    [allProviders, connectProviderId],
  )

  const connect = (providerID: string) => {
    setConnectProviderId(providerID)
  }

  const disconnect = async (provider: ElectronOpencodeProvider) => {
    if (provider.source === "env") {
      setMessage(`${provider.name} is connected through environment variables, so OpenCode cannot remove it here.`)
      return
    }

    setLoading(true)
    setMessage("")
    try {
      await nativeApi.invoke("opencode_auth_remove", { providerID: provider.id }).catch(() => undefined)
      if (provider.source === "custom" || provider.source === "config") {
        const config = await nativeApi.invoke("opencode_global_config_get")
        const disabled = Array.isArray(config.disabled_providers) ? config.disabled_providers.filter((id): id is string => typeof id === "string") : []
        await nativeApi.invoke("opencode_global_config_update", {
          config: {
            disabled_providers: disabled.includes(provider.id) ? disabled : [...disabled, provider.id],
          },
        })
      }
      await nativeApi.invoke("opencode_global_dispose").catch(() => undefined)
      await refresh()
      setMessage(`${provider.name} disconnected.`)
    } catch (err) {
      setMessage(formatError(err, "Failed to disconnect provider."))
    } finally {
      setLoading(false)
    }
  }

  const openConnectFromAll = (id: string) => {
    setSelectOpen(false)
    connect(id)
  }

  return (
    <div className="thin-scrollbar flex h-full flex-col overflow-y-auto px-2 py-2">
      <div className="flex max-w-2xl flex-col gap-5">
        <div className="flex items-center justify-between gap-3 px-1">
          <div className="flex items-center gap-2 text-[12px] text-muted-foreground">
            <span
              className={`size-2 rounded-full ${
                serverStatus?.healthy ? "bg-success" : loading ? "bg-warning" : "bg-muted-foreground/45"
              }`}
            />
            <span>{serverStatus?.healthy ? "server connected" : loading ? "Starting server" : "server not ready"}</span>
          </div>
          <Button type="button" variant="ghost" size="sm" onClick={() => void refresh()} disabled={loading}>
            {loading ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
            Refresh
          </Button>
        </div>

        <Section title="Connected">
          {loading && !providerList ? <LoadingLine>Loading providers...</LoadingLine> : null}
          {!loading && connectedProviders.length === 0 ? (
            <div className="py-4 text-center text-sm text-muted-foreground">No connected providers yet.</div>
          ) : null}
          {connectedProviders.map((provider) => (
            <ProviderRow
              key={provider.id}
              provider={provider}
              tag={<Tag>{sourceLabel(provider.source)}</Tag>}
              right={
                provider.source === "env" ? (
                  <span className="pr-3 text-[12px] text-muted-foreground">Managed by env</span>
                ) : (
                  <Button type="button" variant="ghost" size="sm" onClick={() => void disconnect(provider)} disabled={loading}>
                    Disconnect
                  </Button>
                )
              }
            />
          ))}
        </Section>

        <Section title="Popular">
          {popularProviders.map((provider) => {
            const note = PROVIDER_NOTES.find((item) => item.match(provider.id))?.note
            const recommended = provider.id === "opencode" || provider.id === "opencode-go"
            return (
              <ProviderRow
                key={provider.id}
                provider={provider}
                note={note}
                tag={recommended ? <Tag>Recommended</Tag> : undefined}
                right={
                  <Button type="button" size="sm" variant="secondary" onClick={() => connect(provider.id)} disabled={loading}>
                    <Plus className="size-3.5" />
                    Connect
                  </Button>
                }
              />
            )
          })}

          <ProviderRow
            provider={{ id: "openai", name: "OpenAI Compatible" }}
            note="Connect any OpenAI-compatible endpoint and fetch its models."
            tag={<Tag>Custom</Tag>}
            right={
              <Button type="button" size="sm" variant="secondary" onClick={() => setOpenAICompatibleOpen(true)} disabled={loading}>
                <Plus className="size-3.5" />
                Connect
              </Button>
            }
          />

          <ProviderRow
            provider={{ id: "synthetic", name: "Custom Provider" }}
            note="Define a custom provider and model list in config."
            tag={<Tag>Custom</Tag>}
            right={
              <Button type="button" size="sm" variant="secondary" onClick={() => setCustomProviderOpen(true)} disabled={loading}>
                <Plus className="size-3.5" />
                Connect
              </Button>
            }
          />
        </Section>

        <Button
          type="button"
          variant="ghost"
          className="w-fit px-0 text-[13px] font-medium text-primary hover:bg-transparent"
          onClick={() => setSelectOpen(true)}
          disabled={loading || allProviders.length === 0}
        >
          View all providers
        </Button>

        {message ? (
          <div className="rounded-xl border border-border/70 bg-card/70 px-3 py-2 text-[12px] text-muted-foreground">
            {message}
          </div>
        ) : null}
      </div>

      <ConnectProviderDialog
        provider={selectedProvider}
        methods={selectedProvider ? authMethods[selectedProvider.id] ?? [] : []}
        open={!!connectProviderId}
        onOpenChange={(open) => {
          if (!open) setConnectProviderId(null)
        }}
        onConnected={refresh}
        onViewAll={() => {
          setConnectProviderId(null)
          setSelectOpen(true)
        }}
      />

      <SelectProviderDialog
        open={selectOpen}
        providers={allProviders}
        connected={connectedIds}
        onOpenChange={setSelectOpen}
        onConnect={openConnectFromAll}
      />

      <CustomOpenAICompatibleDialog
        open={openAICompatibleOpen}
        onOpenChange={setOpenAICompatibleOpen}
        onSaved={refresh}
      />

      <CustomProviderDialog
        open={customProviderOpen}
        onOpenChange={setCustomProviderOpen}
        onSaved={refresh}
      />

    </div>
  )
}
