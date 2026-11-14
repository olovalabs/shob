# Kathamo

> A GPU-accelerated terminal emulator built for developers orchestrating AI coding agents across multiple projects.

Kathamo combines native terminal rendering powered by [GPUI Fast](https://github.com/longbridge/gpui-fast) and [alacritty_terminal](https://github.com/alacritty/alacritty) with an intelligent **Agent Attention Model** and multi-project workspace management. It tracks the state of autonomous AI agents (Claude Code, OpenCode, Codex, Pi, OMP, Command Code) in real time—alerting you when an agent is blocked, waiting for input, or finished while you were away.

---

<img width="1609" height="1043" alt="image" src="https://github.com/user-attachments/assets/7202a427-8b1e-4e09-9433-28a99f398149" />

## Key Features

- **🚀 GPU-Accelerated Terminal Rendering**
  - Powered by GPUI Fast and Alacritty's VTE engine.
  - Adaptive 1 ms stream coalescing with a 128 KiB parse cap handles intensive outputs (e.g., 40 MB/s compilation logs) without locking the UI thread.
  - Full support for ANSI colors (16 standard, 256-color, 24-bit true color), bold, italics, underlines, and custom font glyphs.
  - Displays a plain terminal grid without block UI, with a Warp-style bottom input whenever the shell is idle. The input temporarily hides while a command runs or the alternate screen is active.

- **🤖 Native Agent Attention System**
  - **Agent Attention Protocol**: Strict attention priority hierarchy (`blocked > done > working > idle`).
  - **Unviewed Completion Latching**: If an agent finishes while its tab is not visible or the window is unfocused, it latches to `done` and alerts you. When you switch to it, it transitions cleanly to `idle`.
  - **Audio & Visual Notifications**: Dedicated audio chimes and system toasts for blocked queries and unobserved completions (`KATHAMO_NO_SOUND=1` supported).
  - **Sub-Agent / Worker Hierarchy**: Child sessions indented cleanly beneath parent agents without triggering duplicate alerts.

- **📁 Multi-Project Sidebar & Session Persistence**
  - Group terminal sessions under collapsible project folders in the sidebar, with the newest session first.
  - Restores the full workspace on relaunch: projects, tabs, split panes, focus, per-pane working directories, input drafts, command history, window geometry, and the previous terminal output.
  - Agent conversations are saved to disk; rpi sessions resume on the next prompt.
  - Quick-action buttons to create new terminal sessions per project or open existing directories.

- **Project changes**
  - The agent toolbar shows the project's changed-file count and line totals. Select **Changes** there or in a pane header, or run `/diff` in agent chat, to open a Git diff beside your work.
  - Review per-file patches and line counts against `HEAD` (or an empty tree before the first commit), grouped into collapsible tracked and untracked sections. Ignored untracked files are excluded; changed dotfiles and lockfiles remain visible. Use **Refresh** to reload the snapshot.
  - Click a filename to read it in a syntax-highlighted, read-only editor; select the arrow beside it to expand its patch. Deleted files open their last committed version. Links and project changes reuse the same preview pane beside their source.
  - File previews mark additions, changes and deletions in the gutter against Git HEAD, including staged and unstaged changes. The markers reflect the file when it was opened.
  - The preview includes Tree-sitter highlighting for languages such as Rust, Python, JavaScript, TypeScript/TSX, Go, C/C++, C#, Java, Swift, Ruby, SQL, HTML, CSS, JSON, YAML, TOML, and shell scripts. Unsupported file types are shown as plain text.

- **🔌 First-Class Agent Integrations & CLI**
  - Automated one-command setup for popular agent CLIs:
    ```bash
    kathamo integration setup
    ```
  - Built-in hook support for **Claude Code**, **OpenCode v2**, **Codex**, **Pi**, **OMP**, and **Command Code** (native Kathamo compatibility).
  - Explicit CLI reporting via `kathamo report <state> [message]` or standard OSC 7501 escape sequences.

- **🖥️ Desktop Polish**
  - Native window decorations and custom title bar controls.
  - Bundled custom application icons for Linux (`.desktop` auto-registration), macOS, and Windows.
  - Light, dark, and system appearance modes with Warp and GitHub terminal palettes.
  - Compatible terminal applications can follow light/dark changes while running using DEC mode 2031 and OSC 11.

---

## Architecture Overview

```mermaid
graph TD
    UI[GPUI Application Window] --> Sidebar[Multi-Project Sidebar]
    UI --> TerminalView[GPUI Terminal View]

    TerminalView --> PTY[portable-pty System]
    TerminalView --> VTE[alacritty_terminal VTE Parser]

    Agent[AI Agent Process] -- Unix Socket / OSC 7501 --> Bus[Status Bus IPC]
    Bus --> Engine[Attention State Machine]
    Engine --> Attention[Badge / Sound / Toast]
    Attention --> Sidebar
    Attention --> UI
```

---

## Where Kathamo stores data

Kathamo uses the platform-native application directories (the same convention as Zed):

| Platform | Data | Settings |
|---|---|---|
| Linux | `~/.local/share/kathamo` | `~/.config/kathamo/settings.json` |
| macOS | `~/Library/Application Support/Kathamo` | `~/.config/kathamo/settings.json` |
| Windows | `%LOCALAPPDATA%\Kathamo` | `%APPDATA%\Kathamo\settings.json` |

### Custom theme colors

On first launch Kathamo writes a complete default `theme_colors` object into
the user settings file listed above. Edit `theme_colors.light.style` and
`theme_colors.dark.style` to change colors, using dotted keys such as
`window.bg` and `terminal.ansi.red`. `theme` selects `light`, `dark`, or
`system`. Colors use `#RRGGBB` or `#RRGGBBAA`; terminal colors must be opaque.
In Settings, choose **Open settings.json**, edit and save the file, then choose
**Reload theme** to apply theme changes. For an existing
settings file, add `theme_colors` with `light` and `dark` objects containing
`style` maps; omitted values use the selected palette defaults.

The structure is inspired by Zed's dotted theme styles, but Kathamo's token
names are its own and it does not yet support importing Zed theme files. These
tokens cover Kathamo's existing UI tokens and terminal palette; some component
library styles and colors defined directly in views are not yet configurable.

The data folder holds `workspace.json` (projects, tabs, split layout, cwd), `sessions/` (per-pane terminal output), `conversations/` (agent transcripts), `rpi/sessions/` (RPI histories grouped by project), and `backups/`. Manage or clear it from **Settings → Data & Sessions**, or override the location with `KATHAMO_DATA_DIR=/absolute/path`. Set `KATHAMO_DISABLE_RESTORE=1` for a clean launch without reading saved state.

Deleting a sidebar session removes its saved workspace entry, pane scrollback, Kathamo conversation transcripts, and its embedded RPI session files. Other command-line agent histories and standalone RPI sessions are not removed. Existing project-local RPI sessions are left in place; an embedded session is copied to the data folder when resumed. Standalone RPI uses the same default location, with `--session-dir`, the `sessionDir` setting, and `RPI_CODING_AGENT_SESSION_DIR` available as overrides.

Processes are not resurrected: a restored pane starts a fresh shell in the saved working directory with the previous output shown above the new prompt.

---

## Installation & Building

### Prerequisites

- **Rust**: Version 1.94.1+ (native cloud credential SDKs)
- **Linux Packages**:
  ```bash
  sudo apt install -y libwayland-dev libxkbcommon-dev libfontconfig1-dev libssl-dev cmake pkg-config
  ```
- **macOS**: Xcode Command Line Tools.

### Build from Source

```bash
# Clone the repository
git clone https://github.com/olovalabs/kathamo.git
cd kathamo

# Run the development build
cargo run

# Build the release binary
cargo build --release

# Run Kathamo
./target/release/kathamo
```

### Native agent chat

Kathamo's native agent view runs the embedded Rust [pi-rust](https://github.com/bigfish1913/pi-rust)
backend in the app process. Its native provider catalog includes all 67 integrations
listed in the Oh My Pi README, without running or importing the OMP executable.
The frozen catalog has 104 provider identities and 6,279 model entries, including
5,995 chat models; existing Kathamo models and configured `models.json` providers
are preserved. Non-chat entries retain their capability metadata but are not
offered as agent chat models.

In a terminal pane, select **Agent** in the bottom input bar to open a native
chat view. Enter sends a prompt, Shift+Enter inserts a newline, and Stop or
Ctrl+C cancels a running request. Select **Terminal** to return to the shell;
each pane keeps its own agent conversation while it remains open. Answer text
and reasoning/thinking appear incrementally as the agent sends them, rendered as
Markdown with syntax-highlighted code blocks. Thinking blocks fold into compact,
clickable accordions once the agent begins answering. Tool calls display all execution
details by default in rich cards: tool badges, call IDs (`#id`), inline argument summaries,
status badges, and line counts. Cards present pretty-printed input parameters and
streaming output blocks with dedicated one-click copy buttons for both arguments and results.
Retry or compaction notices appear as activity updates. Chat autoscrolling stays
pinned to the bottom during live streaming and asynchronous Markdown layout changes;
scrolling up pauses autoscroll and shows a floating "Scroll to bottom" button.
Kathamo opens an embedded session when you enter Agent mode or choose a model.
For Gemini 3.8 Flash, **Fast** uses low reasoning and **Deep** uses high reasoning.
Use `/thinking low|medium|high` to set a per-pane override.

Start a prompt with `@browser` for autocomplete, or include the mention anywhere
in a prompt, for example `@browser open example.com and summarize it`. Release
builds bundle [agent-browser](https://github.com/vercel-labs/agent-browser). On
first use, the agent checks for Chrome for Testing and downloads it if missing;
this uses the shell tool and normal tool approvals apply. The browser is headless
by default. Linux may need additional system libraries. Source builds without a
bundled binary use agent-browser from PATH; to embed one, set
`KATHAMO_AGENT_BROWSER_BIN` to the platform's native executable when building.

Type `/auth` in agent mode to open provider authentication. API-key providers
offer **Save Key & Connect**; providers with native browser/device sign-in also
offer **Connect with OAuth**. Both methods remain available when supported.
Credentials and renewed tokens use the embedded backend's private `auth.json`;
existing Antigravity credentials retain their store. Input is masked. Installed
authentication extensions remain supported, with project-scoped capability discovery.
Readiness reports local configuration, not account entitlement or upstream acceptance.
A ready provider can be selected without signing in again; **Add account** explicitly
opens authentication. Signing in does not switch an existing conversation's model.

Local Ollama, LM Studio, llama.cpp, vLLM and LiteLLM rosters are discovered from
their running servers. SiliconFlow, Otari and Charm Hyper also discover the account/server
roster instead of inventing model IDs. Override endpoints and models in
`~/.rpi/agent/models.json` (`RPI_CODING_AGENT_DIR` overrides that directory).
Existing explicit endpoints, headers and model definitions take precedence.

Amazon Bedrock supports API bearer tokens and the native AWS credential chain
(environment, profiles, SSO, web identity, assumed roles and container/instance roles).
Set the intended `AWS_REGION` or configure the selected AWS profile's region.
Vertex supports API keys or Google Application Default Credentials; configure
`GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION` for project-routed requests.
Cloud-specific resource/deployment settings still need to match your account.

Gemini CLI browser OAuth reads `KATHAMO_GEMINI_CLI_CLIENT_ID` and
`KATHAMO_GEMINI_CLI_CLIENT_SECRET` from your local environment. Configure a desktop
OAuth client with redirect `http://127.0.0.1:8085/oauth2callback`; client credentials
are not embedded in the repository.

Select **Models** in the agent input bar or enter `/model` to see the available
rpi and Kathamo models. Select a row for the current pane, or
choose **Set default** to use that provider and model in new panes. Each pane
keeps the model shown in its input bar across restarts, so panes can run
different models side by side. Each selected model has a separate conversation
within the pane. The first prompt supplies the session title.

#### Antigravity models

For Antigravity, open `/auth`, select **google-antigravity**, and choose
**Connect with OAuth (Browser)**. After Google sign-in, choose a model in `/model`:
`google-antigravity/gemini-3.8-flash` (the default),
`google-antigravity/claude-sonnet-4-6`,
`google-antigravity/gemini-3.1-flash-lite`,
`google-antigravity/gemini-3.5-flash-lite`,
`google-antigravity/gemini-2.5-flash`, or
`google-antigravity/gemini-2.5-flash-lite`. The Flash-Lite and Claude routes
stay responsive when the Gemini 3.8 Flash Cloud Code endpoint is congested.
The `⚡ Fast`/`🧠 Deep` toggle and `/thinking` apply to all Antigravity models.
Kathamo exchanges and refreshes the token in its embedded backend. Sign in once
to create the native credential store. Existing Kathamo transcripts remain
readable.

Configure the desktop OAuth client through `KATHAMO_ANTIGRAVITY_CLIENT_ID` and
`KATHAMO_ANTIGRAVITY_CLIENT_SECRET`, or set `client_id` and `client_secret`
in `~/.rpi/agent/antigravity-oauth-client.json`. If you set
`RPI_CODING_AGENT_DIR`, place the file there. The client needs the loopback
redirect `http://127.0.0.1:51121/oauth-callback`.

This integration follows the [Oh My Pi Antigravity wire protocol](https://github.com/can1357/oh-my-pi).
Google's [Antigravity terms](https://www.antigravity.google/terms) restrict
third-party use of personal OAuth. The public Gemini API is also available as
`google/gemini-3.8-flash` using a [Gemini API key](https://ai.google.dev/gemini-api/docs/api-key)
through `/auth` or `GEMINI_API_KEY`.

Gemini's built-in file reads and searches run directly. Kathamo asks before
running other tools, including shell commands, edits and writes.

#### Measuring latency

Set `KATHAMO_STREAM_TIMINGS=1` to print metadata-only timings for session
startup, first reasoning and answer deltas, UI application and paint, and tool
execution. To exercise the local streaming and tool fixtures, build the test
executable with `cargo test --locked --bin kathamo --no-run` and pass its path
to `scripts/bench-agent-stream.py`.


---

## CLI Usage Reference

Kathamo acts both as a standalone GUI terminal and as a command-line helper for running shell sessions and agent scripts:

### Status Reporting

Report agent state from inside any terminal pane:

```bash
# Report active task
kathamo report working "Refactoring authentication module" --progress 35

# Request user decision or approval
kathamo report blocked "Waiting for API credentials" --kind auth

# Mark task completion
kathamo report done "All tests passed"

# Signal an error
kathamo report error "Compilation failed on status.rs"

# Clear status
kathamo report clear
```

### Agent Integrations Management

Configure agent CLI hooks automatically:

```bash
# Register hooks for Claude Code, Codex, Pi, OMP, and OpenCode
kathamo integration setup

# Inspect integration status across installed agents
kathamo integration status

# Run diagnostic verification
kathamo integration doctor

# Remove Kathamo integration hooks cleanly
kathamo integration remove
```

### Manual Status Inspection

```bash
# Query active terminal status
kathamo status

# Explain current state inference and detection evidence in JSON
kathamo status explain --json

# Clear current pane status
kathamo status clear
```

---

## Testing & Verification

Run the comprehensive test suites:

```bash
# Run core test suite
cargo test

# Run terminal emulator component tests
cargo test -p terminal

# Run OpenCode TUI integration tests (requires bun)
bun test tests/integrations/opencode-tui.test.ts

# Interactive terminal status demo script
./scripts/status-demo.sh working
./scripts/status-demo.sh blocked
./scripts/status-demo.sh done
```

---

## Repository Structure

```
kathamo/
├── assets/             # Icons, sounds, themes, embedded agent rule manifests
├── components/         # GPUI UI component library (fork of gpui-component)
├── terminal/           # GPU-accelerated terminal emulator widget
├── scripts/            # Development and demo utility scripts
├── src/                # Core application source
│   ├── attention.rs    # Agent attention state machine & priority ladder
│   ├── cli.rs          # CLI parser and command handlers
│   ├── integrations.rs # Automated agent hook configuration
│   ├── main.rs         # Application entry point, window & view layout
│   ├── conversations.rs # Append-only agent conversation history
│   ├── paths.rs        # Per-platform data/config directories
│   ├── settings.rs     # User settings & window geometry
│   ├── status.rs       # Agent status schemas & state definitions
│   ├── status_bus.rs   # Unix domain socket IPC bus
│   └── workspace_state.rs # Workspace/session/scrollback persistence
└── tests/              # End-to-end and integration tests
```

---

## Contributing

We welcome community contributions! Please read [CONTRIBUTING.md](CONTRIBUTING.md) for environment setup and contribution guidelines.

---

## License

Dual-licensed under either:

- **MIT License** ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option for Kathamo's original code. The distributed application also
embeds Zed's default file icons and uses its filename associations, which are
provided under GPL-3.0-or-later. See [the Zed icon notice](assets/zed_file_icons/NOTICE.md)
and [GPL-3.0 license](assets/zed_file_icons/LICENSE-GPL) for those components.
