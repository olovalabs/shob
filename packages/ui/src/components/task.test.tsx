import { describe, expect, test } from "bun:test"
import * as React from "react"
import { Task } from "./task"

describe("Task component", () => {
  test("renders with default status", () => {
    const el = Task({})
    expect(el.type).toBe("div")
    expect(el.props["data-component"]).toBe("task")
    expect(el.props["data-status"]).toBe("pending")
  })

  test("renders with custom status", () => {
    const el = Task({ status: "running" })
    expect(el.props["data-status"]).toBe("running")
  })

  test("renders with progress", () => {
    const el = Task({ progress: 50 })
    expect(el.props["data-component"]).toBe("task")
  })

  test("renders children", () => {
    const el = Task({ children: "child content" })
    expect(el.props.children).toBe("child content")
  })

  test("Header renders correctly", () => {
    const el = Task.Header({})
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-header")
  })

  test("HeaderInfo renders correctly", () => {
    const el = Task.HeaderInfo({})
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-header-info")
  })

  test("Title renders correctly", () => {
    const el = Task.Title({}, "My Task")
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-title")
    expect(el.props.children).toBe("My Task")
  })

  test("Subtitle renders correctly", () => {
    const el = Task.Subtitle({}, "A description")
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-subtitle")
    expect(el.props.children).toBe("A description")
  })

  test("Status renders correctly", () => {
    const el = Task.Status({})
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-status")
  })

  test("Progress renders with value", () => {
    const el = Task.Progress({ value: 50 })
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-progress")
    const bar = el.props.children[0]
    expect(bar.props["data-slot"]).toBe("task-progress-bar")
    expect(bar.props.style.width).toBe("50%")
  })

  test("Progress clamps value above 100", () => {
    const el = Task.Progress({ value: 150 })
    const bar = el.props.children[0]
    expect(bar.props.style.width).toBe("100%")
  })

  test("Progress clamps value below 0", () => {
    const el = Task.Progress({ value: -10 })
    const bar = el.props.children[0]
    expect(bar.props.style.width).toBe("0%")
  })

  test("Content renders correctly", () => {
    const el = Task.Content({}, "task work output")
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-content")
    expect(el.props.children).toBe("task work output")
  })

  test("Actions renders correctly", () => {
    const el = Task.Actions({})
    expect(el.type).toBe("div")
    expect(el.props["data-slot"]).toBe("task-actions")
  })

  test("composes sub-components via JSX", () => {
    const el = (
      <Task status="running" progress={33}>
        <Task.Header>
          <Task.HeaderInfo>
            <Task.Title>Build project</Task.Title>
            <Task.Subtitle>Compiling TypeScript...</Task.Subtitle>
          </Task.HeaderInfo>
          <Task.Status>running</Task.Status>
        </Task.Header>
        <Task.Progress value={33} />
        <Task.Content>Building...</Task.Content>
        <Task.Actions>Cancel</Task.Actions>
      </Task>
    )
    expect(el.type).toBe("div")
    expect(el.props["data-status"]).toBe("running")
  })
})