# Replicate OpenCode Tool-Calling UI in Shob

## Objective

Port the full tool-calling UI from `@opencode-ai/ui` (SolidJS) and `@opencode-ai/app` (SolidJS) into Shob (React 19) as a direct file-by-file translation. The result must match 100% in design, layout, behavior, styling, interactions, animations, and overall user experience with no differences.

## Source Files

All from `C:\Users\sera\Desktop\opencode\opencode\packages\`:

| Source | Lines | Purpose |
|--------|-------|---------|
| `ui/src/components/text-shimmer.tsx` + `.css` | 62 + 119 | Shimmer text animation |
| `ui/src/components/tool-status-title.tsx` | 136 | Animated status label |
| `ui/src/components/tool-error-card.tsx` + `.css` | 145 + 54 | Error card for failed tools |
| `ui/src/components/diff-changes.tsx` + `.css` | 115 + 42 | +/- change counts (default + bars variants) |
| `ui/src/components/spinner.tsx` | ~30 | 4x4 grid spinner |
| `ui/src/components/basic-tool.tsx` + `.css` | 283 + 248 | Collapsible tool card with spring animation |
| `ui/src/components/message-part.tsx` | 2310 | Full part rendering: UserMessageDisplay, AssistantMessageDisplay, TextPart, ReasoningPart, ToolPartDisplay, ContextToolGroup, ToolRegistry, all tool renderers (read, list, glob, grep, webfetch, websearch, task, bash, edit, write, apply_patch, todowrite, question, skill), ShellSubmessage, AnimatedCountList, CompactPart, diagnostics |
| `ui/src/components/tool-count-summary.tsx` + `.css` | ~80 | AnimatedCountList: comma-separated tool counts |
| `ui/src/components/tool-count-label.tsx` + `.css` | ~80 | AnimatedCountLabel: animated number + stem/suffix |
| `ui/src/components/session-turn.tsx` + `.css` | 533 | Turn wrapper: user message, divider, assistant parts, thinking, diffs accordion, error card, auto-scroll |
| `ui/src/components/collapsible.tsx` + `.css` (partial) | 148 | .tool-collapsible variant for card style |
| `ui/src/styles/theme.css` (partial) | 609 | CSS custom properties (--text-strong, --text-base, --background-base, etc.) |

## Target Files (in Shob `src/components/opencode/tools/`)

| New/Upgrade File | Ported From | Notes |
|---|---|---|
| `text-shimmer.tsx` + CSS in `index.css` | `ui/text-shimmer.tsx` + `.css` | React useState/useEffect for swap timing |
| `tool-status-title.tsx` + CSS | `ui/tool-status-title.tsx` | useRef for DOM measurement, rAF animation |
| `tool-error-card.tsx` + CSS | `ui/tool-error-card.tsx` | Collapsible, copy button, variant card |
| `diff-changes.tsx` + CSS | `ui/diff-changes.tsx` | default + bars SVG variant |
| `spinner.tsx` + CSS | `ui/spinner.tsx` | 4x4 grid, CSS pulse animations |
| `basic-tool.tsx` (UPGRADE) | `ui/basic-tool.tsx` | Add motion spring height anim, TextShimmer, open/close icons |
| `tool-registry.ts` (NEW) | from `message-part.tsx` | ToolRegistry singleton, ToolComponent type |
| `tool-renderers/*.tsx` (NEW) | from `message-part.tsx` | One file per tool (read, list, glob, grep, webfetch, websearch, task, bash, edit, write, apply_patch, todowrite, question, skill) |
| `message-user.tsx` (NEW) | `UserMessageDisplay` from `message-part.tsx` | User message with attachments, copy, revert, highlighted refs |
| `message-assistant.tsx` (NEW) | `AssistantMessageDisplay` from `message-part.tsx` | Groups parts, delegates to Part |
| `message-part.tsx` (NEW) | Part/TextPart/ToolPart/ReasoningPart/CompactPart from `message-part.tsx` | PART_MAPPING registry, Part dispatcher |
| `context-tool-group.tsx` (NEW) | from `message-part.tsx` | Groups read/glob/grep/list tools |
| `tool-count-summary.tsx` (NEW) | from `ui/tool-count-summary.tsx` | Animated comma-separated counts |
| `tool-count-label.tsx` (NEW) | from `ui/tool-count-label.tsx` | Animated number + stem/suffix |
| `session-turn.tsx` (NEW) | `ui/session-turn.tsx` | Full turn with auto-scroll, diffs accordion, error card |
| `diff-view.tsx` (NEW) | from `session-turn.tsx` | Diff accordion items for session diffs |

## SolidJS → React Translation Rules

| SolidJS | React |
|---------|-------|
| `createSignal(val)` | `useState(val)` |
| `createEffect(fn)` | `useEffect(fn)` |
| `createEffect(on(dep, fn))` | `useEffect(fn, [dep])` |
| `createMemo(fn)` | `useMemo(fn, deps)` |
| `createStore({x})` / `setState("x", v)` | `useState({x})` / `set(s => ({...s, x: v}))` |
| `onCleanup(fn)` | `useEffect(() => fn, [])` or return fn from useEffect |
| `<Show when={x}>` | `{x && ...}` or ternary |
| `<Switch><Match when={x}>` | ternary / if-else |
| `<For each={arr}>{(item) => ...}</For>` | `arr.map((item, i) => ...)` |
| `<Index each={arr}>{(item) => ...}</Index>` | `arr.map((item, i) => ...)` |
| `<Dynamic component={C} props={p} />` | `<C {...p} />` |
| `classList={{cls: bool}}` | `className={cn("base", bool && "cls")}` |
| `data-component="x"` | Same - keep exact data attributes |
| `motion` library | Same package (`motion/react` import) |
| `import { animate } from "motion"` | `import { animate } from "motion"` (works in React too) |

## CSS Strategy

1. Add CSS custom properties from OpenCode's `theme.css` into Shob's `src/index.css` (--text-strong, --text-base, --text-weak, --background-base, --surface-base, --border-base, --icon-base, etc.)
2. Import `motion` package for spring animations (replaces SolidJS `motion` usage)
3. Port component CSS into `src/index.css` using exact same `[data-component="..."]` and `[data-slot="..."]` selectors
4. Add text-shimmer keyframe animations
5. Add tool-collapsible variant styles matching `collapsible.css`
6. Keep Tailwind for layout, use CSS custom properties for color values

## Dependency Changes

- **Add**: `motion` (v11+) — same library OpenCode uses, React-compatible
- **Keep**: existing `@radix-ui/react-collapsible` / shadcn collapsible (add `.tool-collapsible` variant)
- **Keep**: `lucide-react` (icons — map OpenCode icon names to lucide equivalents)
- **Keep**: existing Tailwind CSS v4 setup

## Integration Points

The new components integrate into Shob via `AgentView.tsx`:
- `SessionTurn` replaces the existing message rendering loop
- Individual tool renderers replace `ToolPart` dispatch
- The `ToolRegistry` singleton replaces the existing if-else chain in `tool-part.tsx`
- CSS classes/layout stay the same for the outer shell (sidebar, tab bar, etc.)

## Verification

- Visual comparison: side-by-side screenshots of OpenCode Desktop and Shob showing identical tool cards, message bubbles, animations
- No regressions in existing Shob functionality (terminal, file tree, settings, sidebar)
- All tool renderers display correctly with real OpenCode session data
