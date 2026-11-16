# Shob

**A fast, native terminal for running AI coding agents across your projects.**

Shob brings your terminals and agent chats together, keeps projects and sessions organized, and lets you know when an agent needs you or finishes while you’re away.

<p align="center">
    <img width="1489" height="1056" alt="image" src="https://github.com/user-attachments/assets/0493d9f2-9a6a-4cde-9572-477047bff1c1" />
</p>

## What you can do

- **Run agents side by side.** Use Claude Code, Codex, Pi, OpenCode, OMP, and Command Code in terminal panes, or open Shob’s built-in agent chat.
- **See what needs attention.** Shob tracks when agents are working, blocked, or done, and surfaces completions you haven’t seen.
- **Keep your place.** Group sessions by project, split panes, review Git changes, and restore your workspace when you return.
- **Make it yours.** Choose light, dark, or system appearance and customize theme colors.

## Get started

Shob currently builds from source. Install [Rust](https://www.rust-lang.org/tools/install) 1.94.1 or newer, then:

```bash
git clone https://github.com/olovalabs/kathamo.git
cd shob
cargo run
```

On Linux, install the system dependencies first:

```bash
sudo apt install -y libwayland-dev libxkbcommon-dev libfontconfig1-dev libssl-dev cmake pkg-config
```

For a release build, run `cargo build --release`.

## Connect your agents

Install and sign in to the agent CLIs you use, then let Shob configure their integrations:

```bash
shob integration setup
```

Use `shob integration status` to check them, or `shob integration doctor` to diagnose setup issues. In a terminal pane, switch to **Agent** to start a native chat; use `/auth` to connect a model provider.

The approval mode selected in native agent chat applies to all chats and is remembered on restart. Agents running separately in terminal panes manage their own permissions. The legacy `SHOB_RPI_BACKEND=rpc` backend does not enforce Shob's approval modes.

## Data and settings

Shob saves your workspace, terminal scrollback, and agent conversations in its application data directory. Settings live in the platform’s config directory. Use `SHOB_DATA_DIR=/absolute/path` to choose a data location, or `SHOB_DISABLE_RESTORE=1` to start without restoring a saved workspace.

For architecture and development details, see [ARCHITECTURE.md](ARCHITECTURE.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Shob’s original code is dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE). Bundled Zed file icons and filename associations are provided under GPL-3.0-or-later; see the [icon notice](assets/zed_file_icons/NOTICE.md).
