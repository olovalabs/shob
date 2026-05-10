import { Select as SelectPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"
import { pipe, groupBy, entries, map } from "remeda"
import { Icon } from "./icon"

export type SelectProps<T> = {
  placeholder?: string
  options: T[]
  current?: T
  value?: (x: T) => string
  label?: (x: T) => string
  groupBy?: (x: T) => string
  valueClass?: string
  onSelect?: (value: T | undefined) => void
  onHighlight?: (value: T | undefined) => (() => void) | void
  class?: string
  disabled?: boolean
  size?: "small" | "normal" | "large"
  variant?: "primary" | "secondary" | "ghost"
  triggerStyle?: React.CSSProperties
  triggerVariant?: "settings"
  triggerProps?: Record<string, string | number | boolean | undefined>
  children?: (item: T | undefined) => React.ReactNode
}

function Select<T>({
  placeholder,
  options,
  current,
  value: valueFn,
  label: labelFn,
  groupBy: groupByFn,
  valueClass,
  onSelect,
  class: className,
  disabled,
  size,
  variant,
  triggerStyle,
  triggerVariant,
  triggerProps,
  children: renderChildren,
}: SelectProps<T>) {
  const getValue = (item: T): string => {
    if (valueFn) return valueFn(item)
    return String(item)
  }

  const getLabel = (item: T): string => {
    if (labelFn) return labelFn(item)
    return String(item)
  }

  const grouped = React.useMemo(() => {
    const result = pipe(
      options,
      groupBy((x) => (groupByFn ? groupByFn(x) : "")),
      entries(),
      map(([k, v]) => ({ category: k, options: v })),
    )
    return result
  })

  const currentValue = current ? getValue(current) : undefined

  return (
    <SelectPrimitive.Root
      value={currentValue}
      onValueChange={(v) => {
        const found = options.find((opt) => getValue(opt) === v)
        onSelect?.(found ?? undefined)
      }}
      disabled={disabled}
    >
      <SelectPrimitive.Trigger
        data-component="select"
        data-trigger-style={triggerVariant}
        className={cn(className)}
        style={triggerStyle}
        {...(triggerProps as Record<string, string>)}
      >
        <SelectPrimitive.Value placeholder={placeholder} className={valueClass} />
        <SelectPrimitive.Icon>
          <Icon name={triggerVariant === "settings" ? "selector" : "chevron-down"} size="small" />
        </SelectPrimitive.Icon>
      </SelectPrimitive.Trigger>
      <SelectPrimitive.Portal>
        <SelectPrimitive.Content data-component="select-content" data-trigger-style={triggerVariant} className={cn(className)}>
          <SelectPrimitive.Viewport>
            {grouped().map((group) => (
              <React.Fragment key={group.category}>
                {group.category && <SelectPrimitive.Group>
                  <SelectPrimitive.Label data-slot="select-section">{group.category}</SelectPrimitive.Label>
                  {group.options.map((option) => (
                    <SelectPrimitive.Item key={getValue(option)} value={getValue(option)} data-slot="select-select-item">
                      <SelectPrimitive.ItemText data-slot="select-select-item-label">
                        {renderChildren
                          ? renderChildren(option)
                          : labelFn
                            ? labelFn(option)
                            : (option as string)}
                      </SelectPrimitive.ItemText>
                      <SelectPrimitive.ItemIndicator data-slot="select-select-item-indicator">
                        <Icon name="check-small" size="small" />
                      </SelectPrimitive.ItemIndicator>
                    </SelectPrimitive.Item>
                  ))}
                </SelectPrimitive.Group>}
              </React.Fragment>
            ))}
          </SelectPrimitive.Viewport>
        </SelectPrimitive.Content>
      </SelectPrimitive.Portal>
    </SelectPrimitive.Root>
  )
}

export { Select }
