import type { PropsWithChildren } from "react"
import { Accordion } from "./accordion"

export function StickyAccordionHeader(props: PropsWithChildren) {
  return (
    <Accordion.Header data-component="sticky-accordion-header">
      {props.children}
    </Accordion.Header>
  )
}
