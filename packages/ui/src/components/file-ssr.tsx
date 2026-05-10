import { DIFFS_TAG_NAME, FileDiff, VirtualizedFileDiff } from "@pierre/diffs"
import { type PreloadFileDiffResult, type PreloadMultiFileDiffResult } from "@pierre/diffs/ssr"
import { useEffect } from "react"
import { useWorkerPool } from "../context/worker-pool"
import { createDefaultOptions, styleVariables } from "../pierre"
import { markCommentedDiffLines } from "../pierre/commented-lines"
import { fixDiffSelection } from "../pierre/diff-selection"
import {
  applyViewerScheme,
  clearReadyWatcher,
  createReadyWatcher,
  notifyShadowReady,
} from "../pierre/file-runtime"
import { acquireVirtualizer, virtualMetrics } from "../pierre/virtualizer"
import { File, type DiffFileProps, type FileProps } from "./file"

function cn(base: string | undefined, classList?: Record<string, boolean | undefined>): string | undefined {
  const classes = [...(base ? [base] : [])]
  if (classList) {
    for (const [key, val] of Object.entries(classList)) {
      if (val) classes.push(key)
    }
  }
  return classes.filter(Boolean).join(" ") || undefined
}

type DiffPreload<T> = PreloadMultiFileDiffResult<T> | PreloadFileDiffResult<T>

type SSRDiffFileProps<T> = DiffFileProps<T> & {
  preloadedDiff: DiffPreload<T>
}

function DiffSSRViewer<T>(props: SSRDiffFileProps<T>) {
  let container!: HTMLDivElement
  let fileDiffRef!: HTMLElement
  let fileDiffInstance: FileDiff<T> | undefined
  let sharedVirtualizer: NonNullable<ReturnType<typeof acquireVirtualizer>> | undefined

  const ready = createReadyWatcher()
  const workerPool = useWorkerPool(props.diffStyle)

  const { fileDiff, before, after, class: _class, classList, annotations, selectedLines, commentedLines, onLineSelected, onLineSelectionEnd, onLineNumberSelectionEnd, onRendered, preloadedDiff, ...others } = props

  const getRoot = () => fileDiffRef?.shadowRoot ?? undefined

  const getVirtualizer = () => {
    if (sharedVirtualizer) return sharedVirtualizer.virtualizer
    const result = acquireVirtualizer(container)
    if (!result) return
    sharedVirtualizer = result
    return result.virtualizer
  }

  const setSelectedLines = (range: DiffFileProps<T>["selectedLines"], attempt = 0) => {
    const diff = fileDiffInstance
    if (!diff) return

    const fixed = fixDiffSelection(getRoot(), range ?? null)
    if (fixed === undefined) {
      if (attempt >= 120) return
      requestAnimationFrame(() => setSelectedLines(range ?? null, attempt + 1))
      return
    }

    diff.setSelectedLines(fixed)
  }

  const notifyRendered = () => {
    notifyShadowReady({
      state: ready,
      container,
      getRoot,
      isReady: (root) => root.querySelector("[data-line]") != null,
      settleFrames: 1,
      onReady: () => {
        setSelectedLines(selectedLines ?? null)
        onRendered?.()
      },
    })
  }

  useEffect(() => {
    if (typeof window === "undefined") return

    const virtualizer = getVirtualizer()
    const annots = annotations ?? preloadedDiff.annotations ?? []
    fileDiffInstance = virtualizer
      ? new VirtualizedFileDiff<T>(
          {
            ...createDefaultOptions(props.diffStyle),
            ...others,
            ...preloadedDiff.options,
          },
          virtualizer,
          virtualMetrics,
          workerPool,
        )
      : new FileDiff<T>(
          {
            ...createDefaultOptions(props.diffStyle),
            ...others,
            ...preloadedDiff.options,
          },
          workerPool,
        )

    applyViewerScheme(fileDiffRef)

    // @ts-expect-error private field required for hydration
    fileDiffInstance.fileContainer = fileDiffRef
    fileDiffInstance.hydrate(
      fileDiff
        ? {
            fileDiff,
            lineAnnotations: annots,
            fileContainer: fileDiffRef,
            containerWrapper: container,
            prerenderedHTML: preloadedDiff.prerenderedHTML,
          }
        : {
            oldFile: before
              ? { ...before, contents: typeof before.contents === "string" ? before.contents : "" }
              : before,
            newFile: after
              ? { ...after, contents: typeof after.contents === "string" ? after.contents : "" }
              : after,
            lineAnnotations: annots,
            fileContainer: fileDiffRef,
            containerWrapper: container,
            prerenderedHTML: preloadedDiff.prerenderedHTML,
          },
    )

    notifyRendered()

    return () => {
      clearReadyWatcher(ready)
      fileDiffInstance?.cleanUp()
      sharedVirtualizer?.release()
      sharedVirtualizer = undefined
    }
  }, [])

  useEffect(() => {
    const diff = fileDiffInstance
    if (!diff) return
    diff.setLineAnnotations(annotations ?? [])
    diff.rerender()
  })

  useEffect(() => {
    setSelectedLines(selectedLines ?? null)
  })

  useEffect(() => {
    const ranges = commentedLines ?? []
    requestAnimationFrame(() => {
      const root = getRoot()
      if (!root) return
      markCommentedDiffLines(root, ranges)
    })
  })

  return (
    <div
      data-component="file"
      data-mode="diff"
      style={styleVariables}
      className={cn(_class, classList)}
      ref={(el) => { container = el }}
    >
      {(() => {
        const Tag = DIFFS_TAG_NAME as unknown as React.ElementType
        return <Tag ref={(el: HTMLElement) => { fileDiffRef = el }} id="ssr-diff">
          {typeof window === "undefined" && (
            <template shadowrootmode="open" dangerouslySetInnerHTML={{ __html: preloadedDiff.prerenderedHTML }} />
          )}
        </Tag>
      })()}
    </div>
  )
}

export type FileSSRProps<T = {}> = FileProps<T>

export function FileSSR<T>(props: FileSSRProps<T>) {
  if (props.mode !== "diff" || !props.preloadedDiff) return File(props)
  return DiffSSRViewer(props as SSRDiffFileProps<T>)
}
