# Shob

Shob is a GPU-accelerated terminal emulator for developers orchestrating AI coding agents. A GPUI Fast frontend renders an [alacritty_terminal](https://github.com/alacritty/alacritty)-based terminal widget, a Unix-socket status bus tracks agent state in real time, and an embedded Rust agent runtime (pi-rust) powers native agent chat with a frozen multi-provider catalog.

You can think of Shob as a native desktop counterpart to tools like Warp and Zed's terminal, built specifically around the problem that matters when agents do the typing: knowing when an agent is blocked, when it finished while you were away, and when it needs you — without notification fatigue.

## What makes Shob special?

Shob's value is a small set of properties that are easy to regress and hard to win back. Never compromise on these.

### 1. Performance without compromise

The terminal is a hot path. Output can arrive faster than 40 MB/s, and the UI must never freeze, drop frames, or fall behind. This is why the 1 ms stream coalescer, the 128 KiB parse cap per frame, bounded queues everywhere, and background markdown/tree-sitter parsing exist. Any change that adds unbounded allocation, per-frame work, or blocking I/O on the GPUI main thread is a regression, even if it feels small.

### 2. The attention model is sacred

The attention hierarchy is `blocked > done > working > idle`. An agent completion seen live transitions directly to `idle`/`active` — no bell, no toast. Only an unviewed completion latches to `done`, plays a chime, and clears when you switch to its tab. Never infer `done` from silence or inactivity. Never let a sub-agent (a session reporting an `agent_id`) raise its own notification or corrupt its parent's status. These invariants are what users trust; a wrong alert is a bug, and so is a missed one.

### 3. State survives everything

Workspace restore is a core promise: projects, tabs, split panes, focus, per-pane cwd, input drafts, command history, window geometry, scrollback, and agent conversations all come back on relaunch, and restored panes look exactly as they did before. Anything you add to pane or conversation state must either survive restart (through `workspace.json`, `sessions/`, or `conversations/`) or be a deliberate, documented exception.

### 4. Agent status from every source

Agent state can arrive five ways: the authenticated Unix status bus, OSC 7501 escape sequences, embedded TOML screen-detection regexes evaluated against the visible buffer, `shob report` from the CLI, and Command Code's native socket compatibility. A status feature that only works through one source is incomplete.

## A note from the maintainers

We like simple systems, native-feeling software, and terminals that feel instant. Do not preserve complexity just because it exists, and do not introduce machinery because it looks architecturally impressive. Understand the real constraint, then fight for the smallest model that makes the correct behavior unsurprising.

Channel both "measure twice, cut once" and "yagni". The bar for a new dependency, a new thread, or a new background task is high; the bar for an extra allocation in a per-frame path is higher.

The rest of this document is meant to help you navigate the codebase and make changes effectively. Think of these instructions less as "hard rules", more as "good defaults". The developer's preferences should be able to override anything here.

Of note: most Shob development is done by agents, often inside a Shob terminal pane. This means you should be careful about accessing data, killing processes, and launching instances — the machine you are changing code on may be running the very app you are editing.

## A small glossary

We need to be on the same page with terminology. When communicating, use this language:

- **you** means the agent reading this file and changing Shob.
- **we, us, and maintainers** mean the people building Shob. These are who you are talking to now.
- **user** means the person using Shob to run terminals and direct coding agents.
- **agent** means the coding agent a user runs inside a Shob pane. Depending on context, that may also include you.
- **project** means a folder on disk grouping related sessions in the sidebar.
- **tab** means one saved session under a project.
- **pane** means one terminal region in the split tree. A pane owns its cwd, input draft, command history, terminal view, and its own agent conversation.
- **workspace** means the persisted set of projects, tabs, and panes recorded in `workspace.json`.
- **status bus** means the per-user Unix socket that receives agent state events.
- **status source** means one of the ways state arrives: socket, OSC 7501, screen detection, CLI report, Command Code compat.
- **attention** means the state machine that decides when the user is alerted, and how strongly.
- **conversation** means the append-only JSONL transcript of one pane's agent chat.
- **embedded backend** means the in-process shob agent runtime (`agent/` subtree) that powers native agent chat.
- **provider** means a model provider (Anthropic, OpenAI, Gemini, Antigravity, Bedrock, Vertex, Ollama, ...) in the native catalog.
- **scrollback** means the saved terminal output for a pane (`sessions/<pane-uid>.txt`).
- **Shob home** means the platform data directory (`~/.local/share/shob` on Linux by default). Runtime state lives below it.

## The three ways to hurt yourself

1. **Killing by pattern.** Never `pkill -f`, `pgrep | kill`, or kill a PID you found by matching a name, path, or command string. Shob runs real shells, and the user's real agents run inside them — including possibly you. Kill only a PID you captured at spawn.
2. **Writing to the live install.** The Shob home directory is the developer's real workspace: real projects, real conversations, real scrollback. Reading it for fixtures is fine and encouraged; never launch a test instance against it. Point test instances at scratch state with `SHOB_DATA_DIR=/absolute/path` and `SHOB_DISABLE_RESTORE=1`. Never start a second Shob that would fight the running one for the status socket or window focus.
3. **Blocking the main thread.** Never do unbounded I/O, parsing, `/proc` scanning, locking across await points, or long computation on the GPUI main thread. Heavy work belongs on background executors or the embedded backend's Tokio runtime and comes back through bounded queues. If you add a channel, it has a cap.

## Hit every surface

The most common defect in this repo is a change that works on the path you tested and is missing everywhere else. Before calling work done, walk this list and say which entries applied:

- **Platforms.** Linux (Wayland and X11), macOS, and Windows all build and run. Linux needs the system packages listed in the README; platform paths go through `src/paths.rs`.
- **Pane modes.** Every pane has two modes: terminal view and agent chat. A capability usually belongs in both or must explicitly not apply to one. Each pane keeps its own conversation, model, and thinking level.
- **Status sources.** A status or attention change must behave correctly from all five sources. Screen detection can only see what a regex can see; the socket is authenticated by `pane_generation`.
- **Integrations.** Claude Code, Codex, Pi, OMP, OpenCode, and Command Code each have an integration surface (`shob integration setup|status|doctor|remove`). Agent-shaped features need a decision per integration, even if the decision is "not supported here".
- **Attention surfaces.** A state change reaches the sidebar badge, sounds, toasts, and sub-agent grouping. A new state needs a decision for each — and for `SHOB_NO_SOUND=1`.
- **Themes.** Light, dark, and system appearance all exist, plus user overrides in `theme_colors`. A hardcoded color in a view is a bug unless the token does not exist yet.
- **Backends.** The embedded backend is the default; the legacy subprocess bridge survives behind `SHOB_AGENT_BACKEND=rpc`. Agent-bridge changes must not silently break the fallback.
- **Persistence.** Anything touching pane, tab, project, or conversation state must round-trip through restore, respect the v2 `workspace.json` shape and generation guard, and keep the v1 migration path working. Reverse states count too: setup needs remove, latch needs clear-on-view, approval needs deny.
- **Docs.** Check whether the change makes `README.md` or `ARCHITECTURE.md` inaccurate. Apply the [documentation rules](#documentation) before adding anything.

## Running Shob during development

- `cargo run` opens the desktop app; `cargo run -- tui` starts the agent TUI in the current terminal. The dev profile is tuned so dependencies optimize while the app builds fast; do not "simplify" the profile sections in `Cargo.toml`.
- For UI work, launch against scratch state:
  ```bash
  SHOB_DATA_DIR=/tmp/shob-dev SHOB_DISABLE_RESTORE=1 cargo run
  ```
- `cargo run` with no overrides restores the developer's real workspace and grabs real window focus. Prefer the scratch-state form unless you specifically need real state, and never leave a test instance running when you finish.
- Diagnostics: `SHOB_STREAM_TIMINGS=1` prints streaming and paint timings; `SHOB_DIAGNOSTICS=1` adds PTY queue-depth and CPU/RSS counters. Perf claims should come from one of these, not from vibes.
- Stop what you started, by the PID you tracked. A windowing app left running holds the display and may steal focus from the developer.
- The status socket is per-user (`$XDG_RUNTIME_DIR/shob-status.sock`, falling back to `/tmp/shob-$UID/status.sock`). Two instances contend for it; that is expected to be messy, so don't.

## Verifying

- Before you push, run:
  ```bash
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test
  cargo test -p terminal
  ```
  The workspace is rustfmt-formatted; keep it that way.
- Tests live inline as `#[cfg(test)]` modules beside the code they cover. The vendored `agent/` subtree and the standalone `provider` crate have their own test suites; do not run the full `agent/` suite unless you are working in the subtree — it is upstream's, and CI on your change does not own it.
- While you iterate, run only the focused tests for the module you touched: `cargo test attention`, `cargo test workspace_state`, and so on.
- Test behavior that can break: attention priority and latching, status-bus authentication and generation checks, workspace persistence and migration, the coalescer and parse caps, provider/auth flows, and the logic a bug fix changed. Leave out tests for what types already guarantee, trivial logic, and copy text.
- Extend before you add. When an existing test or table covers the behavior, add a row or an assertion there. Write a new `#[test]` only when no case fits.
- Test at one layer: the lowest one that fails. Do not repeat the case in a wrapper test or at each call site.
- No wall-clock sleeps or polling loops in tests. Assert on state and events, not elapsed time. A test that needs a timeout to pass is wrong.
- Purely visual fixes (spacing, color, alignment) get a described before/after, not a test. Logic bugs get a failing test first, then the fix.
- For streaming/tool benchmarks, build the test binary (`cargo test --locked --bin shob --no-run`) and drive it with `scripts/bench-agent-stream.py`.
- Manual smoke paths that are worth one pass after integrating: `scripts/status-demo.sh blocked` (and the other states) for the attention system, and one real `cargo run` with scratch state exercising your change end to end.

## Pull requests

- Never make a PR unless the developer explicitly asks you to do so.
- Conventional commit titles, plain language: `fix(terminal): new panes no longer spike CPU on resize`.
- Body: the problem in a sentence or two, then how you fixed it.
- Never append `Co-Authored-By:` trailers or "Generated with ..." footers to commits or PR descriptions — not Claude's, not Command Code's, not any other AI tool's. Add one only when the developer explicitly names the tool and asks for it; attribution is the developer's call, not the harness's default.
- UI changes need before/after screenshots; motion or streaming changes need `SHOB_STREAM_TIMINGS` numbers.
- One request is one PR. Do not bundle unrelated cleanups.
- Keep implementation plans, research notes, and scratch files out of the repository.

## Documentation

Most code changes do not need a documentation change. Agents can read the code.

- `ARCHITECTURE.md` carries subsystem design, constraints that span components, data flows, and the reasons behind the performance architecture. Before adding a paragraph, ask what a maintainer would get wrong without it. If reading the relevant code answers the question, leave it out.
- Do not document every feature, enumerate fields or methods, narrate control flow, or append PR summaries. Types, tests, and code already record the implementation.
- `README.md` is user-facing: what a feature does, how to start it, and anything unintuitive (environment variables, data locations, provider setup). A settings path is useful; descriptions of visible buttons and layouts are not.
- When a documented decision or constraint changes, rewrite or remove the affected text. Do not append another account of the new behavior.

## How it works

Each pane owns a PTY (`portable-pty`). Output streams through a 1 ms coalescer into alacritty's VTE parser and lands in the cell grid, capped at 128 KiB per frame; GPUI paints glyphs from the grid with embedded fonts. Agent state arrives from the five status sources into the status bus, which validates the pane generation and dispatches to the attention state machine; the machine computes priority and visibility, latches unviewed completions, and drives badges, sounds, and toasts. Agent chat opens an embedded shob session in-process — model/credential resolution from the frozen provider catalog, built-in tools, durable JSONL sessions — streaming typed events into bounded queues that GPUI drains per pane. Persistence is debounced (300 ms) atomic writes: `workspace.json` for structure, `sessions/` for scrollback, `conversations/` for transcripts, `settings.json` (JSONC) for appearance and toggles.

Architecture and its constraints: [ARCHITECTURE.md](ARCHITECTURE.md).

## Where code lives

- `src/` — the main application crate.
  - `attention.rs`, `status.rs`, `status_bus.rs` — attention state machine, status schemas, socket IPC.
  - `integrations.rs`, `agent_rules.rs`, `shob_compat.rs` — agent hook setup and compatibility.
  - `agent_bridge.rs`, `agent_bridge_rpc.rs`, `agent_auth.rs`, `shob_storage.rs` — embedded backend host, legacy RPC bridge, auth, session storage.
  - `agent_ui/` — agent chat view, composer, interaction.
  - `workspace_state.rs`, `conversations.rs`, `settings.rs`, `paths.rs` — persistence and platform paths.
  - `file_preview.rs`, `project_diff.rs`, `file_icons.rs` — changes/diff and preview panes.
  - `theme.rs`, `logos.rs`, `sound.rs`, `diagnostics.rs`, `stream_timing.rs` — presentation and diagnostics.
- `terminal/` — the standalone terminal widget crate: PTY management, coalescer, VTE integration, cell grid, selection, rendering.
- `components/` — the local gpui-component fork, wired in through `[patch.crates-io]`. GPUI UI primitives live here; treat upstream API carefully when bumping.
- `agent/` — the vendored pi-rust Git subtree with its own Cargo workspace. Shob path-depends on `agent/crates/embedded`. Do not restructure casually; syncs are subtree operations. Non-build upstream assets were removed at import.
- `provider/` — the native provider and authentication catalog crate (based on Oh My Pi), built and tested standalone.
- `assets/` — icons, sounds, themes, agent-detection TOML manifests, integration scripts.
- `scripts/` — demo and benchmark utilities.

## Taste

- Never block the GPUI main thread. If work is heavy, it runs on a background executor or a dedicated runtime and returns through a bounded queue with an explicit cap.
- Bounded everything. Queues, batches, and buffers carry their caps in their types and their names; when you add a channel, you add its limit.
- The coalescer and parse caps are load-bearing. Do not bypass, raise, or special-case them without measuring and writing the numbers down.
- No continuously repainting animations. Users run agents for hours; a dropped frame and a pegged GPU are noticed.
- Attention invariants outrank feature convenience. When a feature wants to alert, check the hierarchy and the latching rules first.
- Persistence changes migrate or they don't ship. `workspace.json` has a version, a generation guard, and a v1 backup path — extend them; don't reinvent them.
- IPC authenticates. Anything touching the status bus keeps the per-user scoping and the `pane_generation` check.
- Inferred types and concrete structs over generics; `anyhow` at boundaries, precise types in the engine.
- Default to no comment. Write one only for a constraint, an external quirk, or an invariant the code can't show, in one line where you can and four at most. Don't restate names, narrate steps, or explain the fix you just made.
- Edition 2024 idioms, rustfmt-clean, clippy-clean with `-D warnings`.
- If a rule here fights the task in front of you, say so loudly and get a human sign-off before breaking it.

## Additional tips

- Environment variables are the configuration surface for development and debugging (`SHOB_DATA_DIR`, `SHOB_DISABLE_RESTORE`, `SHOB_STREAM_TIMINGS`, `SHOB_DIAGNOSTICS`, `SHOB_NO_SOUND`, `SHOB_AGENT_BACKEND`, `SHOB_AGENT_BROWSER_BIN`). When you add one, follow the naming pattern and document it in the README section it belongs to.
- Security matters, but should not be over-indexed on for dev-only diagnostics paths. The status socket's authentication is not one of those paths — it is a real boundary.
