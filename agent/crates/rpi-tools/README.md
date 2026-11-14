# rpi-tools

[![crates.io](https://img.shields.io/crates/v/rpi-tools.svg)](https://crates.io/crates/rpi-tools)
[![docs.rs](https://docs.rs/rpi-tools/badge.svg)](https://docs.rs/rpi-tools)

The built-in agent tools and the execution environment they run against:
`read`, `write`, `edit`, `bash`, plus `grep`, `find`, `ls` and `powershell`,
implemented as `AgentTool`s that any agent built on
[`rpi-agent`](https://crates.io/crates/rpi-agent) can use.

The point of the crate is the split between *what a tool does* and *where it
runs*. Tools are written against `ExecutionEnv`, so the same `read` tool works
against the real filesystem, an in-memory fake, or a remote backend — and the
test suite never touches your disk.

## Install

```toml
[dependencies]
rpi-tools = "0.3"
rpi-agent = "0.3"
```

## Execution environments

| Backend | Purpose |
| ------- | ------- |
| `OsExecutionEnv` | The real filesystem and real shell processes; resolves relative paths against a configurable cwd |
| `InMemoryExecutionEnv` | A fake filesystem plus scripted shell responses, for deterministic tests |

Both implement `ExecutionEnv`; mutating backends additionally implement
`MutatingEnv`. Tools are constructed from an `ExecutionToolContext` that carries
the environment:

```rust
use std::sync::Arc;

use rpi_tools::{create_bash_tool, create_write_tool, ExecutionToolContext, OsExecutionEnv};

let env = Arc::new(OsExecutionEnv::with_cwd("/tmp/workspace".into()));
let ctx = ExecutionToolContext::new(
    env.clone() as Arc<dyn rpi_tools::ExecutionEnv>,
    Some(env as Arc<dyn rpi_tools::MutatingEnv>),
);

let write_tool = create_write_tool(&ctx);
let bash_tool = create_bash_tool(&ctx, None);

let agent = rpi_agent::AgentBuilder::new()
    .model(model)
    .stream_fn(stream_fn)
    .tools(vec![write_tool, bash_tool])
    .build()?;
```

A complete, runnable version — a faux provider that writes a file through the
real OS backend and reads it back — is in
[`examples/tools`](https://github.com/bigfish1913/pi-rust/tree/main/examples/tools)
(`cargo run -p tools`).

## Testing against the in-memory backend

```rust
use std::sync::Arc;
use rpi_tools::{ExecutionToolContext, InMemoryExecutionEnv, MutationQueueRegistry};

let env = Arc::new(InMemoryExecutionEnv::new());
let ctx = ExecutionToolContext::new(
    env.clone() as Arc<dyn rpi_tools::ExecutionEnv>,
    Some(env as Arc<dyn rpi_tools::MutatingEnv>),
);
let read = rpi_tools::create_read_tool(&ctx, None);
```

Because the backend is in memory, a test asserts on tool *behaviour* rather than
on whatever happens to be in the working directory, and it runs in parallel with
every other test.

## Safety properties this crate is responsible for

- **Path containment.** Resolving a tool argument into a path is centralized in
  `path_utils`, so a tool cannot silently escape the environment root.
- **Mutation ordering.** `file_mutation_queue` serializes writes to the same
  path, so two concurrent `edit` calls cannot interleave into a corrupted file.
- **Bounded output.** Shell and file output are truncated through `truncate` +
  `shell_output` before reaching the model, so one `cat` of a huge log cannot
  blow up the context window.
- **Cancellation.** A blocking shell command is tied to the run's cancellation
  token, including process-tree termination on Windows.

## Tools available

`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, `powershell`.
`create_*_tool` constructors all take the shared `ExecutionToolContext`;
`create_read_tool` and `create_bash_tool` additionally take an options argument
(`None` for the defaults).

The `rpi` CLI loads `read`, `write`, `edit` and `bash` by default — the
Pi-compatible set — and exposes the rest through the `default_tools` setting.

### Shell execution

Bash and native PowerShell use `tokio::process::Command` with stdin, both output
pipes, child status, deadlines and cancellation driven concurrently. The bounded
`ShellExecOptions::output` channel must be consumed concurrently; synchronous
stdout/stderr callbacks remain available. Idle streams do not reserve channel
capacity. Dropped/aborted calls terminate their process groups; normally
completed calls may intentionally leave background descendants alive.

The capture layer maintains a bounded display tail and persists the complete
sanitized stream after display truncation, batching existing backlogs up to
64 KiB without delaying sparse writes. First progress is immediate; Bash and
PowerShell coalesce later checkpoints with one 16 ms timer, including when
output goes quiet. `RPI_BASH_UPDATE_THROTTLE_MS` overrides that window.
`ShellCaptureOptions::update_interval = None` retains per-chunk callbacks for
other capture consumers.

Post-exit drain allows 100 ms plus at most 1 MiB of immediately ready bytes per
pipe, not arbitrary future descendant output. Slow storage can delay final log
delivery, but child termination remains independently driven. A failed spool is
never advertised as a complete log.

Native benchmark:
`cargo run --manifest-path agent/Cargo.toml -p rpi-tools --example bench_shell -- 15`
from the Kathamo root. See the root `SHELL_TOOL_AUDIT.md` for scope and results.

### Content search

`grep` uses ripgrep's embedded `grep-regex` and `grep-searcher` crates, not an
external `rg` process or a separate custom matching loop. Native files stream
through a reusable search buffer on a blocking worker. Directory discovery is
incremental and stops at the match/output limit instead of snapshotting the
entire tree. Virtual filesystems supply bytes to the same ripgrep engine.

The existing pattern, literal, case, glob, context and limit arguments remain.
Searches retain deterministic breadth-first file order and include hidden
files while honoring ignore rules and excluding `.git` contents. As with
ripgrep, NUL detection stops binary-file searches, CRLF line endings are
supported, and overlapping context windows are merged. Match counts are
global across files; trailing context is retained at the match limit. Output
remains capped at 50 KiB with individual lines capped at 500 characters.

## Related crates

| Crate | Role |
| ----- | ---- |
| [`rpi-agent`](https://crates.io/crates/rpi-agent) | The `AgentTool` trait these tools implement |
| [`rpi-ai`](https://crates.io/crates/rpi-ai) | Message and tool-schema types |
| [`rpi-harness`](https://crates.io/crates/rpi-harness) | Sessions and the run loop that drives them |
| [`rpi-cli`](https://crates.io/crates/rpi-cli) | The terminal agent |

## Documentation

- Crate docs: <https://docs.rs/rpi-tools>
- Architecture: [docs/architecture/overview.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture/overview.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
