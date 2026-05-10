import React, { createContext, useContext, useMemo } from "react"

export function createSimpleContext<T, Props extends Record<string, any>>(input: {
  name: string
  init: ((input: Props) => T) | (() => T)
  gate?: boolean
}) {
  const ctx = createContext<T | undefined>(undefined)

  return {
    provider: (props: React.PropsWithChildren<Props>) => {
      const init = useMemo(() => input.init(props), [props])
      const gate = input.gate ?? true

      if (!gate) {
        return <ctx.Provider value={init}>{props.children}</ctx.Provider>
      }

      // @ts-expect-error
      const ready = init.ready as boolean | (() => boolean) | undefined
      const isReady = ready === undefined || (typeof ready === "function" ? ready() : ready)

      return isReady ? (
        <ctx.Provider value={init}>{props.children}</ctx.Provider>
      ) : null
    },
    use() {
      const value = useContext(ctx)
      if (!value) throw new Error(`${input.name} context must be used within a context provider`)
      return value
    },
  }
}

