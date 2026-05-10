import { Slot } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"
import { Icon, IconProps } from "./icon"

export interface IconButtonProps extends React.ComponentProps<"button"> {
  icon: IconProps["name"]
  size?: "small" | "normal" | "large"
  iconSize?: IconProps["size"]
  variant?: "primary" | "secondary" | "ghost"
  asChild?: boolean
}

function IconButton({ className, variant = "secondary", size = "normal", iconSize, icon, asChild, ...props }: IconButtonProps) {
  const Comp = asChild ? Slot.Root : "button"
  return (
    <Comp
      data-component="icon-button"
      data-icon={icon}
      data-size={size}
      data-variant={variant}
      className={cn(className)}
      {...props}
    >
      <Icon name={icon} size={iconSize ?? (size === "large" ? "normal" : "small")} />
    </Comp>
  )
}

export { IconButton }
