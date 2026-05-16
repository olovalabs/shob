import { createMemo, createSignal, For, Show } from "solid-js"
import { Boxes, SlidersHorizontal, Terminal } from "lucide-solid"
import { useStore } from "../store"
import { CliAvatar } from "./CliAvatar"
import { Button } from "@/components/ui/button"

type SettingsSection = "general" | "providers" | "cli-tools"

const getShellLabel = (shell: string) => shell.split(/[\\/]/).pop() || shell

export function SettingsPage() {
  const preferredCliId = useStore((s) => s.preferredCliId)
  const preferredShell = useStore((s) => s.preferredShell)
  const cliLaunchMode = useStore((s) => s.cliLaunchMode)
  const cliTools = useStore((s) => s.cliTools)
  const availableShells = useStore((s) => s.availableShells)
  const setPreferredCliTool = useStore((s) => s.setPreferredCliTool)
  const setPreferredShell = useStore((s) => s.setPreferredShell)
  const setCliLaunchMode = useStore((s) => s.setCliLaunchMode)
  const installCliTool = useStore((s) => s.installCliTool)
  const [section, setSection] = createSignal<SettingsSection>("general")

  const installedCliTools = createMemo(() => cliTools().filter((tool) => tool.installed))

  return (
    <div class="min-h-0 flex-1 bg-[#1e1e1e] text-[#cccccc]">
      <div class="flex h-full">
        <aside class="w-[220px] border-r border-[#2d2d2d] p-3">
          <div class="grid gap-1">
            <Button type="button" variant={section() === "general" ? "secondary" : "ghost"} class="justify-start" onClick={() => setSection("general")}>
              <SlidersHorizontal class="mr-2 h-4 w-4" /> General
            </Button>
            <Button type="button" variant={section() === "providers" ? "secondary" : "ghost"} class="justify-start" onClick={() => setSection("providers")}>
              <Boxes class="mr-2 h-4 w-4" /> Providers
            </Button>
            <Button type="button" variant={section() === "cli-tools" ? "secondary" : "ghost"} class="justify-start" onClick={() => setSection("cli-tools")}>
              <Terminal class="mr-2 h-4 w-4" /> CLI Tools
            </Button>
          </div>
        </aside>

        <div class="custom-scrollbar min-h-0 flex-1 overflow-y-auto p-6">
          <Show when={section() === "general"}>
            <div class="space-y-6">
              <h2 class="text-lg font-semibold">General</h2>

              <div class="rounded-lg border border-[#2d2d2d] bg-[#252526] p-4">
                <label class="mb-2 block text-sm text-[#bdbdbd]">Default CLI</label>
                <div class="flex items-center gap-2">
                  <span class="inline-flex h-8 w-8 items-center justify-center rounded-md bg-[#2f2f32]">
                    <CliAvatar cliId={preferredCliId() ?? installedCliTools()[0]?.id ?? null} label="Default CLI" size="sm" />
                  </span>
                  <select
                    value={preferredCliId() ?? installedCliTools()[0]?.id ?? ""}
                    onChange={(event) => setPreferredCliTool(event.currentTarget.value || null)}
                    class="h-10 w-full rounded-md border border-[#3a3a3a] bg-[#1f1f20] px-3 text-sm"
                  >
                    <Show when={installedCliTools().length > 0} fallback={<option value="" disabled>No CLI tools detected</option>}>
                      <For each={installedCliTools()}>{(tool) => <option value={tool.id}>{tool.label}</option>}</For>
                    </Show>
                  </select>
                </div>
              </div>

              <div class="rounded-lg border border-[#2d2d2d] bg-[#252526] p-4">
                <label class="mb-2 block text-sm text-[#bdbdbd]">Default Shell</label>
                <select
                  value={preferredShell() ?? availableShells()[0] ?? ""}
                  onChange={(event) => setPreferredShell(event.currentTarget.value || null)}
                  class="h-10 w-full rounded-md border border-[#3a3a3a] bg-[#1f1f20] px-3 text-sm"
                >
                  <Show when={availableShells().length > 0} fallback={<option value="" disabled>No shells detected</option>}>
                    <For each={availableShells()}>{(shell) => <option value={shell}>{getShellLabel(shell)}</option>}</For>
                  </Show>
                </select>
              </div>
            </div>
          </Show>

          <Show when={section() === "providers"}>
            <div class="space-y-6">
              <h2 class="text-lg font-semibold">Providers</h2>
              <div class="rounded-lg border border-[#2d2d2d] bg-[#252526] p-4">
                <label class="mb-2 block text-sm text-[#bdbdbd]">Provider Switch Mode</label>
                <select
                  value={cliLaunchMode()}
                  onChange={(event) => setCliLaunchMode(event.currentTarget.value === "replace-current" ? "replace-current" : "new-tab")}
                  class="h-10 w-full rounded-md border border-[#3a3a3a] bg-[#1f1f20] px-3 text-sm"
                >
                  <option value="new-tab">Open in new tab</option>
                  <option value="replace-current">Replace current tab</option>
                </select>
              </div>
            </div>
          </Show>

          <Show when={section() === "cli-tools"}>
            <div class="space-y-4">
              <h2 class="text-lg font-semibold">CLI Tools</h2>
              <div class="space-y-2">
                <For each={cliTools()}>
                  {(tool) => (
                    <div class="flex items-center justify-between rounded-md border border-[#2d2d2d] bg-[#252526] px-3 py-2">
                      <div class="flex items-center gap-2">
                        <CliAvatar cliId={tool.id} label={tool.label} size="sm" />
                        <span class="text-sm">{tool.label}</span>
                      </div>
                      <Show
                        when={tool.installed}
                        fallback={
                          <Button size="sm" variant="outline" onClick={() => void installCliTool(tool.id, tool.installCommand)}>
                            Install
                          </Button>
                        }
                      >
                        <span class="text-xs text-[#8dcf94]">Installed</span>
                      </Show>
                    </div>
                  )}
                </For>
              </div>
            </div>
          </Show>
        </div>
      </div>
    </div>
  )
}
