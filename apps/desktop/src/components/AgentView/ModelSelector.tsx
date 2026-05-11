import { useState, useMemo } from "react"
import { ChevronDown, Search, CircleX } from "lucide-react"
import { cn } from "@/lib/utils"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { Switch } from "@/components/ui/switch"
import { ProviderIcon } from "@/components/shob/ProviderIcon"
import { useStore } from "@/store"
import type { OpenCodeModelOption } from "@/utils/opencode-models"

interface ModelSelectorProps {
  selectedModel: string
  modelOptions: OpenCodeModelOption[]
  providerStatus: "idle" | "loading" | "ready" | "error"
  onSelect: (value: string) => void
  className?: string
}

export function ModelSelector({ selectedModel, modelOptions, providerStatus, onSelect, className }: ModelSelectorProps) {
  const [open, setOpen] = useState(false)
  const [search, setSearch] = useState("")
  const visibleModels = useStore((s) => s.visibleOpencodeModels)
  const toggleVisible = useStore((s) => s.toggleVisibleOpencodeModel)

  const currentOption = modelOptions.find((o) => o.value === selectedModel)

  const isVisible = (option: OpenCodeModelOption) => {
    if (visibleModels.length === 0) return true
    return visibleModels.includes(`${option.providerID}:${option.modelID}`)
  }

  const filtered = useMemo(() => {
    if (!search.trim()) return modelOptions
    const q = search.trim().toLowerCase()
    return modelOptions.filter(
      (o) =>
        o.label.toLowerCase().includes(q) ||
        o.shortLabel.toLowerCase().includes(q) ||
        o.modelID.toLowerCase().includes(q) ||
        o.providerName.toLowerCase().includes(q),
    )
  }, [modelOptions, search])

  const grouped = useMemo(() => {
    const map = new Map<string, OpenCodeModelOption[]>()
    for (const opt of filtered) {
      const existing = map.get(opt.providerID) ?? []
      existing.push(opt)
      map.set(opt.providerID, existing)
    }
    return Array.from(map.entries())
  }, [filtered])

  const label = currentOption?.shortLabel ?? (providerStatus === "loading" ? "Loading..." : "Select model")

  const handleSelect = (option: OpenCodeModelOption) => {
    onSelect(option.value)
    setOpen(false)
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          disabled={providerStatus === "loading" || modelOptions.length === 0}
          className={cn(
            "h-7 max-w-[140px] flex items-center gap-1 rounded-md border border-border/70 bg-card/90 pl-2 pr-1.5 text-[11px] text-foreground shadow-xs outline-none transition-colors hover:bg-accent/55 disabled:cursor-not-allowed disabled:opacity-50",
            className,
          )}
          title={currentOption?.label ?? "Connected model"}
        >
          <span className="truncate">{label}</span>
          <ChevronDown className="size-3 shrink-0 text-muted-foreground" />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" sideOffset={4} className="w-72 p-2">
        <div className="flex items-center gap-1.5 px-2 h-8 rounded-md bg-muted/60 mb-1">
          <Search className="size-3.5 text-muted-foreground shrink-0" />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search models..."
            spellCheck={false}
            autoCorrect="off"
            autoComplete="off"
            className="flex-1 bg-transparent text-xs text-foreground outline-none placeholder:text-muted-foreground"
          />
          {search ? (
            <button
              type="button"
              onClick={() => setSearch("")}
              className="size-5 flex items-center justify-center rounded text-muted-foreground hover:text-foreground hover:bg-accent transition-colors"
            >
              <CircleX className="size-3" />
            </button>
          ) : null}
        </div>

        <div className="max-h-64 overflow-y-auto thin-scrollbar">
          {grouped.length === 0 ? (
            <div className="py-8 text-center text-xs text-muted-foreground">No models found</div>
          ) : (
            grouped.map(([providerID, options]) => (
              <div key={providerID} className="mb-2 last:mb-0">
                <div className="flex items-center gap-1.5 px-2 py-1.5">
                  <ProviderIcon id={providerID} className="size-4 shrink-0 text-foreground" />
                  <span className="text-[11px] font-medium text-foreground">{options[0].providerName}</span>
                </div>
                {options.map((option) => {
                  const visible = isVisible(option)
                  return (
                    <div
                      key={option.value}
                      className="flex items-center justify-between gap-2 px-2 py-1.5 rounded-md hover:bg-accent/50 cursor-pointer transition-colors"
                      onClick={() => handleSelect(option)}
                    >
                      <span className={`text-xs truncate ${visible ? "text-foreground" : "text-muted-foreground"}`}>
                        {option.shortLabel}
                      </span>
                      <div onClick={(e) => e.stopPropagation()}>
                        <Switch
                          checked={visible}
                          onCheckedChange={() => toggleVisible(option.providerID, option.modelID)}
                        />
                      </div>
                    </div>
                  )
                })}
              </div>
            ))
          )}
        </div>
      </PopoverContent>
    </Popover>
  )
}
