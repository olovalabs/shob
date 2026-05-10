import React, { createContext, useContext, useState, useEffect, useRef, useCallback } from "react"
import * as DialogPrimitive from "@radix-ui/react-dialog"

type DialogElement = () => React.ReactNode

type Active = {
  id: string
  node: React.ReactNode
  onClose?: () => void
  closing: boolean
}

type DialogContextType = {
  active: Active | undefined
  show: (element: DialogElement, onClose?: () => void) => void
  close: () => void
}

const Context = createContext<DialogContextType | undefined>(undefined)

export function DialogProvider(props: React.PropsWithChildren) {
  const [active, setActive] = useState<Active | undefined>()
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>()
  const lock = useRef(false)

  useEffect(() => {
    return () => {
      if (timer.current) clearTimeout(timer.current)
    }
  }, [])

  const close = useCallback(() => {
    if (!active || lock.current) return
    lock.current = true
    active.onClose?.()
    
    setActive(prev => prev ? { ...prev, closing: true } : undefined)

    const id = active.id
    if (timer.current) clearTimeout(timer.current)

    timer.current = setTimeout(() => {
      timer.current = undefined
      setActive(prev => prev?.id === id ? undefined : prev)
      lock.current = false
    }, 100)
  }, [active])

  useEffect(() => {
    if (!active) return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        close()
        event.preventDefault()
        event.stopPropagation()
      }
    }
    window.addEventListener("keydown", onKeyDown, { capture: true })
    return () => window.removeEventListener("keydown", onKeyDown, { capture: true })
  }, [active, close])

  const show = useCallback((element: DialogElement, onClose?: () => void) => {
    if (timer.current) clearTimeout(timer.current)
    lock.current = false

    const id = Math.random().toString(36).slice(2)
    setActive({
      id,
      node: element(),
      onClose,
      closing: false
    })
  }, [])

  const ctx: DialogContextType = {
    active,
    show,
    close
  }

  return (
    <Context.Provider value={ctx}>
      {props.children}
      <div data-component="dialog-stack">
        {active && (
          <DialogPrimitive.Root
            modal
            open={!active.closing}
            onOpenChange={(open) => {
              if (!open) close()
            }}
          >
            <DialogPrimitive.Portal>
              <DialogPrimitive.Overlay data-component="dialog-overlay" onClick={close} />
              {active.node}
            </DialogPrimitive.Portal>
          </DialogPrimitive.Root>
        )}
      </div>
    </Context.Provider>
  )
}

export function useDialog() {
  const ctx = useContext(Context)
  if (!ctx) {
    throw new Error("useDialog must be used within a DialogProvider")
  }
  return ctx
}
