//! The `pi` binary entry point. Mirrors the TS
//! `packages/coding-agent/src/cli.ts` (the `#!/usr/bin/env node` shebang file
//! whose `bin` entry in `package.json` resolves to `dist/cli.js`, which just
//! calls `main(process.argv.slice(2))`).
//!
//! Rust's analogue: a thin `#[tokio::main]` wrapper that boots the multi-thread
//! runtime the harness needs (the harness's `StreamFn` bridge uses
//! `block_in_place`, which requires the multi-thread flavor) and hands control
//! to [`rpi_cli::main::run`], exiting with its returned code.

use std::process::ExitCode;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // The harness run loop uses `block_in_place` to drive the provider stream
    // synchronously inside the async lane; that requires the multi-thread
    // runtime (configured above). `run` does everything else: arg parse, model
    // resolution, harness build, mode dispatch.
    let code = rpi_cli::app::run().await;
    ExitCode::from(code as u8)
}
