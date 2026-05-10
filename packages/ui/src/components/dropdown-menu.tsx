import { DropdownMenu as DropdownMenuPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export interface DropdownMenuProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Root> {}
export interface DropdownMenuTriggerProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Trigger> {}
export interface DropdownMenuIconProps extends React.ComponentProps<"span"> {}
export interface DropdownMenuPortalProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Portal> {}
export interface DropdownMenuContentProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Content> {}
export interface DropdownMenuArrowProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Arrow> {}
export interface DropdownMenuSeparatorProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Separator> {}
export interface DropdownMenuGroupProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Group> {}
export interface DropdownMenuGroupLabelProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Label> {}
export interface DropdownMenuItemProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Item> {}
export interface DropdownMenuItemLabelProps extends React.ComponentProps<"span"> {}
export interface DropdownMenuItemDescriptionProps extends React.ComponentProps<"span"> {}
export interface DropdownMenuItemIndicatorProps extends React.ComponentProps<typeof DropdownMenuPrimitive.ItemIndicator> {}
export interface DropdownMenuRadioGroupProps extends React.ComponentProps<typeof DropdownMenuPrimitive.RadioGroup> {}
export interface DropdownMenuRadioItemProps extends React.ComponentProps<typeof DropdownMenuPrimitive.RadioItem> {}
export interface DropdownMenuCheckboxItemProps extends React.ComponentProps<typeof DropdownMenuPrimitive.CheckboxItem> {}
export interface DropdownMenuSubProps extends React.ComponentProps<typeof DropdownMenuPrimitive.Sub> {}
export interface DropdownMenuSubTriggerProps extends React.ComponentProps<typeof DropdownMenuPrimitive.SubTrigger> {}
export interface DropdownMenuSubContentProps extends React.ComponentProps<typeof DropdownMenuPrimitive.SubContent> {}

function DropdownMenuRoot(props: DropdownMenuProps) {
  return <DropdownMenuPrimitive.Root data-component="dropdown-menu" {...props} />
}

function DropdownMenuTrigger({ className, children, ...props }: React.PropsWithChildren<DropdownMenuTriggerProps>) {
  return (
    <DropdownMenuPrimitive.Trigger data-slot="dropdown-menu-trigger" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.Trigger>
  )
}

function DropdownMenuIcon({ className, children, ...props }: React.PropsWithChildren<DropdownMenuIconProps>) {
  return (
    <span data-slot="dropdown-menu-icon" className={cn(className)} {...props}>
      {children}
    </span>
  )
}

function DropdownMenuPortal(props: DropdownMenuPortalProps) {
  return <DropdownMenuPrimitive.Portal {...props} />
}

function DropdownMenuContent({ className, children, ...props }: React.PropsWithChildren<DropdownMenuContentProps>) {
  return (
    <DropdownMenuPrimitive.Portal>
      <DropdownMenuPrimitive.Content data-component="dropdown-menu-content" className={cn(className)} {...props}>
        {children}
      </DropdownMenuPrimitive.Content>
    </DropdownMenuPrimitive.Portal>
  )
}

function DropdownMenuArrow({ className, ...props }: DropdownMenuArrowProps) {
  return <DropdownMenuPrimitive.Arrow data-slot="dropdown-menu-arrow" className={cn(className)} {...props} />
}

function DropdownMenuSeparator({ className, ...props }: DropdownMenuSeparatorProps) {
  return <DropdownMenuPrimitive.Separator data-slot="dropdown-menu-separator" className={cn(className)} {...props} />
}

function DropdownMenuGroup({ className, children, ...props }: React.PropsWithChildren<DropdownMenuGroupProps>) {
  return (
    <DropdownMenuPrimitive.Group data-slot="dropdown-menu-group" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.Group>
  )
}

function DropdownMenuGroupLabel({ className, children, ...props }: React.PropsWithChildren<DropdownMenuGroupLabelProps>) {
  return (
    <DropdownMenuPrimitive.Label data-slot="dropdown-menu-group-label" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.Label>
  )
}

function DropdownMenuItem({ className, children, ...props }: React.PropsWithChildren<DropdownMenuItemProps>) {
  return (
    <DropdownMenuPrimitive.Item data-slot="dropdown-menu-item" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.Item>
  )
}

function DropdownMenuItemLabel({ className, children, ...props }: React.PropsWithChildren<DropdownMenuItemLabelProps>) {
  return (
    <span data-slot="dropdown-menu-item-label" className={cn(className)} {...props}>
      {children}
    </span>
  )
}

function DropdownMenuItemDescription({ className, children, ...props }: React.PropsWithChildren<DropdownMenuItemDescriptionProps>) {
  return (
    <span data-slot="dropdown-menu-item-description" className={cn(className)} {...props}>
      {children}
    </span>
  )
}

function DropdownMenuItemIndicator({ className, children, ...props }: React.PropsWithChildren<DropdownMenuItemIndicatorProps>) {
  return (
    <DropdownMenuPrimitive.ItemIndicator data-slot="dropdown-menu-item-indicator" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.ItemIndicator>
  )
}

function DropdownMenuRadioGroup({ className, children, ...props }: React.PropsWithChildren<DropdownMenuRadioGroupProps>) {
  return (
    <DropdownMenuPrimitive.RadioGroup data-slot="dropdown-menu-radio-group" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.RadioGroup>
  )
}

function DropdownMenuRadioItem({ className, children, ...props }: React.PropsWithChildren<DropdownMenuRadioItemProps>) {
  return (
    <DropdownMenuPrimitive.RadioItem data-slot="dropdown-menu-radio-item" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.RadioItem>
  )
}

function DropdownMenuCheckboxItem({ className, children, ...props }: React.PropsWithChildren<DropdownMenuCheckboxItemProps>) {
  return (
    <DropdownMenuPrimitive.CheckboxItem data-slot="dropdown-menu-checkbox-item" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.CheckboxItem>
  )
}

function DropdownMenuSub(props: DropdownMenuSubProps) {
  return <DropdownMenuPrimitive.Sub {...props} />
}

function DropdownMenuSubTrigger({ className, children, ...props }: React.PropsWithChildren<DropdownMenuSubTriggerProps>) {
  return (
    <DropdownMenuPrimitive.SubTrigger data-slot="dropdown-menu-sub-trigger" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.SubTrigger>
  )
}

function DropdownMenuSubContent({ className, children, ...props }: React.PropsWithChildren<DropdownMenuSubContentProps>) {
  return (
    <DropdownMenuPrimitive.SubContent data-component="dropdown-menu-sub-content" className={cn(className)} {...props}>
      {children}
    </DropdownMenuPrimitive.SubContent>
  )
}

export const DropdownMenu = Object.assign(DropdownMenuRoot, {
  Trigger: DropdownMenuTrigger,
  Icon: DropdownMenuIcon,
  Portal: DropdownMenuPortal,
  Content: DropdownMenuContent,
  Arrow: DropdownMenuArrow,
  Separator: DropdownMenuSeparator,
  Group: DropdownMenuGroup,
  GroupLabel: DropdownMenuGroupLabel,
  Item: DropdownMenuItem,
  ItemLabel: DropdownMenuItemLabel,
  ItemDescription: DropdownMenuItemDescription,
  ItemIndicator: DropdownMenuItemIndicator,
  RadioGroup: DropdownMenuRadioGroup,
  RadioItem: DropdownMenuRadioItem,
  CheckboxItem: DropdownMenuCheckboxItem,
  Sub: DropdownMenuSub,
  SubTrigger: DropdownMenuSubTrigger,
  SubContent: DropdownMenuSubContent,
})
