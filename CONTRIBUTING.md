# Contributing to Kathamo

Thank you for your interest in contributing to Kathamo! Kathamo is an open-source, GPU-accelerated terminal emulator designed for orchestrating autonomous AI coding agents across multiple development projects.

This guide outlines our development workflow, standards, and testing procedures.

---

## Code of Conduct

All contributors and participants must adhere to our [Code of Conduct](CODE_OF_CONDUCT.md). Please report unacceptable behavior according to the procedures described in that document.

---

## Development Prerequisites

Ensure your machine has the required toolchains and dependencies installed:

1. **Rust Toolchain**:
   - Rust 1.94.1+ (required by the native cloud credential SDKs).
   - Install via [rustup](https://rustup.rs/):
     ```bash
     rustup update stable
     ```

2. **System Dependencies (Linux)**:
   ```bash
   sudo apt update
   sudo apt install -y \
       libwayland-dev \
       libxkbcommon-dev \
       libfontconfig1-dev \
       libssl-dev \
       cmake \
       pkg-config
   ```

3. **JavaScript / TypeScript Runtime (for integration tests)**:
   - [Bun](https://bun.sh/) is used to run integration tests for TUI hooks:
     ```bash
     curl -fsSL https://bun.sh/install | bash
     ```

---

## Getting Started

1. **Fork and Clone**:
   ```bash
   git clone https://github.com/<your-username>/kathamo.git
   cd kathamo
   ```

2. **Check Out a Branch**:
   ```bash
   git checkout -b feature/my-new-feature
   ```

3. **Build the Project**:
   ```bash
   cargo build
   ```

4. **Run the Application**:
   ```bash
   cargo run
   ```

---

## Testing Guidelines

Every change must be validated through our automated test suites and relevant smoke tests:

### 1. Cargo Test Suites
Run the core application and terminal component tests:

```bash
# Run application unit and integration tests
cargo test

# Run terminal emulator widget tests
cargo test -p terminal
```

### 2. Integration Tests
Validate agent plugin and TUI bridge implementations:

```bash
bun test tests/integrations/opencode-tui.test.ts
```

### 3. Interactive Smoke Testing
Test the agent attention system, visual badges, and alert sounds:

```bash
# Test various terminal states
./scripts/status-demo.sh active
./scripts/status-demo.sh working
./scripts/status-demo.sh blocked
./scripts/status-demo.sh error
./scripts/status-demo.sh done
```

---

## Coding Standards & Invariants

- **Rust Edition 2024**: Use modern Rust patterns and idioms. Keep code clean, idiomatic, and avoid unnecessary dependencies.
- **UI Responsiveness**: The terminal must never freeze or drop below smooth interactive framerates. Heavy I/O, process scanning (e.g. `/proc`), and parsing must be bounded or offloaded from the UI thread.
- **Agent Attention Model Invariants**:
  - Parity with the Kathamo protocol: `blocked > done > working > idle`.
  - An agent completion seen live transitions directly to `idle`/`active`. Only unviewed completions latch to `done`.
  - Never infer `done` simply from silence or inactivity.
- **Security**: The status IPC Unix domain socket must enforce per-user permissions and authenticate pane generation IDs to prevent cross-session injection.
- **Formatting**: Format all code using standard `cargo fmt` before submitting.

---

## Submitting a Pull Request

1. Ensure all tests pass cleanly (`cargo test`).
2. Verify code formatting and linting:
   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   ```
3. Commit your changes with descriptive, conventional commit messages:
   - `feat: add support for custom agent status markers`
   - `fix: prevent duplicate alerts for nested worker sessions`
   - `perf: optimize terminal VTE line rendering`
4. Open a Pull Request against the `main` branch with a clear description of the problem, the solution, and verification steps performed.
