import * as React from "react"

import { cn } from "@/lib/utils"

function Input({ className, type, ...props }: React.ComponentProps<"input">) {
  return (
    <input
      type={type}
      data-component="input"
      data-slot="input"
      className={cn(
        "h-8 w-full min-w-0 rounded-[10px] border border-[var(--border)] bg-[var(--card)] px-2.5 py-1 text-sm text-[var(--foreground)] transition-colors outline-none",
        "placeholder:text-[var(--muted-foreground)] focus-visible:border-[var(--ring)] focus-visible:ring-2 focus-visible:ring-[color-mix(in_oklch,var(--ring)_35%,transparent)]",
        "disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50",
        "file:inline-flex file:h-6 file:border-0 file:bg-transparent file:text-sm file:font-medium file:text-[var(--foreground)]",
        className
      )}
      {...props}
    />
  )
}

export { Input }
