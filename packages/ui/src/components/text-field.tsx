import { useState } from "react"
import { cn } from "../lib/utils"
import { useI18n } from "../context/i18n"
import { IconButton } from "./icon-button"
import { Tooltip } from "./tooltip"

export interface TextFieldProps {
  name?: string
  defaultValue?: string
  value?: string
  onChange?: (value: string) => void
  onKeyDown?: (event: React.KeyboardEvent) => void
  validationState?: "valid" | "invalid"
  required?: boolean
  disabled?: boolean
  readOnly?: boolean
  class?: string
  label?: string
  hideLabel?: boolean
  description?: string
  error?: string
  variant?: "normal" | "ghost"
  copyable?: boolean
  copyKind?: "clipboard" | "link"
  multiline?: boolean
  placeholder?: string
}

function TextField(props: TextFieldProps) {
  const i18n = useI18n()
  const [copied, setCopied] = useState(false)

  const label = () => {
    if (copied) return i18n.t("ui.textField.copied")
    if (props.copyKind === "link") return i18n.t("ui.textField.copyLink")
    return i18n.t("ui.textField.copyToClipboard")
  }

  const icon = () => {
    if (copied) return "check"
    if (props.copyKind === "link") return "link"
    return "copy"
  }

  async function handleCopy() {
    const value = props.value ?? props.defaultValue ?? ""
    await navigator.clipboard.writeText(value)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  function handleClick() {
    if (props.copyable) void handleCopy()
  }

  const handleChange = (e: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) => {
    props.onChange?.(e.target.value)
  }

  const handleKeyDown = (e: React.KeyboardEvent) => {
    props.onKeyDown?.(e)
  }

  return (
    <div
      data-component="input"
      data-variant={props.variant || "normal"}
      data-invalid={props.validationState === "invalid" ? "" : undefined}
      className={cn(props.class)}
    >
      {props.label && (
        <label data-slot="input-label" className={cn(props.hideLabel && "sr-only")}>
          {props.label}
        </label>
      )}
      <div data-slot="input-wrapper">
        {props.multiline ? (
          <textarea
            data-slot="input-input"
            name={props.name}
            defaultValue={props.defaultValue}
            value={props.value}
            onChange={handleChange}
            onKeyDown={handleKeyDown}
            onClick={handleClick}
            required={props.required}
            disabled={props.disabled}
            readOnly={props.readOnly}
            placeholder={props.placeholder}
            className={cn(props.class)}
          />
        ) : (
          <input
            data-slot="input-input"
            name={props.name}
            defaultValue={props.defaultValue}
            value={props.value}
            onChange={handleChange}
            onKeyDown={handleKeyDown}
            onClick={handleClick}
            required={props.required}
            disabled={props.disabled}
            readOnly={props.readOnly}
            placeholder={props.placeholder}
            className={cn(props.class)}
          />
        )}
        {props.copyable && (
          <Tooltip value={label()} placement="top" gutter={4} forceOpen={copied} skipDelayDuration={0}>
            <IconButton
              type="button"
              icon={icon()}
              variant="ghost"
              onClick={handleCopy}
              tabIndex={-1}
              data-slot="input-copy-button"
              aria-label={label()}
            />
          </Tooltip>
        )}
      </div>
      {props.description && <p data-slot="input-description">{props.description}</p>}
      {props.error && <p data-slot="input-error">{props.error}</p>}
    </div>
  )
}

export { TextField }
