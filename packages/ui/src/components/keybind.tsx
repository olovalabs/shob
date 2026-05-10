import type { ComponentProps, React.PropsWithChildren } from "react"

export interface KeybindProps extends React.PropsWithChildren {
  class?: string
  classList?: ComponentProps<"span">["classList"]
}

export function Keybind(props: KeybindProps) {
  return (
    <span
      data-component="keybind"
      classList={{
        ...props.classList,
        [props.class ?? ""]: !!props.class,
      }}
    >
      {props.children}
    </span>
  )
}
