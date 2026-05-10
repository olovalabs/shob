import { type FilteredListProps, useFilteredList } from "@shob/ui/hooks"
import { useEffect, type JSX } from "react"
import { useI18n } from "../context/i18n"
import { Icon, type IconProps } from "./icon"
import { IconButton } from "./icon-button"
import { TextField } from "./text-field"
import { useRef, useState } from "react"

function findByKey(container: HTMLElement, key: string) {
  const nodes = container.querySelectorAll<HTMLElement>('[data-slot="list-item"][data-key]')
  for (const node of nodes) {
    if (node.getAttribute("data-key") === key) return node
  }
}

export interface ListSearchProps {
  placeholder?: string
  autofocus?: boolean
  hideIcon?: boolean
  class?: string
  action?: JSX.Element
}

export interface ListAddProps {
  class?: string
  render: () => JSX.Element
}

export interface ListProps<T> extends FilteredListProps<T> {
  class?: string
  children: (item: T) => JSX.Element
  emptyMessage?: string
  loadingMessage?: string
  onKeyEvent?: (event: KeyboardEvent, item: T | undefined) => void
  onMove?: (item: T | undefined) => void
  onFilter?: (value: string) => void
  activeIcon?: IconProps["name"]
  filter?: string
  search?: ListSearchProps | boolean
  itemWrapper?: (item: T, node: JSX.Element) => JSX.Element
  divider?: boolean
  add?: ListAddProps
  groupHeader?: (group: { category: string; items: T[] }) => JSX.Element
}

export interface ListRef {
  onKeyDown: (e: KeyboardEvent) => void
  setScrollRef: (el: HTMLDivElement | undefined) => void
  setFilter: (value: string) => void
}

export function List<T>(props: ListProps<T> & { ref?: (ref: ListRef) => void }) {
  const i18n = useI18n()
  let inputRef: HTMLInputElement | HTMLTextAreaElement | undefined
  const [mouseActive, setMouseActive] = useState(false)
  const scrollRef = useRef<HTMLDivElement>(null)
  const [internalFilter, setInternalFilter] = useState("")

  const scrollIntoView = (container: HTMLDivElement, node: HTMLElement, block: "center" | "nearest") => {
    const containerRect = container.getBoundingClientRect()
    const nodeRect = node.getBoundingClientRect()
    const top = nodeRect.top - containerRect.top + container.scrollTop
    const bottom = top + nodeRect.height
    const viewTop = container.scrollTop
    const viewBottom = viewTop + container.clientHeight
    const target =
      block === "center"
        ? top - container.clientHeight / 2 + nodeRect.height / 2
        : top < viewTop
          ? top
          : bottom > viewBottom
            ? bottom - container.clientHeight
            : viewTop
    const max = Math.max(0, container.scrollHeight - container.clientHeight)
    container.scrollTop = Math.max(0, Math.min(target, max))
  }

  const { filter, grouped, flat, active, setActive, onKeyDown, onInput, refetch } = useFilteredList<T>(props)

  const searchPropsVal = typeof props.search === "object" ? props.search : {}

  const applyFilter = (value: string, options?: { ref?: boolean }) => {
    const prev = filter
    setInternalFilter(value)
    onInput(value)
    props.onFilter?.(value)

    if (!options?.ref) return

    if (prev === value) {
      void refetch()
      return
    }
    queueMicrotask(() => refetch())
  }

  useEffect(() => {
    if (props.filter === undefined) return
    if (props.filter === internalFilter) return
    setInternalFilter(props.filter)
    onInput(props.filter)
  }, [props.filter])

  useEffect(() => {
    scrollRef.current?.scrollTo(0, 0)
  }, [filter])

  useEffect(() => {
    const scroll = scrollRef.current
    if (!scroll) return
    if (!props.current) return
    const key = props.key(props.current)
    requestAnimationFrame(() => {
      const element = findByKey(scroll, key)
      if (!element) return
      scrollIntoView(scroll, element, "center")
    })
  })

  useEffect(() => {
    if (mouseActive || flat.length === 0) return
    const scroll = scrollRef.current
    if (!scroll) return
    if (active === props.key(flat[0])) {
      scroll.scrollTo(0, 0)
      return
    }
    if (!active) return
    const element = findByKey(scroll, active)
    if (!element) return
    scrollIntoView(scroll, element, "center")
  })

  useEffect(() => {
    const current = active
    const item = flat.find((x) => props.key(x) === current)
    props.onMove?.(item)
  })

  const handleSelect = (item: T | undefined, index: number) => {
    props.onSelect?.(item, index)
  }

  const handleKey = (e: KeyboardEvent) => {
    setMouseActive(false)
    if (e.key === "Escape") return

    const selected = flat.find((x) => props.key(x) === active)
    const index = selected ? flat.indexOf(selected) : -1
    props.onKeyEvent?.(e, selected)

    if (e.defaultPrevented) return

    if (e.key === "Enter" && !e.isComposing) {
      e.preventDefault()
      if (selected) handleSelect(selected, index)
    } else if (props.search) {
      if (e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && (e.key === "n" || e.key === "p")) {
        onKeyDown(e)
        return
      }
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        onKeyDown(e)
      }
    } else {
      onKeyDown(e)
    }
  }

  const setScrollRef = (el: HTMLDivElement | undefined) => {
    scrollRef.current = el ?? null
  }

  props.ref?.({
    onKeyDown: handleKey,
    setScrollRef,
    setFilter: (value: string) => applyFilter(value, { ref: true }),
  })

  function GroupHeader(groupProps: { group: { category: string; items: T[] } }): JSX.Element {
    const [stuck, setStuck] = useState(false)
    const headerRef = useRef<HTMLDivElement>(null)

    useEffect(() => {
      const scroll = scrollRef.current
      const node = headerRef.current
      if (!scroll || !node) return

      const handler = () => {
        const rect = node.getBoundingClientRect()
        const scrollRect = scroll.getBoundingClientRect()
        setStuck(rect.top <= scrollRect.top + 1 && scroll.scrollTop > 0)
      }

      scroll.addEventListener("scroll", handler, { passive: true })
      handler()
      return () => scroll.removeEventListener("scroll", handler)
    }, [])

    return (
      <div data-slot="list-header" data-stuck={stuck} ref={headerRef}>
        {props.groupHeader?.(groupProps.group) ?? groupProps.group.category}
      </div>
    )
  }

  const emptyMessage = () => {
    if (grouped.loading) return props.loadingMessage ?? i18n.t("ui.list.loading")
    if (props.emptyMessage) return props.emptyMessage

    const query = filter
    if (!query) return i18n.t("ui.list.empty")

    const suffix = i18n.t("ui.list.emptyWithFilter.suffix")
    return (
      <>
        <span>{i18n.t("ui.list.emptyWithFilter.prefix")}</span>
        <span data-slot="list-filter">&quot;{query}&quot;</span>
        {suffix && <span>{suffix}</span>}
      </>
    )
  }

  const renderAdd = () => {
    const add = props.add
    if (!add) return null
    return (
      <div data-slot="list-item-add" className={add.class}>
        {add.render()}
      </div>
    )
  }

  return (
    <div data-component="list" className={props.class}>
      {!!props.search && (
        <div data-slot="list-search-wrapper">
          <div
            data-slot="list-search"
            className={searchPropsVal.class}
            onPointerDown={(event) => {
              const container = event.currentTarget
              if (!(container instanceof HTMLElement)) return

              const node = container.querySelector("input, textarea")
              const input = node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement ? node : inputRef
              input?.focus()
              event.stopPropagation()
            }}
          >
            <div data-slot="list-search-container">
              {!searchPropsVal.hideIcon && (
                <Icon name="magnifying-glass" />
              )}
              <TextField
                autofocus={searchPropsVal.autofocus}
                variant="ghost"
                data-slot="list-search-input"
                type="text"
                ref={(el: HTMLInputElement | HTMLTextAreaElement) => {
                  inputRef = el
                }}
                value={internalFilter}
                onChange={(value: string) => applyFilter(value)}
                onKeyDown={handleKey}
                placeholder={searchPropsVal.placeholder}
                spellcheck={false}
                autocorrect="off"
                autocomplete="off"
                autocapitalize="off"
              />
            </div>
            {internalFilter && (
              <IconButton
                icon="circle-x"
                variant="ghost"
                onClick={() => {
                  setInternalFilter("")
                  queueMicrotask(() => inputRef?.focus())
                }}
                aria-label={i18n.t("ui.list.clearFilter")}
              />
            )}
          </div>
          {searchPropsVal.action}
        </div>
      )}
      <div ref={setScrollRef} data-slot="list-scroll">
        {flat.length > 0 || !!props.add ? (
          <>
            {grouped.latest.map((group, groupIndex) => {
              const isLastGroup = groupIndex === grouped.latest.length - 1
              return (
                <div data-slot="list-group" key={group.category}>
                  {group.category && <GroupHeader group={group} />}
                  <div data-slot="list-items">
                    {group.items.map((item, i) => {
                      const node = (
                        <button
                          data-slot="list-item"
                          data-key={props.key(item)}
                          data-active={props.key(item) === active}
                          data-selected={item === props.current}
                          onClick={() => handleSelect(item, i)}
                          onKeyDown={handleKey}
                          type="button"
                          onMouseMove={(event) => {
                            const moved = event.movementX !== 0 || event.movementY !== 0
                            if (!moved) return
                            setMouseActive(true)
                            setActive(props.key(item))
                          }}
                          onMouseLeave={() => {
                            if (!mouseActive) return
                            setActive("")
                          }}
                          key={props.key(item)}
                        >
                          {props.children(item)}
                          {item === props.current && (
                            <span data-slot="list-item-selected-icon">
                              <Icon name="check-small" />
                            </span>
                          )}
                          {props.activeIcon && (
                            <span data-slot="list-item-active-icon">
                              <Icon name={props.activeIcon} />
                            </span>
                          )}
                          {props.divider && (
                            (i !== group.items.length - 1 || (!!props.add && isLastGroup)) && (
                              <span data-slot="list-item-divider" />
                            )
                          )}
                        </button>
                      )
                      if (props.itemWrapper) return props.itemWrapper(item, node)
                      return node
                    })}
                    {!!props.add && isLastGroup && renderAdd()}
                  </div>
                </div>
              )
            })}
            {grouped.latest.length === 0 && !!props.add && (
              <div data-slot="list-group">
                <div data-slot="list-items">{renderAdd()}</div>
              </div>
            )}
          </>
        ) : (
          <div data-slot="list-empty-state">
            <div data-slot="list-message">{emptyMessage()}</div>
          </div>
        )}
      </div>
    </div>
  )
}
