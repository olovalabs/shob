import spriteUrl from "@/assets/provider-icons/sprite.svg"
import { iconNames, type IconName } from "@/assets/provider-icons/types"
import type { SVGProps } from "react"

const iconNameSet = new Set<string>(iconNames)

export function ProviderIcon({
  id,
  className,
  ...props
}: SVGProps<SVGSVGElement> & { id: string }) {
  const resolved = id === "antigravity" ? "google" : iconNameSet.has(id) ? id : ("synthetic" satisfies IconName)

  return (
    <svg data-component="provider-icon" className={className} aria-hidden="true" {...props}>
      <use href={`${spriteUrl}#${resolved}`} />
    </svg>
  )
}
