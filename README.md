# Shob - Run multiple CLI agents across sessions within a single project

<img width="1442" height="987" alt="image" src="https://github.com/user-attachments/assets/0eadb616-2a62-415b-b0a8-482faaad7261" />


Shob is a desktop app for running and organizing multiple CLI agents in one workspace.
It helps you keep parallel sessions focused, persistent, and easy to switch between.

## Why Shob

- Run multiple CLI sessions side by side.
- Keep session context inside the same project.
- Switch quickly between tasks without losing flow.
- Use a desktop-first experience powered by Tauri.

## Tech Stack

- React 19 + TypeScript
- Vite
- Tauri v2 (Rust backend + desktop packaging)
- pnpm

## Project Structure

- `src/`: React UI and client logic
- `src/components/`: UI components and app views
- `src-tauri/`: Tauri config and Rust source
- `.github/workflows/tauri-build.yml`: Build and release workflow

## Requirements

- Node.js 22+
- pnpm
- Rust toolchain (stable)
- Tauri system dependencies (platform-specific)

## Quick Start

```bash
pnpm install
pnpm dev
```

To run as a desktop app during development:

```bash
pnpm tauri dev
```

## Build

```bash
pnpm build
pnpm tauri build
```

## Scripts

- `pnpm dev`: Start Vite dev server
- `pnpm build`: Type-check and build frontend
- `pnpm lint`: Run ESLint
- `pnpm preview`: Preview production frontend build
- `pnpm tauri`: Run Tauri CLI commands

## Release

GitHub Actions builds desktop installers when you push a version tag.

```bash
git tag vX.Y.Z
git push origin vX.Y.Z
```

Workflow output includes platform builds for Windows, macOS, and Linux.

## Notes

- Replace the cover image URL with your own branding when ready.
- Keep frontend and Tauri versions aligned in `package.json` and `src-tauri/tauri.conf.json`.

## License

Proprietary or internal use by default unless you add a license file.
