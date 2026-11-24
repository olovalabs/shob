# Changelog

All notable changes to Shob will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Changed
- Idle sessions now hibernate: a background pane past its quiet threshold drops its terminal grid, keeps its shell, its last 2 000 lines of text and a bounded buffer of new output, and wakes from that text on return. Panes running a command, a full-screen program, or an agent turn stay resident.
- `SHOB_DIAGNOSTICS=1` reports per-pane grid, text, message and cache bytes with active/idle/hibernated counts, and the in-memory scrollback is configurable (`memory.terminal_scrollback_lines`).
- `shob` opens the coding-agent TUI from the current terminal; `shob gui` opens the desktop workspace. Both ship in one executable, including the optional RPC backend. From source, `cargo run` opens the desktop and `cargo run -- tui` opens the TUI.

### Fixed
- The terminal UI checks Shob releases and points to `shob update`, not the upstream engine's updater.
- **New chat** and the active project’s **+** button focus the current empty chat instead of creating a duplicate session.
- Show the workspace, branch, and project controls under the focused Agent composer only on the welcome screen.
- Keep “Open project folder…” inside the project picker, below the scrollable project list.
- Keep keyboard focus in the Git branch picker and make the whole search box clickable, including its padding.
- Center focused Agent completion menus above the composer at both wide and narrow window sizes.
- Group batched tool calls under a per-tool count summary (Read, Write, Shell, etc.); show calls while running, collapse on completion, and retain independently expandable output and per-call failures.
- Preserve nested Markdown emphasis, links, inline code and hard line breaks in Agent replies; keep prompts and tool output literal instead of interpreting their Markdown characters.
- Align the standalone terminal's GPUI pins and lockfile with the app, and check its locked dependency resolution in CI so standalone tests do not fail on a stale lockfile.
- List each project's sidebar sessions newest-first, so a newly created session appears at the top and remains visible when the project already has five or more sessions.
- Show the OpenCode provider marks on the model dialog's popular-provider cards and on every row in the provider list.
- Restored CI formatting compliance and removed a non-looping PTY benchmark readiness loop rejected by Clippy.

### Added
- Add flat **New chat** and **Search** rows above the sidebar projects. Search opens in place and matches session titles across all open projects, including collapsed projects and sessions below the preview limit.
- Show conversation input/output tokens, cache reads/writes, and estimated USD cost beneath the Agent input. Totals update after each model response, survive history restoration, and show unavailable pricing as unknown rather than free; hover for exact token counts.
- Added native provider/model discovery for all 67 OMP README integrations while preserving existing provider/model identities: 104 catalog providers, 6,279 frozen model entries, and real local/account model discovery. Chat selection excludes non-chat task models without deleting their metadata.
- Added native Rust API-key and browser/device OAuth flows, request-time renewal and vendor-specific inference transports; providers supporting both methods expose both. No OMP executable or runtime is required.
- Added an auto-focused, theme-matched model search field with a search icon, clear button, case-insensitive model/provider filtering, and a no-results state.
- Antigravity model catalog now exposes Claude Sonnet 4.6 and Gemini 3.5/3.1/2.5 Flash-Lite alongside Gemini 3.8 Flash, with the same Fast/Deep reasoning controls, so chat can use a responsive route while the 3.8 Cloud Code endpoint is congested.
- Native OMP approval, selection, confirmation, text/editor and structured-question dialogs with cancellation, expiry and stale-session guards; interaction responses stay out of transcript persistence.
- Explicit, confirmed workspace-scoped OMP trust: protected `write` by default, opt-in `yolo`, canonical-path checks and safe deferred authority changes during active turns.
- Per-pane explicit OMP reasoning effort without changing models, alongside Fast/Deep presets.
- Metadata-only OMP provider/argument/tool-cycle timings and `scripts/bench-omp-tools.py`, an isolated loopback fixture exercising real batched reads, local shell steps and approvals.
- Center the empty focused Agent screen's input with the Shob wordmark directly above it; keep the input at the bottom once a conversation starts.
- Zed-style per-platform app data directories (`src/paths.rs`): workspace, scrollback, agent conversations and backups under the OS data dir; user settings under the OS config dir. Missing folders are created on launch.
- Full session restoration: projects, sidebar tabs, nested split layouts, focused pane, per-pane working directories, tab titles/logos, input drafts, command history, sidebar width and window geometry are saved and restored after a restart.
- Terminal scrollback is snapshotted per pane and seeded above the fresh shell prompt on restore.
- Append-only agent conversation history in `conversations/<pane-uid>/*.jsonl`; restored transcripts match the previous UI, and native sessions resume on the next prompt (OMP restores the transcript only).
- **Settings → Data & Sessions** section: shows the data folder, opens it in the file manager, toggles session/scrollback/agent-history restore, and clears saved sessions.
- Synchronous save on window close and app quit, plus a 30-second autosave for crash recovery.
- One-time migration from the legacy `workspace.json` (including the old Markup location) with a timestamped backup in `<data>/backups/`.
- `SHOB_DATA_DIR` and `SHOB_DISABLE_RESTORE` environment overrides.
- Render streamed agent replies as Markdown, including headings, lists, links, and syntax-highlighted code blocks.
- Display all tool call details in rich Zed-style cards showing tool badges, call ID tags, inline argument summaries, status indicators, line counts, pretty-printed arguments with a copy button, and bounded scrollable outputs with one-click copying.
- Collapsible thinking accordions that display live streaming thoughts while reasoning and automatically fold into compact headers when the response begins.
- Dynamic autoscrolling for agent chat that stays pinned to the bottom across streaming updates and asynchronous Markdown layout changes, with smart user scroll-lock and a floating "Scroll to bottom" button.
- Pre-warm agent bridge processes in the background upon switching to Agent mode or selecting a model.
- Enable RPC idle cache warming and default to Fast mode (`low` thinking effort) for rapid responses, with an interactive `⚡ Fast`/`🧠 Deep` toggle and `/fast` / `/thinking` slash commands.
- Model selector for shob and Google Antigravity, with a persistent default provider/model for new panes.
- Polish the agent composer with a contextual placeholder, a roomier input, hidden key hints, a pill-styled model picker, and the same overall height as the terminal composer.
- Google Antigravity OAuth and model selection in `/auth`, with separate native chat sessions using the installed oh-my-pi agent.
- `/auth` in agent mode opens provider discovery and authentication with masked input and shob-managed credentials.
- Native per-pane embedded agent chat with streamed replies, tool activity, cancellation, and a terminal/agent switch. Requires a separately installed and configured `shob` executable.
- **Shob input editor**:
  - Shell history from zsh, bash, and fish history files, merged with the session's own commands for Up/Down recall and ghost suggestions.
  - Context-aware completion menu (Tab) for `PATH` commands, shell builtins, files and directories, git branches, and history — with arrow navigation and click-to-accept.
  - Inline ghost suggestion from history, accepted with Right or Tab.
  - Ctrl+R reverse history search palette with live filtering.
  - Readline editing in the input bar: Ctrl+A/E, Ctrl+U/K/W, Alt+B/F, Ctrl+P/N, Shift+Enter newline, and Ctrl+C interrupt.
  - Multi-line programmatic edits now leave the cursor at the end of the line.

### Changed
- Align Agent conversation typography, prompt bubbles and collapsible tool details with OpenCode Desktop's conversation view, while retaining the original input/composer, native rendering and bounded output previews.
- Stop native `read` after a bounded text page instead of loading the entire large file. Reduce filesystem and sorting overhead in `ls`, `find`, and `grep` without changing search order.
- Make native agent tools exclusive by default, preserve parallel read-only groups around mutations, and order result hooks without serializing safe tool bodies. Reduce shell capture and custom-tool input copying, bound validator retention, and preserve malformed arguments for explicit validation errors.
- Show built-in Agent activity, completion and failure in the sidebar's existing status indicators, independently of hidden terminal output. Unseen completions stay marked until viewed or acknowledged.
- Match terminal and Agent pane frames with 0.5px slate outlines and 10px rounded corners, with focused and inactive colors shared across palettes in dark and light mode.
- Moved the Agent composer's working-directory and Git-branch badges into a compact Environment panel. Click the info icon at the toolbar's top-right, on the same row as Agent and the model picker, to slide it open directly above the input, aligned to its right edge; click again or press Escape from the composer to close it. Terminal badges remain inline.
- Moved initial Markdown/HTML parsing and syntax highlighting off GPUI; bounded pending documents/results with latest-state mailboxes and monotonic compatible-prefix publication during continuous streaming.
- Coalesced embedded-session wake callbacks before mutex/channel work, scanned only unfinished Markdown boundaries, and removed per-cell snapshot strings and whole-match-list searches from terminal painting.
- Added queue residence/backpressure, tool snapshot-to-paint and PTY read/parse-to-paint diagnostics, plus reproducible streaming performance benchmarks.
- Reworked shell streaming with bounded asynchronous capture, complete sanitized logs, 16 ms quiet-period checkpoints, concurrent stdin/stdout/stderr/status handling, and reliable cancellation/drop cleanup. Removed redundant timeout padding and batched log appends.
- Moved PTY setup, input, resize and teardown off GPUI; added bounded FIFO input with physical-write receipts, visible refusal/failure handling, real child exit status and cooperative parsing/restoration.
- Bounded native/RPC event delivery, prompt admission and transcript persistence; coalesce identified snapshots before projection, preserve lifecycle events, resignal fair UI drains, and report persistence overload.
- Replaced the embedded `grep` tool's custom matching loop and full-tree snapshot with ripgrep's Rust matcher/searcher: streaming native file reads, incremental ignore-aware traversal, early result/output limits, binary detection and merged context windows. No external `rg` executable is required.
- Hide anonymous/keyless provider entries from the authentication list without removing their backend support.
- Restyled model selection with compact rows, trailing selection checks, free-model badges, and a two-column provider card section matching the OpenCode reference. Model selection and provider authentication now share one narrower 520px modal with theme-derived backgrounds, text, borders, and hover colors, plus consistent close and back navigation.
- Compact streamed OMP text, worker deltas and cumulative tool snapshots before enqueue with bounded backpressure, preserving tool lifecycle order and complete results.
- Favor independent read/search batches and deterministic multi-step local execution while keeping OMP's shared/exclusive scheduling and full tool/extension discovery.
- Redesigned the project sidebar with compact, smaller session titles without per-session logos, reduced left padding, subtle selection styling, and project/session remove buttons shown only while hovering their row. Each project initially shows the five newest sessions; each "Show more" click reveals ten more, and "Show less" collapses back to five. Expanded lists scroll while the toolbar and Settings footer remain visible.
- Poll active agent streams at approximately frame cadence while retaining the lower idle polling rate.
- Migrated the application and terminal renderer to GPUI Fast.
- The terminal displays a plain grid without block UI. The Warp-style bottom input is always available when the shell is idle and temporarily hides while a command runs or the alternate screen is active.
- Removed the bottom input toggle; a previously saved `bottom_input` preference no longer disables the input.

### Fixed
- Keep Agent input text uniformly colored instead of treating the first word as a shell command; terminal mode retains Bash highlighting.
- Deliver the terminal's trailing throttled search refresh even when output stops; coalesce resize and visibility rescans.
- Fix duplicate Antigravity model-route registration causing false ambiguous-model errors; host-declared transports now replace bundled routes for the same model identity.
- Keep Markdown agent replies updating during continuous streams and reduce OMP streaming payloads.
- Display streamed agent thinking text instead of leaving the chat blank while a model is reasoning.
- `/model` opens model selection instead of authentication; selecting an already authenticated provider no longer starts another login.
- System appearance changes now update existing terminal panes and application chrome; theme changes preserve the configured component fonts.
- Notify compatible running terminal applications when the terminal palette changes, so their light and dark themes can update without a restart.

---

## [0.0.1] - 2026-11-01

### Added
- **GPU-Accelerated Terminal Emulator**:
  - Embedded `terminal` component utilizing GPUI and `alacritty_terminal` for VTE parsing.
  - High-performance 1 ms stream coalescing with a 128 KiB parse cap to preserve UI framerates at high log throughput.
  - Full support for ANSI colors (standard 16, 256-color, 24-bit RGB), text styles, and embedded Mononoki monospace fonts.
  - OSC 52 clipboard copy and paste integration.
- **Agent Attention & Status System**:
  - Parity with the Shob agent attention model (`blocked > done > working > idle`).
  - Latching completion tracking: agent runs that finish in an unviewed tab switch to `done` and alert the user; focusing the tab clears status to `idle`.
  - Audio chimes and desktop toast notifications for blocked queries and missed finishes (`SHOB_NO_SOUND=1` support).
  - Sub-agent hierarchy support: child sessions with `agent_id` nest neatly under parent sessions without duplicate notifications.
- **Multi-Project Workspace Management**:
  - Collapsible project folders in the sidebar grouping related terminal sessions.
  - Workspace state persistence to `~/.config/shob/workspace.json`, automatically restoring open projects and session trees across application restarts.
  - Project-level actions for adding sessions and opening directories.
- **Native Agent Integrations & CLI**:
  - One-command setup CLI (`shob integration setup`, `status`, `doctor`, `remove`).
  - Native hook support for Claude Code, Codex, Pi, OMP, OpenCode v2, and Command Code (Shob compatibility protocol).
  - Terminal CLI commands: `shob report`, `shob skill`, `shob status`, `shob pane`, and OSC 7501 ANSI sequence reporting.
- **Desktop Integration**:
  - Custom title bar and window decoration controls.
  - Automatic Linux desktop entry (`.desktop`) creation in `~/.local/share/applications/`.
  - Taskbar icon support for Linux, macOS, and Windows.
  - GitHub Dark palette theme.
