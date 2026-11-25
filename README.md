
<div align="center">

<img src="https://github.com/user-attachments/assets/1c94b251-14f3-4311-8c1b-fe04cff289fe" alt="Shob Logo" width="110" />

# Shob

**One workspace. Every agent.**

A fast, lightweight, native workspace for AI coding agents, terminals, and projects.

Built with Rust and GPUI.

[Getting Started](#getting-started) · [Features](#features) · [Contributing](CONTRIBUTING.md)

</div>

---

<div align="center">
  <img src="https://github.com/user-attachments/assets/0493d9f2-9a6a-4cde-9572-477047bff1c1" alt="Shob Application Preview" width="100%" />
</div>

## Why Shob?

Working with multiple AI coding agents shouldn't mean juggling terminals, windows, and disconnected conversations.

**Shob brings everything into one place.**

Run your favorite coding agents, manage multiple projects, track active sessions, and switch between terminal workflows and native AI chat without losing context.

Built from the ground up with Rust and GPUI, Shob focuses on native performance, low resource usage, and a smooth developer experience.

## Features

### Multiple Agents, One Workspace

Run your favorite AI coding agents side by side.

- Claude Code
- OpenAI Codex
- Pi
- OpenCode
- OMP
- Command Code
- Shob's built-in AI agent

Agent mode opens by default with Shob’s built-in agent. Split view is terminal-only; switching layouts keeps your conversations and drafts, and your choice is saved. Terminal-based agents still work in either layout.

### Native Performance

Built with Rust and GPUI instead of Electron or a browser-based interface.

- Native desktop interface
- Lightweight resource usage
- Responsive terminal rendering
- Fast workspace navigation
- Efficient session management

Terminal shells and their I/O threads start when a pane is first shown or sent input, rather than for every restored tab. Native agent runtimes start on the first prompt. Idle background tabs release disposable view caches and safely settled embedded agent runtimes; opening the conversation again preserves its history, and the next prompt resumes its saved session.

Running commands, blocked agents, and queued work stay live. Already-started shells, terminal buffers, and conversation text remain in memory to preserve state. Legacy RPC agent runtimes remain resident because that backend cannot prove background work has settled.

### Smart Agent Tracking

Know exactly what your agents are doing.

- Track running, blocked, and completed agents
- Get notified when an agent needs attention
- See unread completions
- Switch between active sessions

### Project & Workspace Management

Keep your development environment organized.

- Manage multiple projects
- Split terminal panes
- Organize agent sessions
- Browse project files with Zed’s default file icons, edit text, and review Git diffs
- Stage or unstage files and commit staged changes
- Restore previous workspaces
- Preserve terminal scrollback and conversations

An empty right panel in Agent mode opens a surface picker for Terminal, Files, and Diff. **+** opens the same picker from an existing tab. Terminal tabs take input directly at the shell prompt. Hiding the panel keeps its shells running; restarting Shob restores the tabs and their selection, not the shell processes.

The Files surface restores open files, unsaved drafts, expanded folders, and the commit message. Use Refresh to pick up external changes. Editing supports UTF-8 files up to 1 MiB and eight open tabs; saves refuse to overwrite a file changed on disk. Read-only diff snapshots and scroll positions are transient.

### Sidebar

The sidebar groups running, pinned, active, snoozed, and settled threads. Use the project filter to narrow the list, and the row menu to pin, settle, snooze, archive, or delete a thread. Archived threads can be restored from Settings.

**New thread** opens a project chooser. `Ctrl/Cmd+N` opens the chooser, `Ctrl/Cmd+K` focuses sidebar search, and `Ctrl/Cmd+Shift+B` opens the project filter. Thread metadata is stored in `workspace.json`; project scope and shelf expansion are stored in `settings.json`.

### Make It Yours

Customize Shob to match your workflow.

- Desktop and terminal palettes in Settings → Themes, with light, dark, and system modes in Appearance
- System appearance detection
- Custom theme colors
- Flexible terminal layouts

## Getting Started

Download Shob from [GitHub Releases](https://github.com/olovalabs/shob/releases) or build it from source.

Official builds check for stable Shob updates at launch. From your shell, run `shob update --check` to check without installing, or exit the TUI and run `shob update` to install. In the desktop app, use **Settings → Updates**. Source builds can check for releases there, but only supported official builds can install updates in the app. Linux x86-64 portable installs and AppImages, macOS Apple Silicon portable installs, and Windows x86-64 portable installs use verified binary deltas when available and fall back to the full package. Windows MSI installs upgrade through Windows Installer. Update Linux DEB, RPM, and Arch installs through their package manager.

If your current release predates the updater, install the first release that includes it using your usual installation method.

With `shob` on your `PATH`, run `shob` from any project folder to start the coding-agent TUI in your current terminal. If no provider is connected, use `/login` inside the TUI. For scripts, configure credentials with `shob auth login --provider <id>`. Run `shob gui` to open the desktop workspace, or `shob --help` for CLI options. Both modes ship in the same executable.

Press `Ctrl+V` or `Alt+V` in the TUI to attach a copied image. Each image gets an inline label such as `[Image 1]`. Reference those numbers in your instructions or delete a label to omit its image. Use `Alt+V` if your terminal intercepts `Ctrl+V`.

### Prerequisites

Install [Rust](https://www.rust-lang.org/tools/install) version 1.94.1 or newer.

**Linux dependencies (Ubuntu/Debian)**

```bash
sudo apt install -y libwayland-dev libxkbcommon-dev libfontconfig1-dev libssl-dev cmake pkg-config
```

### Installation

Clone the repository:

```bash
git clone https://github.com/olovalabs/shob.git
cd shob
cargo run
```

`cargo run` opens the desktop app. To try the agent TUI from source, run `cargo run -- tui`. It defaults to the compact Red theme. Use `/theme` to switch palettes.

For an optimized release build:

```bash
cargo build --release
```

To install the command from source, run `cargo install --path . --locked` and make sure Cargo’s `bin` directory is on your `PATH`. For a portable download, put the `shob` executable or AppImage (named `shob`) in a directory on your `PATH`.

## Connect Your Agents

Install and authenticate your preferred AI coding agent CLIs.

Configure Shob integrations:

```bash
shob integration setup
```

Check integration status:

```bash
shob integration status
```

Diagnose configuration issues:

```bash
shob integration doctor
```

To use Shob's native AI chat, switch a terminal pane to **Agent** mode and run `/auth` to connect your model provider.

The first message in a native agent conversation triggers a short title request using the selected model. Shob updates the sidebar before streaming the reply. If the title request fails or times out, the reply still starts and the sidebar uses the prompt text instead.

Reasoning starts folded and can be expanded while the agent runs. If an unfinished Markdown block exceeds 8 KiB, chat temporarily shows its latest 8 KiB as plain text; copying keeps the full message, and full Markdown returns when the message finishes.

For an OpenAI-compatible Chat Completions service, choose **OpenAI-compatible API** in `/auth`. Enter a unique provider ID, display name, base URL (including `/v1` if required), API key, and comma-separated model IDs. The provider and models are saved for future sessions; keys use the same credential store as other providers.

The editor shortcut opens the last editor you chose. If you haven’t chosen one, Shob uses the first supported editor it finds on your PATH.

Project scripts are saved per project in `settings.json` and run in a new terminal tab using the project directory.

Native agent tools support opt-in compact shell results (`output_mode: "compact"`), saved-log inspection with `output`, compact and paged ripgrep search through `grep`, and up to eight independent read-only operations in `batch`, with up to four running at a time. Compact shell results retain diagnostics and link to the complete sanitized log. Raw shell output is the default. The TUI and native agent chat share these tools through the embedded and legacy RPC backends. In the TUI, press `Ctrl+O` to expand the individual results of a batch.

For read-only investigations, the built-in agent can use declarative **codemode** in a `batch` call. For example, `{"calls":[{"tool":"read","path":"Cargo.toml"},{"tool":"read","path":"agent/Cargo.toml"}],"codemode":{"contains":"edition","limit":3}}` returns up to three matching text lines per manifest. Filtering happens before results enter model context. Omit `contains` to take the first lines. Projected calls have a 30-second batch deadline and a 4 KiB text budget per successful result, plus a truncation marker. This mode does not execute JavaScript or shell commands.

Tool-result details include observed duration and displayed bytes, not estimated token savings. Aggregate counters are transient and reset on session start or tool reload; they do not change workspace persistence.

In native Agent chat with the embedded backend, the agent can delegate independent tasks to subagents in parallel. Open a worker to see its live preview and saved transcript. The parent receives each worker’s report and a transcript path for detailed output; if a worker stops without a report, the parent sees why and receives recent partial text when available. Workers inherit the parent’s tools and approval rules.

### Agent Permissions

Shob remembers the approval mode selected in native agent chat across restarts.

Terminal-based agents manage their own permissions independently.

> **Note:** The legacy `SHOB_AGENT_BACKEND=rpc` backend does not enforce Shob's approval modes.

The agent CLI uses `.shob/` for project resources, `~/.shob/agent/` for global configuration, and `SHOB_*` environment variables. TUI sessions use the same global session storage as desktop Agent chat (`~/.local/share/shob/shob/sessions` on Linux), not a folder inside the project. `SHOB_DATA_DIR` relocates that shared storage; `--session-dir`, the saved `sessionDir` setting, or `SHOB_CODING_AGENT_SESSION_DIR` can explicitly override the TUI session location. `SHOB_CODING_AGENT_DIR` overrides the global agent directory. Native extensions must be rebuilt against the Shob plugin SDK.

## Configuration

Shob automatically saves your workspace, terminal scrollback, and agent conversations in its application data directory.

**Custom data directory**

```bash
SHOB_DATA_DIR=/absolute/path
```

**Disable workspace restoration**

```bash
SHOB_DISABLE_RESTORE=1
```

**Disable the token-reduction layer**

```bash
SHOB_TOKEN_EFFICIENCY=0
```

Settings are stored in your platform's configuration directory.

### Idle sessions

A session you are not looking at releases its render caches immediately and, after
a quiet period, hibernates: the terminal grid is dropped and the pane keeps only
its text, a bounded buffer of any output that arrives while it is away, and its
still-running shell. Returning rebuilds the grid from that text, so waking never
waits on the disk, and nothing is killed. A pane running a command, a full-screen
program, or an agent turn stays resident.

```jsonc
"memory": {
  "hibernation": true,
  "idle_after_secs": 0,          // release render caches
  "hibernate_after_secs": 900,   // drop the terminal grid
  "terminal_scrollback_lines": 10000
}
```

For one run: `SHOB_IDLE_SECS`, `SHOB_HIBERNATE_SECS`, and
`SHOB_DISABLE_HIBERNATION=1`. `SHOB_DIAGNOSTICS=1` appends a per-pane memory and
lifecycle line to its once-a-second report.

### Token Efficiency

Native agent chat runs a token-reduction layer: it answers repeated file reads and
identical searches with an "unchanged since turn N" note instead of re-sending
content, accepts line-anchored edits, compresses noisy tool output, and routes
conversation titles to a cheaper model. Per-turn token and cache-hit-rate rows are
appended to `<data>/token-efficiency/turns.jsonl`.

Every capability is switchable from `settings.json`:

```jsonc
"token_efficiency": {
  "enabled": true,
  "read_dedupe": true,
  "search_cache": true,
  "anchor_edits": true,
  "minimize_output": true,
  "route_cheap_tasks": true,
  "metrics": true,
  "dedupe_window_turns": 8,
  "smol_model": null   // e.g. "anthropic/claude-haiku-4-5"
}
```

A stubbed read or search can be forced back to a real one with `{"fresh": true}`.
The design, the cache's invalidation rules, and the measured savings are in
[`src/token_efficiency/`](src/token_efficiency/).

## Built With

- **Rust** — Core application and performance
- **GPUI** — Native desktop interface
- **AI Agent Integrations** — Terminal-based and native agent workflows

## Contributing

Shob is open source, and contributions are welcome.

Whether you're fixing bugs, improving performance, adding integrations, or suggesting features, your contributions help make Shob better.

Read [CONTRIBUTING.md](CONTRIBUTING.md) to get started.

## Documentation

- [Architecture](ARCHITECTURE.md)
- [Contributing Guide](CONTRIBUTING.md)

## License

Shob's original code is dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE).

Bundled Zed file icons and filename associations are licensed under GPL-3.0-or-later. See the [icon notice](assets/zed_file_icons/NOTICE.md).

---

<div align="center">

**Built for developers who work with AI.**

If you find Shob useful, consider giving the project a star.

</div>
