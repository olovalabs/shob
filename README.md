
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

Use terminal-based agents or switch to native AI chat whenever you need it.

### Native Performance

Built with Rust and GPUI instead of Electron or a browser-based interface.

- Native desktop interface
- Lightweight resource usage
- Responsive terminal rendering
- Fast workspace navigation
- Efficient session management

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

The Files surface restores open files, unsaved drafts, expanded folders, and the commit message. Use Refresh to pick up external changes. Editing supports UTF-8 files up to 1 MiB and eight open tabs; saves refuse to overwrite a file changed on disk. Read-only diff snapshots and scroll positions are transient.

### Make It Yours

Customize Shob to match your workflow.

- Light and dark themes
- System appearance detection
- Custom theme colors
- Flexible terminal layouts

## Getting Started

Download Shob from [GitHub Releases](https://github.com/olovalabs/shob/releases) or build it from source.

Official builds check for stable updates at launch. Open **Settings → Updates** to check manually, download an available update, and restart when you can close your running terminals. Linux x86-64 portable installs and AppImages, macOS Apple Silicon portable installs, and Windows x86-64 portable installs use a verified binary delta from the previous release when available. Shob downloads the full package when it cannot use a delta. Windows MSI installs upgrade through Windows Installer. Update Linux DEB, RPM, and Arch installs through their package manager.

If your current release predates the updater, install the first release that includes it using your usual installation method.

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

For an optimized release build:

```bash
cargo build --release
```

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

Reasoning starts folded and can be expanded while the agent runs. If an unfinished Markdown block exceeds 8 KiB, chat temporarily shows its latest 8 KiB as plain text; copying keeps the full message, and full Markdown returns when the message finishes.

For an OpenAI-compatible Chat Completions service, choose **OpenAI-compatible API** in `/auth`. Enter a unique provider ID, display name, base URL (including `/v1` if required), API key, and comma-separated model IDs. The provider and models are saved for future sessions; keys use the same credential store as other providers.

The editor shortcut opens the last editor you chose. If you haven’t chosen one, Shob uses the first supported editor it finds on your PATH.

Project scripts are saved per project in `settings.json` and run in a new terminal tab using the project directory.

Native agent tools support opt-in compact shell results (`output_mode: "compact"`), saved-log inspection with `output`, compact/paged `grep`, and up to eight independent read-only operations in `batch`, with up to four running at a time. Compact shell results retain diagnostics and link to the complete sanitized log. Raw shell output is the default. Native agent chat uses these tools through the embedded and legacy RPC backends.

For read-only investigations, the built-in agent can use declarative **codemode** in a `batch` call. For example, `{"calls":[{"tool":"read","path":"Cargo.toml"},{"tool":"read","path":"agent/Cargo.toml"}],"codemode":{"contains":"edition","limit":3}}` returns up to three matching text lines per manifest. Filtering happens before results enter model context. Omit `contains` to take the first lines. Projected calls have a 30-second batch deadline and a 4 KiB text budget per successful result, plus a truncation marker. This mode does not execute JavaScript or shell commands.

Tool-result details include observed duration and displayed bytes, not estimated token savings. Aggregate counters are transient and reset on session start or tool reload; they do not change workspace persistence.

### Agent Permissions

Shob remembers the approval mode selected in native agent chat across restarts.

Terminal-based agents manage their own permissions independently.

> **Note:** The legacy `SHOB_RPI_BACKEND=rpc` backend does not enforce Shob's approval modes.

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

Settings are stored in your platform's configuration directory.

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
