# Security Policy

## Supported Versions

Security updates are applied to the latest release of Kathamo.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |
| < 0.1.0 | :x:                |

---

## Reporting a Vulnerability

If you discover a security vulnerability within Kathamo, please **do not** open a public issue. Instead, report it responsibly via one of the following methods:

1. **GitHub Security Advisory**: Open a private advisory on the repository under the **Security** tab.
2. **Email**: Send vulnerability details to `security@olovalabs.com` or contact repository maintainers directly.

### What to Include in Your Report

To help us triage and remediate the issue promptly, please include:
- A detailed description of the vulnerability and its potential impact.
- Step-by-step reproduction instructions or a minimal proof-of-concept (PoC).
- Your operating system, desktop environment, and the version of Kathamo tested.
- Any suggested mitigations or patches if available.

### Disclosure Timeline

- **Initial Response**: We will acknowledge receipt of your report within 48 hours.
- **Triage & Status**: We will provide a status update on reproduction and severity assessment within 5 business days.
- **Remediation**: Once a fix is verified, a patched release will be published and an advisory issued with credit to the reporter.

---

## Security Model & Considerations

Kathamo executes terminal processes and communicates with external AI agent runtimes. The following security mechanisms are enforced:

### 1. IPC Unix Domain Socket Security
- Kathamo binds a private Unix domain socket restricted to the current user (under `$XDG_RUNTIME_DIR/` or a `0700`-permission directory in `/tmp`).
- Every terminal pane is assigned a unique, random `pane_generation` identifier.
- The status bus verifies that all incoming events match the live `pane_generation` and originate from authenticated pane environments, preventing cross-session spoofing.

### 2. Terminal Escape Sequence Sanitization
- Terminal emulation is handled through `alacritty_terminal` with strict VTE parsing rules.
- Clipboard OSC 52 read/write sequences and custom OSC 7501 status reports are bounded, parsed with strict schemas, and validated against size caps.

### 3. Agent Integration Configuration Safety
- When running `kathamo integration setup`, existing agent configurations (e.g. `~/.claude/settings.json`, `~/.codex/hooks.json`) are backed up before any modifications.
- Extensions and hooks only report to terminals launched under Kathamo that supply the authenticated environment variables.
