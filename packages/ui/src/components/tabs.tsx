import { Tabs as TabsPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export interface TabsProps extends React.ComponentProps<typeof TabsPrimitive.Root> {
  variant?: "normal" | "alt" | "pill" | "settings"
}
export interface TabsListProps extends React.ComponentProps<typeof TabsPrimitive.List> {}
export interface TabsTriggerProps extends React.ComponentProps<typeof TabsPrimitive.Trigger> {
  classes?: {
    button?: string
  }
  hideCloseButton?: boolean
  closeButton?: React.ReactNode
  onMiddleClick?: () => void
}
export interface TabsContentProps extends React.ComponentProps<typeof TabsPrimitive.Content> {}

function TabsRoot({ className, variant = "normal", orientation = "horizontal", ...props }: TabsProps) {
  return (
    <TabsPrimitive.Root
      data-component="tabs"
      data-variant={variant}
      data-orientation={orientation}
      orientation={orientation}
      className={cn(className)}
      {...props}
    />
  )
}

function TabsList({ className, ...props }: TabsListProps) {
  return <TabsPrimitive.List data-slot="tabs-list" className={cn(className)} {...props} />
}

function TabsTrigger({ className, classes, children, closeButton, hideCloseButton, onMiddleClick, ...props }: React.PropsWithChildren<TabsTriggerProps>) {
  return (
    <div
      data-slot="tabs-trigger-wrapper"
      data-value={props.value}
      className={cn(className)}
      onMouseDown={(e) => {
        if (e.button === 1 && onMiddleClick) {
          e.preventDefault()
        }
      }}
      onAuxClick={(e) => {
        if (e.button === 1 && onMiddleClick) {
          e.preventDefault()
          onMiddleClick()
        }
      }}
    >
      <TabsPrimitive.Trigger
        data-slot="tabs-trigger"
        data-value={props.value}
        className={cn(classes?.button)}
        {...props}
      >
        {children}
      </TabsPrimitive.Trigger>
      {closeButton && (
        <div data-slot="tabs-trigger-close-button" data-hidden={hideCloseButton}>
          {closeButton}
        </div>
      )}
    </div>
  )
}

function TabsContent({ className, children, ...props }: React.PropsWithChildren<TabsContentProps>) {
  return (
    <TabsPrimitive.Content data-slot="tabs-content" className={cn(className)} {...props}>
      {children}
    </TabsPrimitive.Content>
  )
}

const TabsSectionTitle: React.FC<React.PropsWithChildren> = (props) => {
  return <div data-slot="tabs-section-title">{props.children}</div>
}

export const Tabs = Object.assign(TabsRoot, {
  List: TabsList,
  Trigger: TabsTrigger,
  Content: TabsContent,
  SectionTitle: TabsSectionTitle,
})
