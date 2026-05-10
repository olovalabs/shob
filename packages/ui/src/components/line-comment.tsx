import { useFilteredList } from "@opencode-ai/ui/hooks"
import { getDirectory, getFilename } from "@opencode-ai/core/util/path"
import { useState, useEffect } from "react"
import { Button } from "./button"
import { FileIcon } from "./file-icon"
import { Icon } from "./icon"
import { installLineCommentStyles } from "./line-comment-styles"
import { useI18n } from "../context/i18n"

installLineCommentStyles()

export type LineCommentVariant = "default" | "editor" | "add"

function InlineGlyph(props: { icon: "comment" | "plus" }) {
  return (
    <svg data-slot="line-comment-icon" viewBox="0 0 20 20" fill="none" aria-hidden="true">
      {props.icon === "comment" ? (
        <path d="M16.25 3.75H3.75V16.25L6.875 14.4643H16.25V3.75Z" stroke="currentColor" strokeLinecap="square" />
      ) : (
        <path
          d="M10 5.41699V10.0003M10 10.0003V14.5837M10 10.0003H5.4165M10 10.0003H14.5832"
          stroke="currentColor"
          strokeLinecap="square"
        />
      )}
    </svg>
  )
}

export type LineCommentAnchorProps = {
  id?: string
  top?: number
  inline?: boolean
  hideButton?: boolean
  open: boolean
  variant?: LineCommentVariant
  icon?: "comment" | "plus"
  buttonLabel?: string
  onClick?: React.MouseEventHandler<HTMLButtonElement>
  onMouseEnter?: React.MouseEventHandler<HTMLButtonElement>
  onPopoverFocusOut?: React.FocusEventHandler<HTMLDivElement>
  class?: string
  popoverClass?: string
  children?: React.ReactNode
}

function buildClassName(base: string | undefined, classList: Record<string, boolean | undefined> | undefined): string | undefined {
  const classes = [...(base ? [base] : [])]
  if (classList) {
    for (const [key, val] of Object.entries(classList)) {
      if (val) classes.push(key)
    }
  }
  return classes.filter(Boolean).join(" ") || undefined
}

export const LineCommentAnchor = (props: LineCommentAnchorProps) => {
  const hidden = !props.inline && props.top === undefined
  const variant = props.variant ?? "default"
  const icon = props.icon ?? "comment"
  const inlineBody = props.inline && props.hideButton

  return (
    <div
      data-component="line-comment"
      data-prevent-autofocus=""
      data-variant={variant}
      data-comment-id={props.id}
      data-open={props.open ? "" : undefined}
      data-inline={props.inline ? "" : undefined}
      className={buildClassName(props.class, undefined)}
      style={
        props.inline
          ? undefined
          : {
              top: `${props.top ?? 0}px`,
              opacity: hidden ? 0 : 1,
              pointerEvents: hidden ? "none" : "auto",
            }
      }
    >
      {inlineBody ? (
        <div
          data-slot="line-comment-popover"
          data-inline-body=""
          className={buildClassName(props.popoverClass, undefined)}
          onMouseDown={(e) => e.stopPropagation()}
          onClick={props.onClick as any}
          onMouseEnter={props.onMouseEnter as any}
          onBlur={props.onPopoverFocusOut as any}
        >
          {props.children}
        </div>
      ) : (
        <>
          <button
            type="button"
            aria-label={props.buttonLabel}
            data-slot="line-comment-button"
            onMouseDown={(e) => e.stopPropagation()}
            onMouseUp={(e) => e.stopPropagation()}
            onClick={props.onClick as any}
            onMouseEnter={props.onMouseEnter as any}
          >
            {props.inline ? (
              <InlineGlyph icon={icon} />
            ) : (
              <Icon name={icon === "plus" ? "plus-small" : "comment"} size="small" />
            )}
          </button>
          {props.open && (
            <div
              data-slot="line-comment-popover"
              className={buildClassName(props.popoverClass, undefined)}
              onMouseDown={(e) => e.stopPropagation()}
              onBlur={props.onPopoverFocusOut as any}
            >
              {props.children}
            </div>
          )}
        </>
      )}
    </div>
  )
}

export type LineCommentProps = Omit<LineCommentAnchorProps, "children" | "variant"> & {
  comment: React.ReactNode
  selection: React.ReactNode
  actions?: React.ReactNode
}

export const LineComment = ({ comment, selection, actions, ...rest }: LineCommentProps) => {
  const i18n = useI18n()

  return (
    <LineCommentAnchor {...rest} variant="default" hideButton={rest.inline}>
      <div data-slot="line-comment-content">
        <div data-slot="line-comment-head">
          <div data-slot="line-comment-text">{comment}</div>
          {actions && (
            <div data-slot="line-comment-tools">{actions}</div>
          )}
        </div>
        <div data-slot="line-comment-label">
          {i18n.t("ui.lineComment.label.prefix")}
          {selection}
          {i18n.t("ui.lineComment.label.suffix")}
        </div>
      </div>
    </LineCommentAnchor>
  )
}

export type LineCommentAddProps = Omit<LineCommentAnchorProps, "children" | "variant" | "open" | "icon"> & {
  label?: string
}

export const LineCommentAdd = ({ label, ...rest }: LineCommentAddProps) => {
  const i18n = useI18n()

  return (
    <LineCommentAnchor
      {...rest}
      open={false}
      variant="add"
      icon="plus"
      buttonLabel={label ?? i18n.t("ui.lineComment.submit")}
    />
  )
}

export type LineCommentEditorProps = Omit<LineCommentAnchorProps, "children" | "open" | "variant" | "onClick"> & {
  value: string
  selection: React.ReactNode
  onInput: (value: string) => void
  onCancel: VoidFunction
  onSubmit: (value: string) => void
  placeholder?: string
  rows?: number
  autofocus?: boolean
  cancelLabel?: string
  submitLabel?: string
  mention?: {
    items: (query: string) => string[] | Promise<string[]>
  }
}

export const LineCommentEditor = (props: LineCommentEditorProps) => {
  const i18n = useI18n()
  const { value, selection, onInput, onCancel, onSubmit, placeholder, rows, autofocus, cancelLabel, submitLabel, mention, ...rest } = props

  const refs = {
    textarea: undefined as HTMLTextAreaElement | undefined,
  }
  const [open, setOpen] = useState(false)

  function selectMention(item: { path: string } | undefined) {
    if (!item) return

    const textarea = refs.textarea
    const query = currentMention()
    if (!textarea || !query) return

    const val = `${textarea.value.slice(0, query.start)}@${item.path} ${textarea.value.slice(query.end)}`
    const cursor = query.start + item.path.length + 2

    onInput(val)
    closeMention()

    requestAnimationFrame(() => {
      textarea.focus()
      textarea.setSelectionRange(cursor, cursor)
    })
  }

  const mentionList = useFilteredList<{ path: string }>({
    items: async (query) => {
      if (!mention) return []
      if (!query.trim()) return []
      const paths = await mention.items(query)
      return paths.map((path) => ({ path }))
    },
    key: (item) => item.path,
    filterKeys: ["path"],
    onSelect: selectMention,
  })

  const focus = () => refs.textarea?.focus()
  const hold: React.MouseEventHandler<HTMLButtonElement> = (e) => {
    e.preventDefault()
    e.stopPropagation()
  }
  const click =
    (fn: VoidFunction): React.MouseEventHandler<HTMLButtonElement> =>
    (e) => {
      e.stopPropagation()
      fn()
    }

  const closeMention = () => {
    setOpen(false)
    mentionList.clear()
  }

  const currentMention = () => {
    const textarea = refs.textarea
    if (!textarea) return
    if (!mention) return
    if (textarea.selectionStart !== textarea.selectionEnd) return

    const end = textarea.selectionStart
    const match = textarea.value.slice(0, end).match(/@(\S*)$/)
    if (!match) return

    return {
      query: match[1] ?? "",
      start: end - match[0].length,
      end,
    }
  }

  const syncMention = () => {
    const item = currentMention()
    if (!item) {
      closeMention()
      return
    }

    setOpen(true)
    mentionList.onInput(item.query)
  }

  const selectActiveMention = () => {
    const items = mentionList.flat()
    if (items.length === 0) return
    const active = mentionList.active()
    selectMention(items.find((item) => item.path === active) ?? items[0])
  }

  const submit = () => {
    const v = value.trim()
    if (!v) return
    onSubmit(v)
  }

  useEffect(() => {
    if (autofocus === false) return
    requestAnimationFrame(focus)
  }, [autofocus])

  return (
    <LineCommentAnchor {...rest} open={true} variant="editor" hideButton={props.inline} onClick={() => focus()}>
      <div data-slot="line-comment-editor">
        <textarea
          ref={(el) => {
            refs.textarea = el ?? undefined
          }}
          data-slot="line-comment-textarea"
          rows={rows ?? 3}
          placeholder={placeholder ?? i18n.t("ui.lineComment.placeholder")}
          value={value}
          onInput={(e) => {
            const val = (e.currentTarget as HTMLTextAreaElement).value
            onInput(val)
            syncMention()
          }}
          onClick={() => syncMention()}
          onSelect={() => syncMention()}
          onKeyDown={(e) => {
            const event = e as unknown as KeyboardEvent
            if (event.isComposing || event.keyCode === 229) return
            event.stopPropagation()
            if (open) {
              if (e.key === "Escape") {
                event.preventDefault()
                closeMention()
                return
              }

              if (e.key === "Tab") {
                if (mentionList.flat().length === 0) return
                event.preventDefault()
                selectActiveMention()
                return
              }

              const nav = e.key === "ArrowUp" || e.key === "ArrowDown" || e.key === "Enter"
              const ctrlNav =
                event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey && (e.key === "n" || e.key === "p")
              if ((nav || ctrlNav) && mentionList.flat().length > 0) {
                mentionList.onKeyDown(event)
                event.preventDefault()
                return
              }
            }

            if (e.key === "Escape") {
              event.preventDefault()
              e.currentTarget.blur()
              onCancel()
              return
            }
            if (e.key !== "Enter") return
            if (e.shiftKey) return
            event.preventDefault()
            submit()
          }}
        />
        {open && mentionList.flat().length > 0 && (
          <div data-slot="line-comment-mention-list">
            {mentionList.flat().slice(0, 10).map((item) => {
              const directory = item.path.endsWith("/") ? item.path : getDirectory(item.path)
              const name = item.path.endsWith("/") ? "" : getFilename(item.path)
              return (
                <button
                  key={item.path}
                  type="button"
                  data-slot="line-comment-mention-item"
                  data-active={mentionList.active() === item.path ? "" : undefined}
                  onMouseDown={(event) => event.preventDefault()}
                  onMouseEnter={() => mentionList.setActive(item.path)}
                  onClick={() => selectMention(item)}
                >
                  <FileIcon node={{ path: item.path, type: "file" }} className="shrink-0 size-4" />
                  <div data-slot="line-comment-mention-path">
                    <span data-slot="line-comment-mention-dir">{directory}</span>
                    {name && (
                      <span data-slot="line-comment-mention-file">{name}</span>
                    )}
                  </div>
                </button>
              )
            })}
          </div>
        )}
        <div data-slot="line-comment-actions">
          <div data-slot="line-comment-editor-label">
            {i18n.t("ui.lineComment.editorLabel.prefix")}
            {selection}
            {i18n.t("ui.lineComment.editorLabel.suffix")}
          </div>
          {!props.inline ? (
            <>
              <Button size="small" variant="ghost" onClick={onCancel}>
                {cancelLabel ?? i18n.t("ui.common.cancel")}
              </Button>
              <Button size="small" variant="primary" disabled={value.trim().length === 0} onClick={submit}>
                {submitLabel ?? i18n.t("ui.lineComment.submit")}
              </Button>
            </>
          ) : (
            <>
              <button
                type="button"
                data-slot="line-comment-action"
                data-variant="ghost"
                onMouseDown={hold as any}
                onClick={click(onCancel) as any}
              >
                {cancelLabel ?? i18n.t("ui.common.cancel")}
              </button>
              <button
                type="button"
                data-slot="line-comment-action"
                data-variant="primary"
                disabled={value.trim().length === 0}
                onMouseDown={hold as any}
                onClick={click(submit) as any}
              >
                {submitLabel ?? i18n.t("ui.lineComment.submit")}
              </button>
            </>
          )}
        </div>
      </div>
    </LineCommentAnchor>
  )
}
