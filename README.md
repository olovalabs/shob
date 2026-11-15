# Kathamo

**A fast, native terminal for running AI coding agents across your projects.**

Kathamo brings your terminals and agent chats together, keeps projects and sessions organized, and lets you know when an agent needs you or finishes while you’re away.

<p align="center">
  <img width="1000" alt="Kathamo desktop app" src="https://github.com/user-attachments/assets/9e09e9f1-af06-4df6-bfc2-c86e305b09d6" />
</p>

## What you can do

- **Run agents side by side.** Use Claude Code, Codex, Pi, OpenCode, OMP, and Command Code in terminal panes, or open Kathamo’s built-in agent chat.
- **See what needs attention.** Kathamo tracks when agents are working, blocked, or done, and surfaces completions you haven’t seen.
- **Keep your place.** Group sessions by project, split panes, review Git changes, and restore your workspace when you return.
- **Make it yours.** Choose light, dark, or system appearance and customize theme colors.

## Get started

Kathamo currently builds from source. Install [Rust](https://www.rust-lang.org/tools/install) 1.94.1 or newer, then:

```bash
git clone https://github.com/olovalabs/kathamo.git
cd kathamo
cargo run
```

On Linux, install the system dependencies first:

```bash
sudo apt install -y libwayland-dev libxkbcommon-dev libfontconfig1-dev libssl-dev libglib2.0-dev cmake pkg-config
```

For a release build, run `cargo build --release`.

## Connect your agents

Install and sign in to the agent CLIs you use, then let Kathamo configure their integrations:

```bash
kathamo integration setup
```

Use `kathamo integration status` to check them, or `kathamo integration doctor` to diagnose setup issues. In a terminal pane, switch to **Agent** to start a native chat; use `/auth` to connect a model provider.

The approval mode selected in native agent chat applies to all chats and is remembered on restart. Agents running separately in terminal panes manage their own permissions. The legacy `KATHAMO_RPI_BACKEND=rpc` backend does not enforce Kathamo's approval modes.

## Data and settings

Kathamo saves your workspace, terminal scrollback, and agent conversations in its application data directory. Settings live in the platform’s config directory. Use `KATHAMO_DATA_DIR=/absolute/path` to choose a data location, or `KATHAMO_DISABLE_RESTORE=1` to start without restoring a saved workspace.

For architecture and development details, see [ARCHITECTURE.md](ARCHITECTURE.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Kathamo’s original code is dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE). Bundled Zed file icons and filename associations are provided under GPL-3.0-or-later; see the [icon notice](assets/zed_file_icons/NOTICE.md).
