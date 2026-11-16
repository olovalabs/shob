
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
- Review Git changes
- Restore previous workspaces
- Preserve terminal scrollback and conversations

### Make It Yours

Customize Shob to match your workflow.

- Light and dark themes
- System appearance detection
- Custom theme colors
- Flexible terminal layouts

## Getting Started

Shob currently supports building from source.

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
