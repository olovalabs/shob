import { Select as SelectPrimitive } from "@kobalte/core"

import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group"
import { ChevronDownIcon, XIcon, CheckIcon } from "lucide-solid"
import { children } from "solid-js"
import type { JSX } from "solid-js"

const Combobox = SelectPrimitive.Root

function ComboboxValue(props: any) {
  return <SelectPrimitive.Value {...props} />
}

function ComboboxTrigger(props: any) {
  const resolvedChildren = children(() => props.children)
  return (
    <SelectPrimitive.Trigger
      data-slot="combobox-trigger"
      class={cn("[&_svg:not([class*='size-'])]:size-4", props.class)}
      {...props}
    >
      {resolvedChildren()}
      <ChevronDownIcon class="pointer-events-none size-4 text-muted-foreground" />
    </SelectPrimitive.Trigger>
  )
}

interface ComboboxInputProps {
  class?: string
  children?: JSX.Element
  disabled?: boolean
  showTrigger?: boolean
  showClear?: boolean
  placeholder?: string
}

function ComboboxInput(props: ComboboxInputProps) {
  return (
    <InputGroup class={cn("w-auto", props.class)}>
      <ComboboxTrigger as={InputGroupInput} disabled={props.disabled} placeholder={props.placeholder} />
      <InputGroupAddon align="inline-end">
        {props.showTrigger !== false && (
          <InputGroupButton
            size="icon-xs"
            variant="ghost"
            data-slot="input-group-button"
            class="group-has-data-[slot=combobox-clear]/input-group:hidden data-expanded:bg-transparent"
            disabled={props.disabled}
          >
            <ChevronDownIcon class="size-4" />
          </InputGroupButton>
        )}
        {props.showClear && (
          <Button
            variant="ghost"
            size="icon-xs"
            data-slot="combobox-clear"
            class="opacity-50 hover:opacity-100"
          >
            <XIcon class="pointer-events-none" />
          </Button>
        )}
      </InputGroupAddon>
      {props.children}
    </InputGroup>
  )
}

interface ComboboxContentProps {
  class?: string
  children: JSX.Element
}

function ComboboxContent(props: ComboboxContentProps) {
  const resolvedChildren = children(() => props.children)
  return (
    <SelectPrimitive.Portal>
      <SelectPrimitive.Content
        data-slot="combobox-content"
        class={cn("group/combobox-content relative max-h-[var(--kb-select-content-available-height)] w-[var(--kb-select-trigger-width)] min-w-[calc(var(--kb-select-trigger-width)+--spacing(7))] origin-[var(--kb-select-content-transform-origin)] overflow-hidden rounded-lg bg-popover text-popover-foreground shadow-md ring-1 ring-foreground/10 duration-100 data-expanded:animate-in data-expanded:fade-in-0 data-expanded:zoom-in-95 data-closed:animate-out data-closed:fade-out-0 data-closed:zoom-out-95", props.class)}
      >
        {resolvedChildren()}
      </SelectPrimitive.Content>
    </SelectPrimitive.Portal>
  )
}

function ComboboxList(props: any) {
  return (
    <SelectPrimitive.Listbox
      data-slot="combobox-list"
      class={cn(
        "no-scrollbar max-h-[min(calc(--spacing(72)---spacing(9)),calc(var(--kb-select-content-available-height)---spacing(9)))] scroll-py-1 overflow-y-auto overscroll-contain p-1 data-empty:p-0",
        props.class
      )}
      {...props}
    >
      {() => props.children}
    </SelectPrimitive.Listbox>
  )
}

function ComboboxItem(props: any) {
  const resolvedChildren = children(() => props.children)
  return (
    <SelectPrimitive.Item
      data-slot="combobox-item"
      class={cn(
        "relative flex w-full cursor-default items-center gap-2 rounded-md py-1 pr-8 pl-1.5 text-sm outline-hidden select-none focus:bg-accent focus:text-accent-foreground data-disabled:pointer-events-none data-disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
        props.class
      )}
      {...props}
    >
      {resolvedChildren()}
      <span class="pointer-events-none absolute right-2 flex size-4 items-center justify-center">
        <SelectPrimitive.ItemIndicator>
          <CheckIcon class="pointer-events-none" />
        </SelectPrimitive.ItemIndicator>
      </span>
    </SelectPrimitive.Item>
  )
}

function ComboboxGroup(props: any) {
  return (
    <div
      data-slot="combobox-group"
      class={cn(props.class)}
      role="group"
    >
      {props.children}
    </div>
  )
}

function ComboboxLabel(props: any) {
  return (
    <div
      data-slot="combobox-label"
      class={cn("px-2 py-1.5 text-xs text-muted-foreground", props.class)}
      {...props}
    />
  )
}

function ComboboxEmpty(props: any) {
  return (
    <div
      data-slot="combobox-empty"
      class={cn(
        "hidden w-full justify-center py-2 text-center text-sm text-muted-foreground group-data-empty/combobox-content:flex",
        props.class
      )}
      {...props}
    />
  )
}

function ComboboxSeparator(props: any) {
  return (
    <div
      data-slot="combobox-separator"
      class={cn("-mx-1 my-1 h-px bg-border", props.class)}
      {...props}
    />
  )
}

function ComboboxChips(props: any) {
  return (
    <div
      data-slot="combobox-chips"
      class={cn(
        "flex min-h-8 flex-wrap items-center gap-1 rounded-lg border border-input bg-transparent bg-clip-padding px-2.5 py-1 text-sm transition-colors focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 has-data-[slot=combobox-chip]:px-1 dark:bg-input/30",
        props.class
      )}
      {...props}
    />
  )
}

function ComboboxChip(props: { class?: string; children: JSX.Element; showRemove?: boolean; onRemove?: () => void }) {
  const resolvedChildren = children(() => props.children)
  return (
    <div
      data-slot="combobox-chip"
      class={cn(
        "flex h-[calc(--spacing(5.25))] w-fit items-center justify-center gap-1 rounded-sm bg-muted px-1.5 text-xs font-medium whitespace-nowrap text-foreground has-disabled:pointer-events-none has-disabled:cursor-not-allowed has-disabled:opacity-50 has-data-[slot=combobox-chip-remove]:pr-0",
        props.class
      )}
    >
      {resolvedChildren()}
      {props.showRemove !== false && (
        <Button
          variant="ghost"
          size="icon-xs"
          class="-ml-1 opacity-50 hover:opacity-100"
          data-slot="combobox-chip-remove"
          onClick={props.onRemove}
        >
          <XIcon class="pointer-events-none" />
        </Button>
      )}
    </div>
  )
}

function ComboboxChipsInput(props: JSX.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      data-slot="combobox-chip-input"
      class={cn("min-w-16 flex-1 outline-none", props.class)}
      {...props}
    />
  )
}

function useComboboxAnchor() {
  let ref: HTMLDivElement | undefined
  return {
    get ref() { return ref },
    set ref(el: HTMLDivElement | undefined) { ref = el }
  }
}

export {
  Combobox,
  ComboboxInput,
  ComboboxContent,
  ComboboxList,
  ComboboxItem,
  ComboboxGroup,
  ComboboxLabel,
  ComboboxEmpty,
  ComboboxSeparator,
  ComboboxChips,
  ComboboxChip,
  ComboboxChipsInput,
  ComboboxTrigger,
  ComboboxValue,
  useComboboxAnchor,
}
