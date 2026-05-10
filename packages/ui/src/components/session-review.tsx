import { Accordion } from "./accordion"
import { Button } from "./button"
import { DropdownMenu } from "./dropdown-menu"
import { RadioGroup } from "./radio-group"
import { DiffChanges } from "./diff-changes"
import { FileIcon } from "./file-icon"
import { Icon } from "./icon"
import { IconButton } from "./icon-button"
import { StickyAccordionHeader } from "./sticky-accordion-header"
import { Tooltip } from "./tooltip"
import { ScrollView } from "./scroll-view"
import { useFileComponent } from "../context/file"
import { useI18n } from "../context/i18n"
import { getDirectory, getFilename } from "@opencode-ai/core/util/path"
import { checksum } from "../lib/encode"
import { useEffect, useMemo, useRef, useState } from "react"
import { type FileContent, type SnapshotFileDiff, type VcsFileDiff } from "@opencode-ai/sdk/v2"
import { PreloadMultiFileDiffResult } from "@pierre/diffs/ssr"
import { type SelectedLineRange } from "@pierre/diffs"
import { mediaKindFromPath } from "../pierre/media"
import { cloneSelectedLineRange, previewSelectedLines } from "../pierre/selection-bridge"
import { createLineCommentController } from "./line-comment-annotations"
import type { LineCommentEditorProps } from "./line-comment"
import { normalize, text, type ViewDiff } from "./session-diff"

const MAX_DIFF_CHANGED_LINES = 500
const REVIEW_MOUNT_MARGIN = 300

export type SessionReviewDiffStyle = "unified" | "split"

export type SessionReviewComment = {
  id: string
  file: string
  selection: SelectedLineRange
  comment: string
}

export type SessionReviewLineComment = {
  file: string
  selection: SelectedLineRange
  comment: string
  preview?: string
}

export type SessionReviewCommentUpdate = SessionReviewLineComment & {
  id: string
}

export type SessionReviewCommentDelete = {
  id: string
  file: string
}

export type SessionReviewCommentActions = {
  moreLabel: string
  editLabel: string
  deleteLabel: string
  saveLabel: string
}

export type SessionReviewFocus = { file: string; id: string }

type ReviewDiff = (SnapshotFileDiff | VcsFileDiff) & { preloaded?: PreloadMultiFileDiffResult<any> }
type Item = ViewDiff & { preloaded?: PreloadMultiFileDiffResult<any> }

function diff(value: unknown): value is ReviewDiff {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false
  if (!("file" in value) || typeof value.file !== "string") return false
  if (!("additions" in value) || typeof value.additions !== "number") return false
  if (!("deletions" in value) || typeof value.deletions !== "number") return false
  if ("patch" in value && value.patch !== undefined && typeof value.patch !== "string") return false
  if ("before" in value && value.before !== undefined && typeof value.before !== "string") return false
  if ("after" in value && value.after !== undefined && typeof value.after !== "string") return false
  if (!("status" in value) || value.status === undefined) return true
  return value.status === "added" || value.status === "deleted" || value.status === "modified"
}

function list(value: unknown): ReviewDiff[] {
  if (Array.isArray(value) && value.every(diff)) return value
  if (Array.isArray(value)) return value.filter(diff)
  if (diff(value)) return [value]
  if (!value || typeof value !== "object") return []
  return Object.values(value).filter(diff)
}

export interface SessionReviewProps {
  title?: React.ReactNode
  empty?: React.ReactNode
  split?: boolean
  diffStyle?: SessionReviewDiffStyle
  onDiffStyleChange?: (diffStyle: SessionReviewDiffStyle) => void
  onDiffRendered?: VoidFunction
  onLineComment?: (comment: SessionReviewLineComment) => void
  onLineCommentUpdate?: (comment: SessionReviewCommentUpdate) => void
  onLineCommentDelete?: (comment: SessionReviewCommentDelete) => void
  lineCommentActions?: SessionReviewCommentActions
  comments?: SessionReviewComment[]
  focusedComment?: SessionReviewFocus | null
  onFocusedCommentChange?: (focus: SessionReviewFocus | null) => void
  focusedFile?: string
  open?: string[]
  onOpenChange?: (open: string[]) => void
  scrollRef?: (el: HTMLDivElement) => void
  onScroll?: React.UIEventHandler<HTMLDivElement>
  className?: string
  classes?: { root?: string; header?: string; container?: string }
  actions?: React.ReactNode
  diffs: ReviewDiff[]
  onViewFile?: (file: string) => void
  readFile?: (path: string) => Promise<FileContent | undefined>
  lineCommentMention?: LineCommentEditorProps["mention"]
}

function ReviewCommentMenu(props: {
  labels: SessionReviewCommentActions
  onEdit: VoidFunction
  onDelete: VoidFunction
}) {
  return (
    <div onMouseDown={(event) => event.stopPropagation()} onClick={(event) => event.stopPropagation()}>
      <DropdownMenu gutter={4} placement="bottom-end">
        <DropdownMenu.Trigger
          as={IconButton}
          icon="dot-grid"
          variant="ghost"
          size="small"
          className="size-6 rounded-md"
          aria-label={props.labels.moreLabel}
        />
        <DropdownMenu.Portal>
          <DropdownMenu.Content>
            <DropdownMenu.Item onSelect={props.onEdit}>
              <DropdownMenu.ItemLabel>{props.labels.editLabel}</DropdownMenu.ItemLabel>
            </DropdownMenu.Item>
            <DropdownMenu.Item onSelect={props.onDelete}>
              <DropdownMenu.ItemLabel>{props.labels.deleteLabel}</DropdownMenu.ItemLabel>
            </DropdownMenu.Item>
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu>
    </div>
  )
}

function diffId(file: string): string | undefined {
  const sum = checksum(file)
  if (!sum) return
  return `session-review-diff-${sum}`
}

type SessionReviewSelection = {
  file: string
  range: SelectedLineRange
}

function ReviewDiffItem(props: {
  diff: Item
  open: string[]
  visible: Record<string, boolean>
  force: Record<string, boolean>
  selection: SessionReviewSelection | null
  commenting: SessionReviewSelection | null
  opened: SessionReviewFocus | null
  focusedFile?: string
  focusedComment?: SessionReviewFocus | null
  onFocusedCommentChange?: (focus: SessionReviewFocus | null) => void
  grouped: Map<string, SessionReviewComment[]>
  diffStyle: SessionReviewDiffStyle
  fileComponent: React.ComponentType<any>
  i18n: ReturnType<typeof useI18n>
  pinned: (file: string) => boolean
  onOpenChange: (next: string[]) => void
  onLineComment?: (comment: SessionReviewLineComment) => void
  onLineCommentUpdate?: (comment: SessionReviewCommentUpdate) => void
  onLineCommentDelete?: (comment: SessionReviewCommentDelete) => void
  lineCommentActions?: SessionReviewCommentActions
  lineCommentMention?: LineCommentEditorProps["mention"]
  onDiffRendered?: VoidFunction
  onViewFile?: (file: string) => void
  openFileLabel: string
  readFile?: (path: string) => Promise<FileContent | undefined>
  setSelection: (val: SessionReviewSelection | null) => void
  setCommenting: (val: SessionReviewSelection | null) => void
  setOpened: (val: SessionReviewFocus | null) => void
  setForce: (fn: (prev: Record<string, boolean>) => Record<string, boolean>) => void
}) {
  const diff = props.diff
  const file = diff.file

  const diffCanRender = diff.additions !== 0 || diff.deletions !== 0

  const expanded = props.open.includes(file)
  const mounted = expanded && (!!props.visible[file] || props.pinned(file))
  const force = !!props.force[file]

  const comments = props.grouped.get(file) ?? []
  const commentedLines = comments.map((c) => c.selection)

  const beforeText = text(diff, "deletions")
  const afterText = text(diff, "additions")
  const changedLines = diff.additions + diff.deletions
  const mediaKind = mediaKindFromPath(file)

  const tooLarge = !expanded ? false : force ? false : mediaKind ? false : changedLines > MAX_DIFF_CHANGED_LINES

  const isAdded = diff.status === "added" || (beforeText.length === 0 && afterText.length > 0)
  const isDeleted = diff.status === "deleted" || (afterText.length === 0 && beforeText.length > 0)

  const selectedLines = props.selection?.file === file ? props.selection.range : null
  const draftRange = props.commenting?.file === file ? props.commenting.range : null

  const anchorsRef = useRef(new Map<string, HTMLElement>())
  const nodesRef = useRef(new Map<string, HTMLDivElement>())

  const commentsUi = createLineCommentController<SessionReviewComment>({
    comments,
    label: props.i18n.t("ui.lineComment.submit"),
    draftKey: () => file,
    mention: props.lineCommentMention,
    state: {
      opened: () => {
        const current = props.opened
        if (!current || current.file !== file) return null
        return current.id
      },
      setOpened: (id) => props.setOpened(id ? { file, id } : null),
      selected: selectedLines,
      setSelected: (range) => props.setSelection(range ? { file, range } : null),
      commenting: draftRange,
      setCommenting: (range) => props.setCommenting(range ? { file, range } : null),
    },
    getSide: (range: SelectedLineRange) => range.endSide ?? range.side ?? "additions",
    clearSelectionOnSelectionEndNull: false,
    onSubmit: ({ comment, selection }) => {
      const contents = text(diff, (selection.endSide ?? selection.side ?? "additions") as "additions" | "deletions")
      const preview = contents.length === 0 ? undefined : previewSelectedLines(contents, selection)
      props.onLineComment?.({ file, selection, comment, preview })
    },
    onUpdate: ({ id, comment, selection }) => {
      const contents = text(diff, (selection.endSide ?? selection.side ?? "additions") as "additions" | "deletions")
      const preview = contents.length === 0 ? undefined : previewSelectedLines(contents, selection)
      props.onLineCommentUpdate?.({ id, file, selection, comment, preview })
    },
    onDelete: (comment) => {
      props.onLineCommentDelete?.({ id: comment.id, file })
    },
    editSubmitLabel: props.lineCommentActions?.saveLabel,
    renderCommentActions: props.lineCommentActions
      ? (comment, controls) => (
          <ReviewCommentMenu
            labels={props.lineCommentActions!}
            onEdit={controls.edit}
            onDelete={controls.remove}
          />
        )
      : undefined,
  })

  useEffect(() => {
    return () => {
      anchorsRef.current.delete(file)
      nodesRef.current.delete(file)
    }
  }, [])

  const handleLineSelected = (range: SelectedLineRange | null) => {
    if (!props.onLineComment) return
    commentsUi.onLineSelected(range)
  }

  const handleLineSelectionEnd = (range: SelectedLineRange | null) => {
    if (!props.onLineComment) return
    commentsUi.onLineSelectionEnd(range)
  }

  const FileComponent = props.fileComponent as React.ComponentType<any>

  return (
    <Accordion.Item
      value={diffCanRender ? file : null!}
      id={diffId(file)}
      data-file={file}
      data-slot="session-review-accordion-item"
      data-selected={props.focusedFile === file ? "" : undefined}
    >
      <StickyAccordionHeader>
        <Accordion.Trigger disabled={!diffCanRender} className="cursor-default">
          <div data-slot="session-review-trigger-content">
            <div data-slot="session-review-file-info">
              <FileIcon node={{ path: file, type: "file" }} />
              <div data-slot="session-review-file-name-container">
                {file.includes("/") && (
                  <span data-slot="session-review-directory">{`\u202A${getDirectory(file)}\u202C`}</span>
                )}
                <span data-slot="session-review-filename">{getFilename(file)}</span>
                {props.onViewFile && diffCanRender && (
                  <Tooltip value={props.openFileLabel} placement="top" gutter={4}>
                    <button
                      data-slot="session-review-view-button"
                      type="button"
                      aria-label={props.openFileLabel}
                      onClick={(e) => {
                        e.stopPropagation()
                        props.onViewFile?.(file)
                      }}
                    >
                      <Icon name="open-file" size="small" />
                    </button>
                  </Tooltip>
                )}
              </div>
            </div>
            <div data-slot="session-review-trigger-actions">
              {isAdded ? (
                <div data-slot="session-review-change-group" data-type="added">
                  <span data-slot="session-review-change" data-type="added">
                    {props.i18n.t("ui.sessionReview.change.added")}
                  </span>
                  <DiffChanges changes={diff} />
                </div>
              ) : isDeleted ? (
                <span data-slot="session-review-change" data-type="removed">
                  {props.i18n.t("ui.sessionReview.change.removed")}
                </span>
              ) : !!mediaKind ? (
                <span data-slot="session-review-change" data-type="modified">
                  {props.i18n.t("ui.sessionReview.change.modified")}
                </span>
              ) : (
                <DiffChanges changes={diff} />
              )}
              {diffCanRender && (
                <span data-slot="session-review-diff-chevron">
                  <Icon name="chevron-down" size="small" />
                </span>
              )}
            </div>
          </div>
        </Accordion.Trigger>
      </StickyAccordionHeader>
      <Accordion.Content data-slot="session-review-accordion-content">
        <div
          data-slot="session-review-diff-wrapper"
          ref={(el) => {
            anchorsRef.current.set(file, el)
            nodesRef.current.set(file, el)
          }}
        >
          {expanded && (
            <>
              {!mounted && !tooLarge ? (
                <div
                  data-slot="session-review-diff-placeholder"
                  className="rounded-lg border border-border-weak-base bg-background-stronger/40"
                  style={{ height: "160px" }}
                />
              ) : tooLarge ? (
                <div data-slot="session-review-large-diff">
                  <div data-slot="session-review-large-diff-title">
                    {props.i18n.t("ui.sessionReview.largeDiff.title")}
                  </div>
                  <div data-slot="session-review-large-diff-meta">
                    {props.i18n.t("ui.sessionReview.largeDiff.meta", {
                      limit: MAX_DIFF_CHANGED_LINES.toLocaleString(),
                      current: changedLines.toLocaleString(),
                    })}
                  </div>
                  <div data-slot="session-review-large-diff-actions">
                    <Button
                      size="normal"
                      variant="secondary"
                      onClick={() => props.setForce((prev) => ({ ...prev, [file]: true }))}
                    >
                      {props.i18n.t("ui.sessionReview.largeDiff.renderAnyway")}
                    </Button>
                  </div>
                </div>
              ) : (
                <FileComponent
                  mode="diff"
                  fileDiff={diff.fileDiff}
                  preloadedDiff={diff.preloaded}
                  diffStyle={props.diffStyle}
                  onRendered={() => {
                    props.onDiffRendered?.()
                  }}
                  enableLineSelection={props.onLineComment != null}
                  enableHoverUtility={props.onLineComment != null}
                  onLineSelected={handleLineSelected}
                  onLineSelectionEnd={handleLineSelectionEnd}
                  onLineNumberSelectionEnd={commentsUi.onLineNumberSelectionEnd}
                  annotations={commentsUi.annotations()}
                  renderAnnotation={commentsUi.renderAnnotation}
                  renderHoverUtility={props.onLineComment ? commentsUi.renderHoverUtility : undefined}
                  selectedLines={selectedLines}
                  commentedLines={commentedLines}
                  media={{
                    mode: "auto",
                    path: file,
                    deleted: diff.status === "deleted",
                    readFile: diff.status === "deleted" ? undefined : props.readFile,
                  }}
                />
              )}
            </>
          )}
        </div>
      </Accordion.Content>
    </Accordion.Item>
  )
}

export const SessionReview = (props: SessionReviewProps) => {
  let scroll: HTMLDivElement | undefined
  let focusToken = 0
  let frame: number | undefined
  const i18n = useI18n()
  const fileComponent = useFileComponent()
  const anchors = new Map<string, HTMLElement>()
  const nodes = new Map<string, HTMLDivElement>()
  const [open, setOpen] = useState<string[]>([])
  const [visible, setVisible] = useState<Record<string, boolean>>({})
  const [force, setForce] = useState<Record<string, boolean>>({})
  const [selection, setSelection] = useState<SessionReviewSelection | null>(null)
  const [commenting, setCommenting] = useState<SessionReviewSelection | null>(null)
  const [opened, setOpened] = useState<SessionReviewFocus | null>(null)

  const resolvedOpen = props.open ?? open

  const items = useMemo<Item[]>(
    () => list(props.diffs).map((diff) => ({ ...normalize(diff), preloaded: diff.preloaded })),
    [props.diffs],
  )
  const files = useMemo(() => items.map((diff) => diff.file), [items])
  const grouped = useMemo(() => {
    const next = new Map<string, SessionReviewComment[]>()
    for (const comment of props.comments ?? []) {
      const list = next.get(comment.file)
      if (list) {
        list.push(comment)
        continue
      }
      next.set(comment.file, [comment])
    }
    return next
  }, [props.comments])

  const diffStyle = props.diffStyle ?? (props.split ? "split" : "unified")
  const hasDiffs = files.length > 0

  const syncVisible = () => {
    frame = undefined
    if (!scroll) return

    const root = scroll.getBoundingClientRect()
    const top = root.top - REVIEW_MOUNT_MARGIN
    const bottom = root.bottom + REVIEW_MOUNT_MARGIN
    const openSet = new Set(resolvedOpen)
    const next: Record<string, boolean> = {}

    for (const [file, el] of nodes) {
      if (!openSet.has(file)) continue
      const rect = el.getBoundingClientRect()
      if (rect.bottom < top || rect.top > bottom) continue
      next[file] = true
    }

    const prev = visible
    const prevKeys = Object.keys(prev)
    const nextKeys = Object.keys(next)
    if (prevKeys.length === nextKeys.length && nextKeys.every((file) => prev[file])) return
    setVisible(next)
  }

  const visibleRef = useRef(visible)
  visibleRef.current = visible

  const queue = () => {
    if (frame !== undefined) return
    frame = requestAnimationFrame(() => {
      syncVisible()
    })
  }

  const pinned = (file: string) =>
    props.focusedComment?.file === file ||
    props.focusedFile === file ||
    selection?.file === file ||
    commenting?.file === file ||
    opened?.file === file

  const handleScroll: React.UIEventHandler<HTMLDivElement> = (event) => {
    queue()
    props.onScroll?.(event)
  }

  useEffect(() => {
    return () => {
      if (frame === undefined) return
      cancelAnimationFrame(frame)
    }
  }, [])

  useEffect(() => {
    queue()
  }, [props.open, files])

  const handleChange = (next: string[]) => {
    props.onOpenChange?.(next)
    if (props.open === undefined) setOpen(next)
    queue()
  }

  const handleExpandOrCollapseAll = () => {
    const next = resolvedOpen.length > 0 ? [] : files
    handleChange(next)
  }

  const openFileLabel = i18n.t("ui.sessionReview.openFile")

  useEffect(() => {
    const focus = props.focusedComment
    if (!focus) return

    focusToken++
    const token = focusToken

    setOpened(focus)

    const comment = (props.comments ?? []).find((c) => c.file === focus.file && c.id === focus.id)
    if (comment) setSelection({ file: comment.file, range: cloneSelectedLineRange(comment.selection) })

    const current = resolvedOpen
    if (!current.includes(focus.file)) {
      handleChange([...current, focus.file])
    }

    const scrollTo = (attempt: number) => {
      if (token !== focusToken) return

      const root = scroll
      if (!root) return

      const wrapper = anchors.get(focus.file)
      const anchor = wrapper?.querySelector(`[data-comment-id="${focus.id}"]`)
      const ready =
        anchor instanceof HTMLElement && anchor.style.pointerEvents !== "none" && anchor.style.opacity !== "0"

      const target = ready ? anchor : wrapper
      if (!target) {
        if (attempt >= 120) return
        requestAnimationFrame(() => scrollTo(attempt + 1))
        return
      }

      const rootRect = root.getBoundingClientRect()
      const targetRect = target.getBoundingClientRect()
      const offset = targetRect.top - rootRect.top
      const next = root.scrollTop + offset - rootRect.height / 2 + targetRect.height / 2
      root.scrollTop = Math.max(0, next)

      if (ready) return
      if (attempt >= 120) return
      requestAnimationFrame(() => scrollTo(attempt + 1))
    }

    requestAnimationFrame(() => scrollTo(0))

    requestAnimationFrame(() => props.onFocusedCommentChange?.(null))
  }, [props.focusedComment])

  return (
    <div data-component="session-review" className={props.className}>
      <div data-slot="session-review-header" className={props.classes?.header}>
        <div data-slot="session-review-title">
          {props.title === undefined ? i18n.t("ui.sessionReview.title") : props.title}
        </div>
        <div data-slot="session-review-actions">
          {hasDiffs && props.onDiffStyleChange && (
            <RadioGroup
              options={["unified", "split"] as const}
              current={diffStyle}
              size="small"
              value={(style) => style}
              label={(style) =>
                i18n.t(style === "unified" ? "ui.sessionReview.diffStyle.unified" : "ui.sessionReview.diffStyle.split")
              }
              onSelect={(style) => style && props.onDiffStyleChange?.(style)}
            />
          )}
          {hasDiffs && (
            <Button
              size="small"
              icon="chevron-grabber-vertical"
              className="w-[106px] justify-start"
              onClick={handleExpandOrCollapseAll}
            >
              {resolvedOpen.length > 0
                ? i18n.t("ui.sessionReview.collapseAll")
                : i18n.t("ui.sessionReview.expandAll")}
            </Button>
          )}
          {props.actions}
        </div>
      </div>

      <ScrollView
        data-slot="session-review-scroll"
        viewportRef={(el) => {
          scroll = el
          props.scrollRef?.(el)
          queue()
        }}
        onScroll={handleScroll}
        className={props.classes?.root}
      >
        <div data-slot="session-review-container" className={props.classes?.container}>
          {hasDiffs ? (
            <div className="pb-6">
              <Accordion multiple value={resolvedOpen} onChange={handleChange}>
                {items.map((diff) => (
                  <ReviewDiffItem
                    key={diff.file}
                    diff={diff}
                    open={resolvedOpen}
                    visible={visible}
                    force={force}
                    selection={selection}
                    commenting={commenting}
                    opened={opened}
                    focusedFile={props.focusedFile}
                    focusedComment={props.focusedComment}
                    onFocusedCommentChange={props.onFocusedCommentChange}
                    grouped={grouped}
                    diffStyle={diffStyle}
                    fileComponent={fileComponent}
                    i18n={i18n}
                    pinned={pinned}
                    onOpenChange={handleChange}
                    onLineComment={props.onLineComment}
                    onLineCommentUpdate={props.onLineCommentUpdate}
                    onLineCommentDelete={props.onLineCommentDelete}
                    lineCommentActions={props.lineCommentActions}
                    lineCommentMention={props.lineCommentMention}
                    onDiffRendered={props.onDiffRendered}
                    onViewFile={props.onViewFile}
                    openFileLabel={openFileLabel}
                    readFile={props.readFile}
                    setSelection={setSelection}
                    setCommenting={setCommenting}
                    setOpened={setOpened}
                    setForce={setForce}
                  />
                ))}
              </Accordion>
            </div>
          ) : (
            props.empty
          )}
        </div>
      </ScrollView>
    </div>
  )
}
