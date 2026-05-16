// @ts-nocheck
import { Collapsible as CollapsiblePrimitive } from "@kobalte/core"

import type { JSX } from "solid-js"
import { children } from "solid-js"

function Collapsible(props: any) {
  return <CollapsiblePrimitive.Root data-slot="collapsible" {...props} />
}

function CollapsibleTrigger(props: any) {
  return (
    <CollapsiblePrimitive.Trigger
      data-slot="collapsible-trigger"
      {...props}
    />
  )
}

function CollapsibleContent(props: any) {
  const resolvedChildren = children(() => props.children)
  return (
    <CollapsiblePrimitive.Content
      data-slot="collapsible-content"
      {...props}
    >
      {resolvedChildren()}
    </CollapsiblePrimitive.Content>
  )
}

export { Collapsible, CollapsibleTrigger, CollapsibleContent }
