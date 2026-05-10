declare module "morphdom" {
  interface MorphDomOptions {
    childrenOnly?: boolean
    onBeforeElUpdated?: (fromEl: HTMLElement, toEl: HTMLElement) => boolean
    onElUpdated?: (el: HTMLElement) => void
    onNodeAdded?: (node: HTMLElement) => void
    onNodeDiscarded?: (node: HTMLElement) => void
    getNodeKey?: (node: HTMLElement) => string | number
  }
  export default function morphdom(
    fromNode: HTMLElement,
    toNode: HTMLElement | string,
    options?: MorphDomOptions,
  ): void
}

