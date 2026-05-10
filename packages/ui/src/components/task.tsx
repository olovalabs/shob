import * as React from "react"
import { cn } from "../lib/utils"

export interface TaskProps extends React.ComponentProps<"div"> {
  status?: "pending" | "running" | "completed" | "cancelled"
  progress?: number
}

export interface TaskHeaderProps extends React.PropsWithChildren<React.ComponentProps<"div">> {}
export interface TaskHeaderInfoProps extends React.PropsWithChildren<React.ComponentProps<"div">> {}
export interface TaskTitleProps extends React.PropsWithChildren<React.ComponentProps<"div">> {}
export interface TaskSubtitleProps extends React.PropsWithChildren<React.ComponentProps<"div">> {}
export interface TaskStatusProps extends React.PropsWithChildren<React.ComponentProps<"div">> {}
export interface TaskProgressProps extends React.PropsWithChildren<React.ComponentProps<"div">> {
  value?: number
}
export interface TaskContentProps extends React.PropsWithChildren<React.ComponentProps<"div">> {}
export interface TaskActionsProps extends React.PropsWithChildren<React.ComponentProps<"div">> {}

function TaskRoot(props: TaskProps) {
  const { className, status = "pending", progress, children, ...rest } = props ?? {}
  return React.createElement(
    "div",
    {
      ...rest,
      "data-component": "task",
      "data-status": status,
      className: cn(className),
    },
    children,
  )
}

function TaskHeader(props: TaskHeaderProps) {
  const { className, children, ...rest } = props ?? {}
  return React.createElement("div", { ...rest, "data-slot": "task-header", className: cn(className) }, children)
}

function TaskHeaderInfo(props: TaskHeaderInfoProps) {
  const { className, children, ...rest } = props ?? {}
  return React.createElement("div", { ...rest, "data-slot": "task-header-info", className: cn(className) }, children)
}

function TaskTitle(props: TaskTitleProps) {
  const { className, children, ...rest } = props ?? {}
  return React.createElement("div", { ...rest, "data-slot": "task-title", className: cn(className) }, children)
}

function TaskSubtitle(props: TaskSubtitleProps) {
  const { className, children, ...rest } = props ?? {}
  return React.createElement("div", { ...rest, "data-slot": "task-subtitle", className: cn(className) }, children)
}

function TaskStatus(props: TaskStatusProps) {
  const { className, children, ...rest } = props ?? {}
  return React.createElement("div", { ...rest, "data-slot": "task-status", className: cn(className) }, children)
}

function TaskProgress(props: TaskProgressProps) {
  const { className, value = 0, children, ...rest } = props ?? {}
  return React.createElement(
    "div",
    { ...rest, "data-slot": "task-progress", className: cn(className) },
    React.createElement("div", { "data-slot": "task-progress-bar", style: { width: `${Math.min(100, Math.max(0, value))}%` } }),
    children,
  )
}

function TaskContent(props: TaskContentProps) {
  const { className, children, ...rest } = props ?? {}
  return React.createElement("div", { ...rest, "data-slot": "task-content", className: cn(className) }, children)
}

function TaskActions(props: TaskActionsProps) {
  const { className, children, ...rest } = props ?? {}
  return React.createElement("div", { ...rest, "data-slot": "task-actions", className: cn(className) }, children)
}

export const Task = Object.assign(TaskRoot, {
  Header: TaskHeader,
  HeaderInfo: TaskHeaderInfo,
  Title: TaskTitle,
  Subtitle: TaskSubtitle,
  Status: TaskStatus,
  Progress: TaskProgress,
  Content: TaskContent,
  Actions: TaskActions,
})