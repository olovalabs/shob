import { RadioGroup as RadioGroupPrimitive } from "radix-ui"
import * as React from "react"
import { cn } from "../lib/utils"

export type RadioGroupProps<T> = Omit<
  React.ComponentProps<typeof RadioGroupPrimitive.Root>,
  "value" | "defaultValue" | "onValueChange" | "children"
> & {
  options: T[]
  current?: T
  defaultValue?: T
  value?: (x: T) => string
  label?: (x: T) => React.ReactNode | string
  onSelect?: (value: T | undefined) => void
  size?: "small" | "medium"
  fill?: boolean
  pad?: "none" | "normal"
}

function RadioGroup<T>({
  className,
  options,
  current,
  defaultValue,
  value: valueFn,
  label: labelFn,
  onSelect,
  size = "medium",
  fill,
  pad = "normal",
  ...props
}: RadioGroupProps<T>) {
  const getValue = (item: T): string => {
    if (valueFn) return valueFn(item)
    return String(item)
  }

  const getLabel = (item: T): React.ReactNode | string => {
    if (labelFn) return labelFn(item)
    return String(item)
  }

  const findOption = (v: string): T | undefined => {
    return options.find((opt) => getValue(opt) === v)
  }

  return (
    <RadioGroupPrimitive.Root
      data-component="radio-group"
      data-size={size}
      data-fill={fill ? "" : undefined}
      data-pad={pad}
      className={cn(className)}
      value={current ? getValue(current) : undefined}
      defaultValue={defaultValue ? getValue(defaultValue) : undefined}
      onValueChange={(v) => onSelect?.(findOption(v))}
      {...props}
    >
      <div role="presentation" data-slot="radio-group-wrapper">
        <div role="presentation" data-slot="radio-group-indicator" />
        <div role="presentation" data-slot="radio-group-items">
          {options.map((option) => (
            <RadioGroupPrimitive.Item key={getValue(option)} value={getValue(option)} data-slot="radio-group-item" data-value={getValue(option)}>
              <span data-slot="radio-group-item-label">
                <span data-slot="radio-group-item-control">{getLabel(option)}</span>
              </span>
            </RadioGroupPrimitive.Item>
          ))}
        </div>
      </div>
    </RadioGroupPrimitive.Root>
  )
}

export { RadioGroup }
