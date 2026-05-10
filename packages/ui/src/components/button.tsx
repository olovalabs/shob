import { Slot } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"
import { Icon, IconProps } from "./icon"

export interface ButtonProps extends React.ComponentProps<"button"> {
  size?: "small" | "normal" | "large"
  variant?: "primary" | "secondary" | "ghost"
  icon?: IconProps["name"]
  asChild?: boolean
}

function Button({ className, variant = "secondary", size = "normal", icon, asChild, children, ...props }: ButtonProps) {
  const Comp = asChild ? Slot.Root : "button"
  return (
    <Comp
      data-component="button"
      data-size={size}
      data-variant={variant}
      data-icon={icon}
      className={cn(className)}
      {...props}
    >
      {icon && <Icon name={icon} size="small" />}
      {children}
    </Comp>
  )
}

export { Button }
