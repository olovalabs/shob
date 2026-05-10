import { Accordion as AccordionPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export type AccordionProps = Omit<React.ComponentProps<typeof AccordionPrimitive.Root>, "type"> & {
  type?: "single" | "multiple"
}
export interface AccordionItemProps extends React.ComponentProps<typeof AccordionPrimitive.Item> {}
export interface AccordionHeaderProps extends React.ComponentProps<typeof AccordionPrimitive.Header> {}
export interface AccordionTriggerProps extends React.ComponentProps<typeof AccordionPrimitive.Trigger> {}
export interface AccordionContentProps extends React.ComponentProps<typeof AccordionPrimitive.Content> {}

function AccordionRoot({ className, type = "single", ...props }: AccordionProps) {
  return <AccordionPrimitive.Root type={type} data-component="accordion" className={cn(className)} {...props} />
}

function AccordionItem({ className, ...props }: AccordionItemProps) {
  return <AccordionPrimitive.Item data-slot="accordion-item" className={cn(className)} {...props} />
}

function AccordionHeader({ className, children, ...props }: React.PropsWithChildren<AccordionHeaderProps>) {
  return (
    <AccordionPrimitive.Header data-slot="accordion-header" className={cn(className)} {...props}>
      {children}
    </AccordionPrimitive.Header>
  )
}

function AccordionTrigger({ className, children, ...props }: React.PropsWithChildren<AccordionTriggerProps>) {
  return (
    <AccordionPrimitive.Trigger data-slot="accordion-trigger" className={cn(className)} {...props}>
      {children}
    </AccordionPrimitive.Trigger>
  )
}

function AccordionContent({ className, children, ...props }: React.PropsWithChildren<AccordionContentProps>) {
  return (
    <AccordionPrimitive.Content data-slot="accordion-content" className={cn(className)} {...props}>
      {children}
    </AccordionPrimitive.Content>
  )
}

export const Accordion = Object.assign(AccordionRoot, {
  Item: AccordionItem,
  Header: AccordionHeader,
  Trigger: AccordionTrigger,
  Content: AccordionContent,
})
