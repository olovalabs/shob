# React to SolidJS Migration Design

## Overview

Full rewrite of the Shob Electron desktop app from React 19 to SolidJS. All React-specific dependencies replaced with SolidJS equivalents. Automated conversion of React patterns to SolidJS primitives with manual fixes for complex components.

## Dependency Mapping

### Core Framework
| Remove | Add |
|---|---|
| `react` ^19.2.4 | `solid-js` ^1.9.5 |
| `react-dom` ^19.2.4 | `solid-js/web` (bundled with solid-js) |
| `@vitejs/plugin-react` ^6.0.1 | `vite-plugin-solid` ^2.11.6 |
| `@types/react` ^19.2.14 | (remove) |
| `@types/react-dom` ^19.2.3 | (remove) |

### UI Primitives
| Remove | Add |
|---|---|
| `radix-ui` ^1.4.3 | `@kobalte/core` ^0.13.x |
| `@base-ui/react` ^1.3.0 | `@kobalte/core` (covers same primitives) |
| `lucide-react` ^1.6.0 | `lucide-solid` ^1.6.0 |
| `cmdk` ^1.1.1 | Custom with `@kobalte/core` Combobox |
| `vaul` ^1.1.2 | `@kobalte/core` Dialog/Drawer |
| `next-themes` ^0.4.6 | Custom theme context with SolidJS signals |

### State & Routing
| Remove | Add |
|---|---|
| `zustand` ^5.0.12 | `solid-js/store` (bundled) |
| `react-router-dom` ^7.13.2 | `@solidjs/router` ^0.15.x |

### Charts & Data Viz
| Remove | Add |
|---|---|
| `recharts` ^3.8.0 | Custom SVG chart components (bar, line, area) using SolidJS signals for reactivity. The current `chart.tsx` is a shadcn/ui wrapper around Recharts providing ChartContainer, ChartTooltip, ChartLegend, and ChartStyle. Replace with native SVG components that accept the same `ChartConfig` pattern and render bars/lines/areas directly. |

### Forms & Inputs
| Remove | Add |
|---|---|
| `react-day-picker` ^9.14.0 | Custom date picker with Kobalte |
| `react-resizable-panels` ^4.8.0 | Custom resize logic with SolidJS signals |
| `input-otp` ^1.4.2 | Custom OTP input component |
| `embla-carousel-react` ^8.6.0 | Custom carousel with SolidJS signals |

### Notifications
| Remove | Add |
|---|---|
| `sonner` ^2.0.7 | `solid-sonner` |

### Kept Unchanged (framework-agnostic)
- `@xterm/*` — Terminal rendering
- `@lydell/node-pty` — PTY bindings
- `chokidar` — File watching
- `class-variance-authority` — Variant classes
- `clsx`, `tailwind-merge` — Class utilities
- `date-fns` — Date formatting
- `electron`, `electron-builder` — Desktop framework
- `tailwindcss`, `@tailwindcss/vite` — Styling

## Conversion Patterns

### React Hooks → SolidJS Primitives

| React | SolidJS |
|---|---|
| `useState<T>(initial)` | `createSignal<T>(initial)` |
| `useEffect(fn, deps)` | `createEffect(() => fn())` |
| `useMemo(fn, deps)` | `createMemo(() => fn())` |
| `useCallback(fn, deps)` | Direct function (stable by default) |
| `useRef<T>(initial)` | `let el: T;` or `createSignal<T>(initial)` |
| `useContext(Context)` | `useContext(Context)` (same API) |
| `useReducer(reducer, init)` | `createStore(init)` with actions |
| `useId()` | `useId()` from solid-js (same API) |
| Custom hooks | Composables (same pattern, SolidJS primitives) |

### JSX Transformations

| React | SolidJS |
|---|---|
| `className="..."` | `class="..."` |
| `htmlFor="..."` | `for="..."` |
| `style={{ key: value }}` | `style={{ key: value }}` (same) |
| `{condition && <Comp />}` | `{condition() ? <Comp /> : null}` |
| `{arr.map(...)}` | `{arr().map(...)}` (call signals) |
| `<React.Fragment>` | `<>` or `<Fragment>` |
| `dangerouslySetInnerHTML` | `innerHTML` attribute |
| `ref={elRef}` | `ref={(el) => elRef(el)}` |

### Event Handlers
- Same naming convention (`onClick`, `onChange`, etc.)
- No synthetic events — native DOM events
- `e.preventDefault()` works the same
- No `stopPropagation` auto-behavior changes

### State Management: Zustand → Solid Store

```typescript
// Before (Zustand)
import { create } from 'zustand'
interface SessionState {
  sessions: Session[]
  activeSession: string | null
  addSession: (s: Session) => void
  removeSession: (id: string) => void
  setActiveSession: (id: string) => void
}
export const useSessionStore = create<SessionState>((set) => ({
  sessions: [],
  activeSession: null,
  addSession: (s) => set((state) => ({ sessions: [...state.sessions, s] })),
  removeSession: (id) => set((state) => ({ sessions: state.sessions.filter(s => s.id !== id) })),
  setActiveSession: (id) => set({ activeSession: id }),
}))

// After (Solid Store)
import { createStore } from 'solid-js/store'
interface SessionState {
  sessions: Session[]
  activeSession: string | null
}
const [store, setStore] = createStore<SessionState>({
  sessions: [],
  activeSession: null,
})
export const sessionActions = {
  addSession: (s: Session) => setStore('sessions', (prev) => [...prev, s]),
  removeSession: (id: string) => setStore('sessions', (prev) => prev.filter(s => s.id !== id)),
  setActiveSession: (id: string) => setStore('activeSession', id),
}
export { store, setStore }
```

### Routing: React Router → Solid Router

```typescript
// Before (React Router)
import { BrowserRouter, Routes, Route } from 'react-router-dom'
<BrowserRouter>
  <Routes>
    <Route path="/" element={<Home />} />
    <Route path="/session/:id" element={<Session />} />
  </Routes>
</BrowserRouter>

// After (Solid Router)
import { Router, Route } from '@solidjs/router'
<Router>
  <Route path="/" component={Home} />
  <Route path="/session/:id" component={Session} />
</Router>

// Navigation
// Before: useNavigate() from react-router-dom
// After: useNavigate() from @solidjs/router (same API)
// Before: useParams()
// After: useParams() (same API)
```

## Component Conversion Order

### Phase 1: Foundation
1. `package.json` — Update dependencies
2. `vite.config.ts` — Replace plugin-react with vite-plugin-solid
3. `tsconfig.*.json` — Remove React JSX settings, add SolidJS
4. `src/main.tsx` — Entry point: `render()` from solid-js/web
5. `src/App.tsx` — Root component with Router

### Phase 2: Core Logic
6. `src/store/index.ts` — Zustand → createStore
7. `src/hooks/*` — React hooks → SolidJS composables
8. `src/services/*` — Update any React-specific patterns
9. `src/config/*` — Update any reactive config
10. `src/types/*`, `src/utils/*`, `src/constants/*` — No changes expected

### Phase 3: UI Components (src/components/ui/)
Convert all 55+ UI components from Radix/Base-UI to Kobalte equivalents:
- `accordion.tsx` → Kobalte Accordion
- `alert-dialog.tsx` → Kobalte AlertDialog
- `aspect-ratio.tsx` → Native CSS or remove
- `avatar.tsx` → Kobalte Avatar
- `badge.tsx` → Plain component (no Radix)
- `breadcrumb.tsx` → Plain component
- `button.tsx` → Plain with CVA
- `button-group.tsx` → Plain component
- `calendar.tsx` → Custom with date-fns + Kobalte
- `card.tsx` → Plain component
- `carousel.tsx` → Custom carousel
- `checkbox.tsx` → Kobalte Checkbox
- `chart.tsx` → Custom SVG charts
- `collapsible.tsx` → Kobalte Collapsible
- `combobox.tsx` → Kobalte Combobox
- `command.tsx` → Custom with Kobalte
- `context-menu.tsx` → Kobalte ContextMenu
- `dialog.tsx` → Kobalte Dialog
- `direction.tsx` → Plain component
- `drawer.tsx` → Kobalte Dialog (drawer variant)
- `dropdown-menu.tsx` → Kobalte DropdownMenu
- `empty.tsx` → Plain component
- `field.tsx` → Kobalte Field
- `hover-card.tsx` → Kobalte Popover
- `input-group.tsx` → Plain component
- `input-otp.tsx` → Custom OTP
- `input.tsx` → Plain component
- `item.tsx` → Plain component
- `kbd.tsx` → Plain component
- `label.tsx` → Kobalte Label
- `menubar.tsx` → Kobalte Menubar
- `navigation-menu.tsx` → Kobalte NavigationMenu
- `pagination.tsx` → Plain component
- `popover.tsx` → Kobalte Popover
- `progress.tsx` → Kobalte Progress
- `radio-group.tsx` → Kobalte RadioGroup
- `resizable.tsx` → Custom resize
- `scroll-area.tsx` → Kobalte ScrollArea
- `select.tsx` → Kobalte Select
- `separator.tsx` → Kobalte Separator
- `sheet.tsx` → Kobalte Dialog (sheet variant)
- `sidebar.tsx` → Plain component (different from app Sidebar)
- `skeleton.tsx` → Plain component
- `slider.tsx` → Kobalte Slider
- `sonner.tsx` → solid-sonner
- `spinner.tsx` → Plain component
- `switch.tsx` → Kobalte Switch
- `table.tsx` → Plain component
- `tabs.tsx` → Kobalte Tabs
- `textarea.tsx` → Plain component
- `toggle-group.tsx` → Kobalte ToggleGroup
- `toggle.tsx` → Kobalte Toggle
- `tooltip.tsx` → Kobalte Tooltip

### Phase 4: App Components
11. `src/components/TitleBar.tsx` — Electron title bar
12. `src/components/Sidebar.tsx` — App sidebar with sessions list
13. `src/components/TabBar.tsx` — Tab management
14. `src/components/Terminal.tsx` — xterm.js integration
15. `src/components/FileTree.tsx` — File tree component
16. `src/components/MainView.tsx` — Main content area
17. `src/components/WelcomeScreen.tsx` — Welcome/onboarding
18. `src/components/CliAvatar.tsx` — CLI avatar display
19. `src/components/index.ts` — Barrel exports

### Phase 5: Cleanup
20. Remove all React dependencies from package.json
21. Remove `@types/react`, `@types/react-dom`
22. Run `pnpm install` to clean lockfile
23. Fix any remaining type errors
24. Verify Electron integration works
25. Test build pipeline

## Risks and Mitigations

### Risk: Recharts has no SolidJS equivalent
**Mitigation:** Build custom SVG chart components using SolidJS signals for reactivity. Start with basic bar/line charts. Use existing chart data structures from the current implementation.

### Risk: Kobalte API differs from Radix
**Mitigation:** Kobalte follows similar patterns to Radix but with SolidJS reactivity. Props and component structure will need adjustment. Reference Kobalte docs for each component.

### Risk: xterm.js integration may need adjustment
**Mitigation:** xterm is framework-agnostic. The React component wraps it with `useRef` and `useEffect`. Replace with SolidJS `ref` callback and `createEffect`. Terminal lifecycle (init, dispose, resize) maps directly.

### Risk: Electron IPC calls in reactive context
**Mitigation:** Electron IPC calls are async and framework-agnostic. Wrap in `createResource` for async data or `createEffect` for side effects. No fundamental changes needed.

### Risk: Large component count (65+ files)
**Mitigation:** Convert systematically following the phased order. Foundation first, then UI primitives, then app components. Each phase builds on the previous.

## Success Criteria

1. `pnpm install` succeeds with SolidJS dependencies
2. `pnpm dev` launches Electron app with SolidJS renderer
3. All UI components render correctly with Kobalte
4. Terminal sessions work (xterm.js integration)
5. State management works (sessions, tabs, settings)
6. `pnpm build` produces working renderer bundle
7. `pnpm build:electron` produces desktop installer
8. No React imports remain in source code
9. TypeScript compiles without errors
10. ESLint passes with no errors
