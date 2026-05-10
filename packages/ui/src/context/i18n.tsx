import React, { createContext, useContext } from "react"
import { dict as en } from "../i18n/en"

export type UiI18nKey = keyof typeof en

export type UiI18nParams = Record<string, string | number | boolean>

export type UiI18n = {
  locale: string
  t: (key: UiI18nKey, params?: UiI18nParams) => string
}

function resolveTemplate(text: string, params?: UiI18nParams) {
  if (!params) return text
  return text.replace(/{{\s*([^}]+?)\s*}}/g, (_, rawKey) => {
    const key = String(rawKey)
    const value = params[key]
    return value === undefined ? "" : String(value)
  })
}

const fallback: UiI18n = {
  locale: "en",
  t: (key, params) => {
    const value = en[key] ?? String(key)
    return resolveTemplate(value, params)
  },
}

const Context = createContext<UiI18n>(fallback)

export function I18nProvider(props: React.PropsWithChildren<{ value: UiI18n }>) {
  return <Context.Provider value={props.value}>{props.children}</Context.Provider>
}

export function useI18n() {
  return useContext(Context)
}
