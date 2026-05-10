import type { FileContent } from "@shob/sdk/v2"
import { useEffect, useMemo, useState } from "react"
import { useI18n } from "../context/i18n"
import {
  dataUrlFromMediaValue,
  hasMediaValue,
  isBinaryContent,
  mediaKindFromPath,
  normalizeMimeType,
  svgTextFromValue,
} from "../pierre/media"

export type FileMediaOptions = {
  mode?: "auto" | "off"
  path?: string
  current?: unknown
  before?: unknown
  after?: unknown
  deleted?: boolean
  readFile?: (path: string) => Promise<FileContent | undefined>
  onLoad?: () => void
  onError?: (ctx: { kind: "image" | "audio" | "svg" }) => void
}

function mediaValue(cfg: FileMediaOptions, mode: "image" | "audio") {
  if (cfg.current !== undefined) return cfg.current
  if (mode === "image") return cfg.after ?? cfg.before
  return cfg.after ?? cfg.before
}

type LoadedValue = { key: string; error: true } | { key: string; src: string; mime?: string }

export function FileMedia(props: { media?: FileMediaOptions; fallback: () => React.ReactNode }) {
  const i18n = useI18n()

  const kind = useMemo(() => {
    if (!props.media || props.media.mode === "off") return undefined
    return mediaKindFromPath(props.media.path)
  }, [props.media])

  const isBinary = useMemo(() => {
    if (!props.media || props.media.mode === "off") return false
    if (kind) return false
    return isBinaryContent(props.media.current as any)
  }, [props.media, kind])

  const onLoad = () => props.media?.onLoad?.()

  const deleted = useMemo(() => {
    if (!props.media || !kind) return false
    if (props.media.deleted) return true
    if (kind === "svg") return false
    if (props.media.current !== undefined) return false
    return !hasMediaValue(props.media.after as any) && hasMediaValue(props.media.before as any)
  }, [props.media, kind])

  const direct = useMemo(() => {
    if (!props.media || (kind !== "image" && kind !== "audio")) return undefined
    return dataUrlFromMediaValue(mediaValue(props.media, kind), kind)
  }, [props.media, kind])

  const request = useMemo(() => {
    if (!props.media || (kind !== "image" && kind !== "audio")) return undefined
    if (props.media.current !== undefined) return undefined
    if (deleted) return undefined
    if (direct) return undefined
    if (!props.media.path || !props.media.readFile) return undefined

    return {
      key: `${kind}:${props.media.path}`,
      kind,
      path: props.media.path,
      readFile: props.media.readFile,
      onError: props.media.onError,
    }
  }, [props.media, kind, deleted, direct])

  const [loaded, setLoaded] = useState<LoadedValue | undefined>(undefined)
  const [loading, setLoading] = useState(false)

  useEffect(() => {
    if (!request) {
      setLoaded(undefined)
      setLoading(false)
      return
    }

    setLoading(true)
    let cancelled = false

    request.readFile(request.path).then(
      (result) => {
        if (cancelled) return
        const src = dataUrlFromMediaValue(result as any, request.kind)
        if (!src) {
          request.onError?.({ kind: request.kind })
          setLoaded({ key: request.key, error: true })
        } else {
          setLoaded({
            key: request.key,
            src,
            mime: request.kind === "audio" ? normalizeMimeType(result?.mimeType) : undefined,
          })
        }
        setLoading(false)
      },
      () => {
        if (cancelled) return
        request.onError?.({ kind: request.kind })
        setLoaded({ key: request.key, error: true })
        setLoading(false)
      },
    )

    return () => {
      cancelled = true
    }
  }, [request])

  const remote = useMemo(() => {
    if (!request || !loaded || loaded.key !== request.key) return undefined
    return loaded
  }, [request, loaded])

  const src = useMemo(() => {
    return direct ?? (remote && "src" in remote ? remote.src : undefined)
  }, [direct, remote])

  const status = useMemo((): "ready" | "idle" | "loading" | "error" => {
    if (direct) return "ready"
    if (!request) return "idle"
    if (loading) return "loading"
    if (remote?.error) return "error"
    if (src) return "ready"
    return "idle"
  }, [direct, request, loading, remote, src])

  const audioMime = useMemo(() => {
    return remote && "mime" in remote ? remote.mime : undefined
  }, [remote])

  const svgSource = useMemo(() => {
    if (!props.media || kind !== "svg") return undefined
    return svgTextFromValue(props.media.current as any)
  }, [props.media, kind])

  const svgSrc = useMemo(() => {
    if (!props.media || kind !== "svg") return undefined
    return dataUrlFromMediaValue(props.media.current as any, "svg")
  }, [props.media, kind])

  const svgInvalid = useMemo(() => {
    if (!props.media || kind !== "svg") return undefined
    if (svgSource !== undefined) return undefined
    if (!hasMediaValue(props.media.current as any)) return undefined
    return [props.media.path, props.media.current] as const
  }, [props.media, kind, svgSource])

  useEffect(() => {
    if (!svgInvalid) return
    props.media?.onError?.({ kind: "svg" })
  }, [svgInvalid, props.media])

  const kindLabel = (value: "image" | "audio") =>
    i18n.t(value === "image" ? "ui.fileMedia.kind.image" : "ui.fileMedia.kind.audio")

  return (
    <>
      {(kind === "image" || kind === "audio") ? (
        src ? (
          (() => {
            if (kind !== "image" && kind !== "audio") return props.fallback()
            if (kind === "image") {
              return (
                <div className="flex justify-center bg-background-stronger px-6 py-4">
                  <img
                    src={src}
                    alt={props.media?.path}
                    className="max-h-[60vh] max-w-full rounded border border-border-weak-base bg-background-base object-contain"
                    onLoad={onLoad}
                  />
                </div>
              )
            }
            return (
              <div className="flex justify-center bg-background-stronger px-6 py-4">
                <audio className="w-full max-w-xl" controls preload="metadata" onLoadedMetadata={onLoad}>
                  <source src={src} type={audioMime} />
                </audio>
              </div>
            )
          })()
        ) : (
          (() => {
            const label = kindLabel(kind!)
            if (deleted) {
              return (
                <div className="flex min-h-40 items-center justify-center px-6 py-4 text-center text-text-weak">
                  {i18n.t("ui.fileMedia.state.removed", { kind: label })}
                </div>
              )
            }
            if (status === "loading") {
              return (
                <div className="flex min-h-40 items-center justify-center px-6 py-4 text-center text-text-weak">
                  {i18n.t("ui.fileMedia.state.loading", { kind: label })}
                </div>
              )
            }
            if (status === "error") {
              return (
                <div className="flex min-h-40 items-center justify-center px-6 py-4 text-center text-text-weak">
                  {i18n.t("ui.fileMedia.state.error", { kind: label })}
                </div>
              )
            }
            return (
              <div className="flex min-h-40 items-center justify-center px-6 py-4 text-center text-text-weak">
                {i18n.t("ui.fileMedia.state.unavailable", { kind: label })}
              </div>
            )
          })()
        )
      ) : kind === "svg" ? (
        (() => {
          if (svgSource === undefined && svgSrc == null) return props.fallback()
          return (
            <div className="flex flex-col gap-4 px-6 py-4">
              {svgSource !== undefined && props.fallback()}
              {svgSrc && (
                <div className="flex justify-center">
                  <img
                    src={svgSrc}
                    alt={props.media?.path}
                    className="max-h-[60vh] max-w-full rounded border border-border-weak-base bg-background-base object-contain"
                    onLoad={onLoad}
                  />
                </div>
              )}
            </div>
          )
        })()
      ) : isBinary ? (
        <div className="flex min-h-56 flex-col items-center justify-center gap-2 px-6 py-10 text-center">
          <div className="text-14-semibold text-text-strong">
            {props.media?.path?.split("/").pop() ?? i18n.t("ui.fileMedia.binary.title")}
          </div>
          <div className="text-14-regular text-text-weak">
            {(() => {
              const path = props.media?.path
              if (!path) return i18n.t("ui.fileMedia.binary.description.default")
              return i18n.t("ui.fileMedia.binary.description.path", { path })
            })()}
          </div>
        </div>
      ) : (
        props.fallback()
      )}
    </>
  )
}
