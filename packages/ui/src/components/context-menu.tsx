import { ContextMenu as ContextMenuPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export interface ContextMenuProps extends React.ComponentProps<typeof ContextMenuPrimitive.Root> {}
export interface ContextMenuTriggerProps extends React.ComponentProps<typeof ContextMenuPrimitive.Trigger> {}
export interface ContextMenuIconProps extends React.ComponentProps<"span"> {}
export interface ContextMenuPortalProps extends React.ComponentProps<typeof ContextMenuPrimitive.Portal> {}
export interface ContextMenuContentProps extends React.ComponentProps<typeof ContextMenuPrimitive.Content> {}
export interface ContextMenuArrowProps extends React.ComponentProps<typeof ContextMenuPrimitive.Arrow> {}
export interface ContextMenuSeparatorProps extends React.ComponentProps<typeof ContextMenuPrimitive.Separator> {}
export interface ContextMenuGroupProps extends React.ComponentProps<typeof ContextMenuPrimitive.Group> {}
export interface ContextMenuGroupLabelProps extends React.ComponentProps<typeof ContextMenuPrimitive.Label> {}
export interface ContextMenuItemProps extends React.ComponentProps<typeof ContextMenuPrimitive.Item> {}
export interface ContextMenuItemLabelProps extends React.ComponentProps<"span"> {}
export interface ContextMenuItemDescriptionProps extends React.ComponentProps<"span"> {}
export interface ContextMenuItemIndicatorProps extends React.ComponentProps<typeof ContextMenuPrimitive.ItemIndicator> {}
export interface ContextMenuRadioGroupProps extends React.ComponentProps<typeof ContextMenuPrimitive.RadioGroup> {}
export interface ContextMenuRadioItemProps extends React.ComponentProps<typeof ContextMenuPrimitive.RadioItem> {}
export interface ContextMenuCheckboxItemProps extends React.ComponentProps<typeof ContextMenuPrimitive.CheckboxItem> {}
export interface ContextMenuSubProps extends React.ComponentProps<typeof ContextMenuPrimitive.Sub> {}
export interface ContextMenuSubTriggerProps extends React.ComponentProps<typeof ContextMenuPrimitive.SubTrigger> {}
export interface ContextMenuSubContentProps extends React.ComponentProps<typeof ContextMenuPrimitive.SubContent> {}

function ContextMenuRoot(props: ContextMenuProps) {
  return <ContextMenuPrimitive.Root data-component="context-menu" {...props} />
}

function ContextMenuTrigger({ className, children, ...props }: React.PropsWithChildren<ContextMenuTriggerProps>) {
  return (
    <ContextMenuPrimitive.Trigger data-slot="context-menu-trigger" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.Trigger>
  )
}

function ContextMenuIcon({ className, children, ...props }: React.PropsWithChildren<ContextMenuIconProps>) {
  return (
    <span data-slot="context-menu-icon" className={cn(className)} {...props}>
      {children}
    </span>
  )
}

function ContextMenuPortal(props: ContextMenuPortalProps) {
  return <ContextMenuPrimitive.Portal {...props} />
}

function ContextMenuContent({ className, children, ...props }: React.PropsWithChildren<ContextMenuContentProps>) {
  return (
    <ContextMenuPrimitive.Portal>
      <ContextMenuPrimitive.Content data-component="context-menu-content" className={cn(className)} {...props}>
        {children}
      </ContextMenuPrimitive.Content>
    </ContextMenuPrimitive.Portal>
  )
}

function ContextMenuArrow({ className, ...props }: ContextMenuArrowProps) {
  return <ContextMenuPrimitive.Arrow data-slot="context-menu-arrow" className={cn(className)} {...props} />
}

function ContextMenuSeparator({ className, ...props }: ContextMenuSeparatorProps) {
  return <ContextMenuPrimitive.Separator data-slot="context-menu-separator" className={cn(className)} {...props} />
}

function ContextMenuGroup({ className, children, ...props }: React.PropsWithChildren<ContextMenuGroupProps>) {
  return (
    <ContextMenuPrimitive.Group data-slot="context-menu-group" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.Group>
  )
}

function ContextMenuGroupLabel({ className, children, ...props }: React.PropsWithChildren<ContextMenuGroupLabelProps>) {
  return (
    <ContextMenuPrimitive.Label data-slot="context-menu-group-label" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.Label>
  )
}

function ContextMenuItem({ className, children, ...props }: React.PropsWithChildren<ContextMenuItemProps>) {
  return (
    <ContextMenuPrimitive.Item data-slot="context-menu-item" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.Item>
  )
}

function ContextMenuItemLabel({ className, children, ...props }: React.PropsWithChildren<ContextMenuItemLabelProps>) {
  return (
    <span data-slot="context-menu-item-label" className={cn(className)} {...props}>
      {children}
    </span>
  )
}

function ContextMenuItemDescription({ className, children, ...props }: React.PropsWithChildren<ContextMenuItemDescriptionProps>) {
  return (
    <span data-slot="context-menu-item-description" className={cn(className)} {...props}>
      {children}
    </span>
  )
}

function ContextMenuItemIndicator({ className, children, ...props }: React.PropsWithChildren<ContextMenuItemIndicatorProps>) {
  return (
    <ContextMenuPrimitive.ItemIndicator data-slot="context-menu-item-indicator" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.ItemIndicator>
  )
}

function ContextMenuRadioGroup({ className, children, ...props }: React.PropsWithChildren<ContextMenuRadioGroupProps>) {
  return (
    <ContextMenuPrimitive.RadioGroup data-slot="context-menu-radio-group" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.RadioGroup>
  )
}

function ContextMenuRadioItem({ className, children, ...props }: React.PropsWithChildren<ContextMenuRadioItemProps>) {
  return (
    <ContextMenuPrimitive.RadioItem data-slot="context-menu-radio-item" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.RadioItem>
  )
}

function ContextMenuCheckboxItem({ className, children, ...props }: React.PropsWithChildren<ContextMenuCheckboxItemProps>) {
  return (
    <ContextMenuPrimitive.CheckboxItem data-slot="context-menu-checkbox-item" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.CheckboxItem>
  )
}

function ContextMenuSub(props: ContextMenuSubProps) {
  return <ContextMenuPrimitive.Sub {...props} />
}

function ContextMenuSubTrigger({ className, children, ...props }: React.PropsWithChildren<ContextMenuSubTriggerProps>) {
  return (
    <ContextMenuPrimitive.SubTrigger data-slot="context-menu-sub-trigger" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.SubTrigger>
  )
}

function ContextMenuSubContent({ className, children, ...props }: React.PropsWithChildren<ContextMenuSubContentProps>) {
  return (
    <ContextMenuPrimitive.SubContent data-component="context-menu-sub-content" className={cn(className)} {...props}>
      {children}
    </ContextMenuPrimitive.SubContent>
  )
}

export const ContextMenu = Object.assign(ContextMenuRoot, {
  Trigger: ContextMenuTrigger,
  Icon: ContextMenuIcon,
  Portal: ContextMenuPortal,
  Content: ContextMenuContent,
  Arrow: ContextMenuArrow,
  Separator: ContextMenuSeparator,
  Group: ContextMenuGroup,
  GroupLabel: ContextMenuGroupLabel,
  Item: ContextMenuItem,
  ItemLabel: ContextMenuItemLabel,
  ItemDescription: ContextMenuItemDescription,
  ItemIndicator: ContextMenuItemIndicator,
  RadioGroup: ContextMenuRadioGroup,
  RadioItem: ContextMenuRadioItem,
  CheckboxItem: ContextMenuCheckboxItem,
  Sub: ContextMenuSub,
  SubTrigger: ContextMenuSubTrigger,
  SubContent: ContextMenuSubContent,
})
