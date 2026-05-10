import fuzzysort from "fuzzysort"
import { entries, flatMap, groupBy, map, pipe } from "remeda"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"

export interface FilteredListProps<T> {
  items: T[] | ((filter: string) => T[] | Promise<T[]>)
  key: (item: T) => string
  filterKeys?: string[]
  current?: T
  groupBy?: (x: T) => string
  sortBy?: (a: T, b: T) => number
  sortGroupsBy?: (a: { category: string; items: T[] }, b: { category: string; items: T[] }) => number
  onSelect?: (value: T | undefined, index: number) => void
  noInitialSelection?: boolean
}

export function useFilteredList<T>(props: FilteredListProps<T>) {
  const [filter, setFilter] = useState("")

  type Group = { category: string; items: T[] }
  const empty: Group[] = []

  const [grouped, setGrouped] = useState<{ latest: Group[]; loading: boolean }>({ latest: empty, loading: false })

  const load = useCallback(async (f: string) => {
    setGrouped(prev => ({ ...prev, loading: true }))
    const items = typeof props.items === "function" ? await Promise.resolve(props.items(f)) : props.items
    const query = f ?? ""
    const needle = query.toLowerCase()
    const all = (await Promise.resolve(items)) || []
    const result = pipe(
      all,
      (x) => {
        if (!needle) return x
        if (!props.filterKeys && Array.isArray(x) && x.every((e) => typeof e === "string")) {
          return fuzzysort.go(needle, x).map((x) => x.target) as T[]
        }
        return fuzzysort.go(needle, x, { keys: props.filterKeys! }).map((x) => x.obj)
      },
      groupBy((x) => (props.groupBy ? props.groupBy(x) : "")),
      entries(),
      map(([k, v]) => ({ category: k, items: props.sortBy ? v.sort(props.sortBy) : v })),
      (groups) => (props.sortGroupsBy ? groups.sort(props.sortGroupsBy) : groups),
    )
    setGrouped({ latest: result, loading: false })
  }, [props.items, props.filterKeys, props.groupBy, props.sortBy, props.sortGroupsBy])

  useEffect(() => {
    load(filter)
  }, [filter, load])

  const flat = useMemo(() => {
    return pipe(
      grouped.latest || [],
      flatMap((x) => x.items),
    )
  }, [grouped.latest])

  const flatRef = useRef(flat)
  flatRef.current = flat

  const initialActive = (() => {
    if (props.noInitialSelection) return ""
    if (props.current) return props.key(props.current)
    if (flat.length === 0) return ""
    return props.key(flat[0])
  })()

  const [active, setActive] = useState(initialActive)

  const reset = useCallback(() => {
    if (props.noInitialSelection) {
      setActive("")
      return
    }
    if (flat.length === 0) return
    setActive(props.key(flat[0]))
  }, [props.noInitialSelection, flat, props.key])

  useEffect(() => {
    reset()
  }, [grouped.latest])

  const onKeyDown = useCallback((event: KeyboardEvent) => {
    if (event.key === "Enter" && !event.isComposing) {
      event.preventDefault()
      const all = flatRef.current
      const selectedIndex = all.findIndex((x) => props.key(x) === active)
      const selected = all[selectedIndex]
      if (selected) props.onSelect?.(selected, selectedIndex)
    } else if (event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey) {
      if (event.key === "n" || event.key === "p") {
        event.preventDefault()
        const dir = event.key === "n" ? 1 : -1
        const all = flatRef.current
        if (all.length === 0) return
        const idx = all.findIndex((x) => props.key(x) === active)
        const next = ((idx + dir) % all.length + all.length) % all.length
        setActive(props.key(all[next]))
      }
    } else {
      if (event.altKey || event.metaKey) return
      const all = flatRef.current
      if (all.length === 0) return
      const idx = all.findIndex((x) => props.key(x) === active)
      if (event.key === "ArrowDown") {
        event.preventDefault()
        const next = (idx + 1) % all.length
        setActive(props.key(all[next]))
      } else if (event.key === "ArrowUp") {
        event.preventDefault()
        const next = ((idx - 1) % all.length + all.length) % all.length
        setActive(props.key(all[next]))
      }
    }
  }, [active, props.key, props.onSelect])

  const onInput = useCallback((value: string) => {
    setFilter(value)
  }, [])

  return {
    grouped,
    filter,
    flat,
    reset,
    refetch: () => load(filter),
    clear: () => setFilter(""),
    onKeyDown,
    onInput,
    active,
    setActive,
  }
}
