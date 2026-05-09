import { useState, useRef, useEffect, useMemo } from "react"
import { Check, ChevronDown } from "lucide-react"

interface Todo {
  id: string
  content: string
  status: "pending" | "in_progress" | "completed" | "cancelled"
}

interface TodoDockProps {
  todos: Todo[]
  live: boolean
}

export function TodoDock({ todos, live }: TodoDockProps) {
  const [collapsed, setCollapsed] = useState(false)
  const [stuck, setStuck] = useState(false)
  const listRef = useRef<HTMLDivElement>(null)

  const done = useMemo(() => todos.filter((t) => t.status === "completed" || t.status === "cancelled").length, [todos])
  const total = todos.length
  const active = useMemo(
    () =>
      todos.find((t) => t.status === "in_progress") ??
      todos.find((t) => t.status === "pending") ??
      todos.filter((t) => t.status === "completed").at(-1) ??
      todos[0],
    [todos],
  )

  const visible = total > 0 && live

  if (!visible) return null

  return (
    <div className="mx-auto w-full max-w-[800px] 2xl:max-w-[1000px]">
      <div className="overflow-hidden rounded-xl border border-border/60 bg-card/90">
        <div
          className="flex cursor-pointer items-center gap-2 overflow-visible px-3 py-2 select-none"
          role="button"
          tabIndex={0}
          onClick={() => setCollapsed((v) => !v)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault()
              setCollapsed((v) => !v)
            }
          }}
        >
          <span className="shrink-0 text-[13px] font-medium text-foreground">
            {done}/{total} done
          </span>
          {collapsed && active && (
            <span className="min-w-0 flex-1 truncate text-[13px] text-muted-foreground">{active.content}</span>
          )}
          <div className="ml-auto">
            <ChevronDown
              className={`h-4 w-4 text-muted-foreground transition-transform duration-200 ${collapsed ? "" : "rotate-180"}`}
            />
          </div>
        </div>

        <div
          ref={listRef}
          className={`overflow-hidden transition-all duration-200 ease-out ${collapsed ? "max-h-0" : "max-h-48"}`}
          aria-hidden={collapsed}
        >
          <div
            className="flex flex-col gap-1.5 overflow-y-auto px-3 pb-3"
            style={{ overflowAnchor: "none" }}
            onScroll={(e) => setStuck(e.currentTarget.scrollTop > 0)}
          >
            {stuck && (
              <div className="pointer-events-none sticky -top-3 left-0 right-0 h-4" style={{ background: "linear-gradient(to bottom, var(--card), transparent)" }} />
            )}
            {todos.map((todo) => (
              <TodoItem key={todo.id} todo={todo} />
            ))}
          </div>
        </div>
      </div>
    </div>
  )
}

function TodoItem({ todo }: { todo: Todo }) {
  const isCompleted = todo.status === "completed" || todo.status === "cancelled"
  const isInProgress = todo.status === "in_progress"

  return (
    <div className="flex items-start gap-2 py-0.5">
      <div className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center">
        {isCompleted ? (
          <Check className="h-3.5 w-3.5 text-muted-foreground" strokeWidth={2.5} />
        ) : isInProgress ? (
          <span className="block h-2 w-2 rounded-full bg-accent-foreground/60 animate-pulse" />
        ) : (
          <span className="block h-3.5 w-3.5 rounded-sm border border-border/60" />
        )}
      </div>
      <span
        className={`min-w-0 break-words text-[13px] leading-relaxed ${
          isCompleted ? "text-muted-foreground line-through" : isInProgress ? "text-foreground" : "text-foreground/92"
        }`}
      >
        {todo.content}
      </span>
    </div>
  )
}
