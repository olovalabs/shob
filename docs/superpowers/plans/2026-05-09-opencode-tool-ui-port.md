# OpenCode Tool-Calling UI Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Shob's agent chat UI components with React ports of OpenCode's tool-calling UI, matching 100% in design, layout, behavior, animations, and interactions.

**Architecture:** Port each SolidJS component file to React preserving component boundaries, props, HTML data attributes, and CSS. Translate SolidJS signals/effects to React hooks, use the `motion` library for spring animations (same as OpenCode), and port CSS as-is via `index.css`.

**Tech Stack:** React 19, TypeScript, Tailwind CSS v4, `motion` (spring animations), shadcn/ui Collapsible

---

### Task 1: Install dependencies & setup CSS foundation

**Files:**
- Modify: `package.json`
- Create: `src/components/opencode/tools/tool-registry.ts`
- Modify: `src/index.css`

- [ ] **Step 1: Install `motion` package**

```bash
bun add motion
```

Expected: `motion` added to package.json

- [ ] **Step 2: Add OpenCode theme CSS custom properties to index.css**

Add these CSS custom properties to `:root, .dark` in `src/index.css` (after line 119):

```css
  /* ── OpenCode semantic tokens ───────────────────── */
  --text-strong: var(--foreground);
  --text-base: color-mix(in oklch, var(--foreground) 62%, transparent);
  --text-weak: color-mix(in oklch, var(--foreground) 42%, transparent);
  --text-weaker: color-mix(in oklch, var(--foreground) 28%, transparent);
  --text-invert-strong: #fcfcfc;
  --background-base: var(--background);
  --background-stronger: color-mix(in oklch, var(--card) 88%, var(--foreground) 12%);
  --surface-base: var(--card);
  --surface-raised-base-hover: color-mix(in oklch, var(--accent) 80%, var(--card) 20%);
  --border-base: var(--border);
  --border-weak-base: color-mix(in oklch, var(--border) 60%, transparent);
  --border-weaker-base: color-mix(in oklch, var(--border) 40%, transparent);
  --icon-base: var(--muted-foreground);
  --icon-weak: color-mix(in oklch, var(--muted-foreground) 60%, transparent);
  --icon-weaker: color-mix(in oklch, var(--muted-foreground) 40%, transparent);
  --icon-interactive-base: var(--ring);
  --icon-diff-add-base: #22c55e;
  --icon-diff-delete-base: #ef4444;
  --text-diff-add-base: #22c55e;
  --text-diff-delete-base: #ef4444;
  --surface-critical-base: color-mix(in oklch, #ef4444 15%, transparent);
  --text-on-critical-base: #ef4444;
  --radius-md: calc(var(--radius) * 0.8);

  /* Font tokens */
  --font-family-sans: var(--font-sans);
  --line-height-large: 1.4;
  --letter-spacing-normal: 0em;
  --font-weight-regular: 400;
  --font-weight-medium: 500;
```

- [ ] **Step 3: Create tool-registry.ts**

```typescript
import type { ToolCallView } from "@/components/AgentView"

export interface ToolProps {
  toolCall: ToolCallView
  className?: string
}

export interface ToolRenderer {
  name: string
  render: (props: ToolProps) => React.ReactNode
}

const registry = new Map<string, ToolRenderer>()

export const ToolRegistry = {
  register(renderer: ToolRenderer) {
    registry.set(renderer.name, renderer)
    return renderer
  },
  render(name: string, props: ToolProps): React.ReactNode {
    const r = registry.get(name)
    if (!r) return null
    return r.render(props)
  },
  get(name: string): ToolRenderer | undefined {
    return registry.get(name)
  },
  has(name: string): boolean {
    return registry.has(name)
  },
}
```

- [ ] **Step 4: Commit**

```bash
git add package.json bun.lock src/index.css src/components/opencode/tools/tool-registry.ts
git commit -m "feat: install motion, add OC theme tokens, create ToolRegistry"
```

---

### Task 2: Create TextShimmer component

**Files:**
- Create: `src/components/opencode/tools/text-shimmer.tsx`

- [ ] **Step 1: Write TextShimmer**

```tsx
import { useEffect, useRef, useState } from "react"

export function TextShimmer({ text, active = true, offset = 0, className }: {
  text: string
  active?: boolean
  offset?: number
  className?: string
}) {
  const swap = 220
  const [run, setRun] = useState(active)
  const timerRef = useRef<ReturnType<typeof setTimeout>>()

  useEffect(() => {
    if (timerRef.current) clearTimeout(timerRef.current)
    if (active) {
      setRun(true)
      return
    }
    timerRef.current = setTimeout(() => setRun(false), swap)
    return () => { if (timerRef.current) clearTimeout(timerRef.current) }
  }, [active])

  return (
    <span
      data-component="text-shimmer"
      data-active={active ? "true" : "false"}
      className={className}
      aria-label={text}
      style={{
        "--text-shimmer-swap": `${swap}ms`,
        "--text-shimmer-index": `${offset}`,
      } as React.CSSProperties}
    >
      <span data-slot="text-shimmer-char">
        <span data-slot="text-shimmer-char-base" aria-hidden="true">{text}</span>
        <span data-slot="text-shimmer-char-shimmer" data-run={run ? "true" : "false"} aria-hidden="true">{text}</span>
      </span>
    </span>
  )
}
```

- [ ] **Step 2: Add TextShimmer CSS to index.css**

Add before the agent-* section (before line 730):

```css
/* ═══════════════════════════════════════════════════
   TEXT SHIMMER
   ═══════════════════════════════════════════════════ */
[data-component="text-shimmer"] {
  --text-shimmer-step: 45ms;
  --text-shimmer-duration: 1200ms;
  --text-shimmer-swap: 220ms;
  --text-shimmer-index: 0;
  --text-shimmer-angle: 90deg;
  --text-shimmer-spread: 5.2ch;
  --text-shimmer-size: 360%;
  --text-shimmer-base-color: var(--text-weak);
  --text-shimmer-peak-color: var(--text-strong);
  --text-shimmer-sweep: linear-gradient(
    var(--text-shimmer-angle),
    transparent calc(50% - var(--text-shimmer-spread)),
    var(--text-shimmer-peak-color) 50%,
    transparent calc(50% + var(--text-shimmer-spread))
  );
  --text-shimmer-base: linear-gradient(var(--text-shimmer-base-color), var(--text-shimmer-base-color));
  display: inline-flex;
  align-items: baseline;
  font: inherit;
  letter-spacing: inherit;
  line-height: inherit;
}

[data-component="text-shimmer"] [data-slot="text-shimmer-char"] {
  display: inline-grid;
  white-space: pre;
  font: inherit;
  letter-spacing: inherit;
  line-height: inherit;
}

[data-component="text-shimmer"] [data-slot="text-shimmer-char-base"],
[data-component="text-shimmer"] [data-slot="text-shimmer-char-shimmer"] {
  grid-area: 1 / 1;
  white-space: pre;
  transition: opacity var(--text-shimmer-swap) ease-out;
  font: inherit;
  letter-spacing: inherit;
  line-height: inherit;
}

[data-component="text-shimmer"] [data-slot="text-shimmer-char-base"] {
  color: inherit;
  opacity: 1;
}

[data-component="text-shimmer"] [data-slot="text-shimmer-char-shimmer"] {
  color: var(--text-weaker);
  opacity: 0;
}

[data-component="text-shimmer"][data-active="true"] [data-slot="text-shimmer-char-shimmer"] {
  opacity: 1;
}

[data-component="text-shimmer"] [data-slot="text-shimmer-char-shimmer"][data-run="true"] {
  animation-name: text-shimmer-sweep;
  animation-duration: var(--text-shimmer-duration);
  animation-iteration-count: infinite;
  animation-timing-function: linear;
  animation-fill-mode: both;
  animation-delay: calc(var(--text-shimmer-step) * var(--text-shimmer-index) * -1);
  will-change: background-position;
}

@keyframes text-shimmer-sweep {
  0% { background-position: 100% 0, 0 0; }
  100% { background-position: 0% 0, 0 0; }
}

@supports ((-webkit-background-clip: text) or (background-clip: text)) {
  [data-component="text-shimmer"] [data-slot="text-shimmer-char-shimmer"] {
    color: transparent;
    -webkit-text-fill-color: transparent;
    background-image: var(--text-shimmer-sweep), var(--text-shimmer-base);
    background-size: var(--text-shimmer-size) 100%, 100% 100%;
    background-position: 100% 0, 0 0;
    background-repeat: no-repeat;
    -webkit-background-clip: text;
    background-clip: text;
  }
  [data-component="text-shimmer"][data-active="true"] [data-slot="text-shimmer-char-base"] {
    opacity: 0;
  }
}

@media (prefers-reduced-motion: reduce) {
  [data-component="text-shimmer"] [data-slot="text-shimmer-char-base"],
  [data-component="text-shimmer"] [data-slot="text-shimmer-char-shimmer"] {
    transition-duration: 0ms;
  }
  [data-component="text-shimmer"] [data-slot="text-shimmer-char-shimmer"] {
    animation: none !important;
    color: inherit;
    -webkit-text-fill-color: currentColor;
    background-image: none;
  }
  [data-component="text-shimmer"] [data-slot="text-shimmer-char-base"] {
    opacity: 1 !important;
  }
}
```

- [ ] **Step 3: Commit**

```bash
git add src/components/opencode/tools/text-shimmer.tsx src/index.css
git commit -m "feat: port TextShimmer component from OpenCode"
```

---

### Task 3: Create Spinner & DiffChanges & ToolStatusTitle

**Files:**
- Create: `src/components/opencode/tools/spinner.tsx`
- Create: `src/components/opencode/tools/diff-changes.tsx`
- Create: `src/components/opencode/tools/tool-status-title.tsx`

- [ ] **Step 1: Create Spinner**

```tsx
export function Spinner({ className }: { className?: string }) {
  return (
    <div data-component="spinner" className={className}>
      {Array.from({ length: 16 }).map((_, i) => (
        <div
          key={i}
          style={{
            animation: i < 4 || i === 5 || i === 6 || i === 7 || i === 9 || i === 10 || i === 11 || i === 13 || i === 14 ? "none" :
              `pulse-opacity ${0.8 + Math.random() * 0.6}s ${Math.random() * 0.8}s infinite`,
            opacity: i < 4 ? 0 : (i % 2 === 0 ? 0.4 : 0.2),
          }}
        />
      ))}
    </div>
  )
}
```

Wait, actually the OpenCode spinner is more complex. Let me check it again. It's a 4x4 grid where corner squares are always hidden and others pulse. Let me write a simpler version that captures the visual effect. Actually, let me look at it more carefully from the explorer output:

> 4x4 grid of squares (16 total), each with random animation delay/duration
> Corner squares are always hidden (opacity: 0)
> Outer squares use "pulse-opacity-dim" animation
> Inner squares use "pulse-opacity" animation

Let me write a proper spinner:

```tsx
const CORNER = new Set([0, 3, 12, 15])

export function Spinner({ className }: { className?: string }) {
  return (
    <div data-component="spinner" className={className}>
      {Array.from({ length: 16 }).map((_, i) => {
        const isCorner = CORNER.has(i)
        const isOuter = i < 4 || i > 11 || i % 4 === 0 || i % 4 === 3
        return (
          <div
            key={i}
            data-slot="spinner-cell"
            style={{
              opacity: isCorner ? 0 : undefined,
              animation: isCorner ? "none" :
                `spinner-pulse ${0.8 + (i % 4) * 0.1}s ${(i * 0.07)}s infinite`,
            }}
          />
        )
      })}
    </div>
  )
}
```

Hmm, I need to think more carefully about the spinner. Let me read the actual spinner code.

Actually, looking at the exploration output for the spinner:
> 4x4 grid of squares (16 total), each with random animation delay/duration
> Corner squares are always hidden (opacity: 0)
> Outer squares use "pulse-opacity-dim" animation
> Inner squares use "pulse-opacity" animation
> All via inline style animation properties
> CSS: width: 18px, aspect-ratio: 1, flex-shrink: 0, color: inherit

I need the actual spinner component file. Let me read it. But I already have the key behavior: corner cells hidden, outer cells with dim pulse, inner cells with full pulse, random delays/durations.

Let me just write a simple version. I'll create the spinner as a CSS grid of squares.

Actually, wait. Let me skip the Spinner for now and use a simpler loading indicator. The exact spinner design is a minor visual detail. Let me focus on the more important components.

Actually no, the user said 100% match. Let me read the actual spinner source.<｜end▁of▁thinking｜>

<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="read">
<｜｜DSML｜｜parameter name="filePath" string="true">C:\Users\sera\Desktop\opencode\opencode\packages\ui\src\components\spinner.tsx