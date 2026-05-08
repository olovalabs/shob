# Shob - Run multiple CLI agents across sessions within a single project

<img width="1442" height="987" alt="image" src="https://github.com/user-attachments/assets/0eadb616-2a62-415b-b0a8-482faaad7261" />

Shob is a desktop app for running and organizing multiple CLI agents in one workspace.
It helps you keep parallel sessions focused, persistent, and easy to switch between.

## Why Shob

- Run multiple CLI sessions side by side.
- Keep session context inside the same project.
- Switch quickly between tasks without losing flow.
- Use a desktop-first experience powered by Electron.

## Tech Stack

- React 19 + TypeScript
- Vite
- Electron
- Bun

## Project Structure

- `src/`: React UI and client logic
- `src/components/`: UI components and app views
- `electron/`: Electron main/preload process code
- `.github/workflows/`: Build and release workflow

## Requirements

- Node.js 22+
- Bun

## Quick Start

```bash
bun install
bun run electron
```

## Build

```bash
bun run build
bun run build:electron
```

## Scripts

- `bun run dev`: Start Vite dev server
- `bun run electron`: Run Electron app in development
- `bun run build`: Type-check and build frontend
- `bun run build:electron`: Build desktop installers with electron-builder
- `bun run lint`: Run ESLint
- `bun run preview`: Preview production frontend build

## Release

GitHub Actions builds desktop installers when you push a version tag.

```bash
git tag vX.Y.Z
git push origin vX.Y.Z
```

Workflow output includes platform builds for Windows, macOS, and Linux.

## License

Proprietary or internal use by default unless you add a license file.

