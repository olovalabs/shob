import * as React from "react"
import { cva, type VariantProps } from "class-variance-authority"
import { Slot } from "radix-ui"

import { cn } from "@/lib/utils"

const buttonVariants = cva(
  "group/button inline-flex shrink-0 items-center justify-center whitespace-nowrap rounded-[10px] border text-sm font-medium transition-colors outline-none select-none disabled:pointer-events-none [&_svg]:pointer-events-none [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        default:
          "border-[var(--border)] bg-[var(--primary)] text-[var(--primary-foreground)] hover:brightness-110 active:brightness-95",
        outline:
          "border-[var(--border)] bg-[var(--card)] text-[var(--foreground)] hover:bg-[var(--muted)] active:bg-[var(--accent)]",
        secondary:
          "border-transparent bg-[var(--secondary)] text-[var(--foreground)] hover:brightness-110 active:brightness-95",
        ghost:
          "border-transparent bg-transparent text-[var(--foreground)] hover:bg-[var(--muted)] active:bg-[var(--accent)]",
        destructive: "border-[var(--destructive)] bg-[color-mix(in_oklch,var(--destructive)_14%,transparent)] text-[var(--destructive)] hover:bg-[color-mix(in_oklch,var(--destructive)_22%,transparent)]",
        link: "text-primary underline-offset-4 hover:underline",
      },
      size: {
        default:
          "h-7 gap-2 px-2 text-[12px] leading-[1.2] has-data-[icon=inline-end]:pr-2 has-data-[icon=inline-start]:pl-2 [&_svg:not([class*='size-'])]:size-3.5",
        xs: "h-6 gap-1 px-2 text-[11px] has-data-[icon=inline-end]:pr-1.5 has-data-[icon=inline-start]:pl-1.5 [&_svg:not([class*='size-'])]:size-3",
        sm: "h-6.5 gap-1.5 px-2.5 text-[12px] has-data-[icon=inline-end]:pr-1.5 has-data-[icon=inline-start]:pl-1.5 [&_svg:not([class*='size-'])]:size-3.5",
        lg: "h-9 gap-1.5 px-2.5 has-data-[icon=inline-end]:pr-3 has-data-[icon=inline-start]:pl-3",
        icon: "size-8",
        "icon-xs":
          "size-6 rounded-full border-[var(--border)] bg-[var(--card)] [&_svg:not([class*='size-'])]:size-3",
        "icon-sm": "size-7 rounded-full [&_svg:not([class*='size-'])]:size-3.5",
        "icon-lg": "size-9",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  }
)

function Button({
  className,
  variant = "default",
  size = "default",
  asChild = false,
  ...props
}: React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & {
    asChild?: boolean
  }) {
  const Comp = asChild ? Slot.Root : "button"

  return (
    <Comp
      data-component="button"
      data-slot="button"
      data-variant={variant}
      data-size={size}
      className={cn(buttonVariants({ variant, size, className }))}
      {...props}
    />
  )
}

export { Button, buttonVariants }
