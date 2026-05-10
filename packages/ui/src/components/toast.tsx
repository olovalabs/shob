import { toast as sonnerToast, Toaster as SonnerToaster } from "sonner"
import type { ToasterProps } from "sonner"
import * as React from "react"
import { useI18n } from "../context/i18n"
import { Icon, type IconProps } from "./icon"
import { IconButton } from "./icon-button"

export interface ToastRegionProps {
  theme?: "light" | "dark"
}

function ToastRegion(props: ToastRegionProps & Omit<ToasterProps, "theme">) {
  return <SonnerToaster theme={props.theme ?? "dark"} {...props} />
}

export interface ToastRootProps {
  class?: string
  children?: React.ReactNode
  toastId?: string | number
  duration?: number
  persistent?: boolean
}

function ToastRoot({ class: className, children }: ToastRootProps) {
  return (
    <div data-component="toast" className={className}>
      {children}
    </div>
  )
}

function ToastIcon(props: { name: IconProps["name"] }) {
  return (
    <div data-slot="toast-icon">
      <Icon name={props.name} />
    </div>
  )
}

function ToastContent(props: React.ComponentProps<"div">) {
  return <div data-slot="toast-content" {...props} />
}

function ToastTitle(props: React.ComponentProps<"div">) {
  return <div data-slot="toast-title" {...props} />
}

function ToastDescription(props: React.ComponentProps<"div">) {
  return <div data-slot="toast-description" {...props} />
}

function ToastActions(props: React.ComponentProps<"div">) {
  return <div data-slot="toast-actions" {...props} />
}

function ToastCloseButton(props: React.ComponentProps<"button">) {
  const i18n = useI18n()
  return (
    <IconButton data-slot="toast-close-button" icon="close" variant="ghost" aria-label={i18n.t("ui.common.dismiss")} {...props} />
  )
}

function ToastProgressTrack(props: React.ComponentProps<"div">) {
  return <div data-slot="toast-progress-track" {...props} />
}

function ToastProgressFill(props: React.ComponentProps<"div">) {
  return <div data-slot="toast-progress-fill" {...props} />
}

export const Toast = Object.assign(ToastRoot, {
  Region: ToastRegion,
  Icon: ToastIcon,
  Content: ToastContent,
  Title: ToastTitle,
  Description: ToastDescription,
  Actions: ToastActions,
  CloseButton: ToastCloseButton,
  ProgressTrack: ToastProgressTrack,
  ProgressFill: ToastProgressFill,
})

export { sonnerToast as toaster }

export type ToastVariant = "default" | "success" | "error" | "loading"

export interface ToastAction {
  label: string
  onClick: "dismiss" | (() => void)
}

export interface ToastOptions {
  title?: string
  description?: string
  icon?: IconProps["name"]
  variant?: ToastVariant
  duration?: number
  persistent?: boolean
  actions?: ToastAction[]
}

function ToastContentInner({ options, onDismiss }: { options: ToastOptions; onDismiss: () => void }) {
  return (
    <Toast>
      {options.icon && <Toast.Icon name={options.icon} />}
      <Toast.Content>
        {options.title && <Toast.Title>{options.title}</Toast.Title>}
        {options.description && <Toast.Description>{options.description}</Toast.Description>}
        {options.actions && options.actions.length > 0 && (
          <Toast.Actions>
            {options.actions.map((action, i) => (
              <button key={i} data-slot="toast-action" onClick={() => {
                if (typeof action.onClick === "function") {
                  action.onClick()
                }
                onDismiss()
              }}>
                {action.label}
              </button>
            ))}
          </Toast.Actions>
        )}
      </Toast.Content>
      <Toast.CloseButton />
    </Toast>
  )
}

export function showToast(options: ToastOptions | string) {
  const opts = typeof options === "string" ? { description: options } : options
  const id = sonnerToast.custom(
    (t) => (
      <ToastContentInner
        options={opts}
        onDismiss={() => sonnerToast.dismiss(t)}
      />
    ),
    {
      duration: opts.persistent ? Infinity : opts.duration,
      unstyled: true,
    },
  )
  return id
}

export interface ToastPromiseOptions<T, U = unknown> {
  loading?: React.ReactNode
  success?: (data: T) => React.ReactNode
  error?: (error: U) => React.ReactNode
}

export function showPromiseToast<T, U = unknown>(
  promise: Promise<T> | (() => Promise<T>),
  options: ToastPromiseOptions<T, U>,
) {
  const p = typeof promise === "function" ? promise() : promise
  const id = sonnerToast.custom(
    () => (
      <Toast>
        <Toast.Content>
          <Toast.Description>{options.loading}</Toast.Description>
        </Toast.Content>
      </Toast>
    ),
    { unstyled: true },
  )

  p.then(
    (data) => {
      sonnerToast.custom(
        () => (
          <Toast>
            <Toast.Content>
              <Toast.Description>{options.success?.(data)}</Toast.Description>
            </Toast.Content>
          </Toast>
        ),
        { id, unstyled: true },
      )
    },
    (error) => {
      sonnerToast.custom(
        () => (
          <Toast>
            <Toast.Content>
              <Toast.Description>{options.error?.(error)}</Toast.Description>
            </Toast.Content>
          </Toast>
        ),
        { id, unstyled: true },
      )
    },
  )

  return id
}
