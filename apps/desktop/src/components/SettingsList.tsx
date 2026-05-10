import type { ReactNode } from "react"

export function SettingsList({ children }: { children: ReactNode }) {
  return <div className="bg-[var(--surface-base)] px-4 rounded-lg">{children}</div>
}
