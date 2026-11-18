# tools

[![crates.io](https://img.shields.io/crates/v/rpi-tools.svg)](https://crates.io/crates/rpi-tools)
[![docs.rs](https://docs.rs/rpi-tools/badge.svg)](https://docs.rs/rpi-tools)

The built-in agent tools and the execution environment they run against:
`read`, `write`, `edit`, `bash`, plus `grep`, `find`, `ls` and `powershell`,
implemented as `AgentTool`s that any agent built on
[`agent`](https://crates.io/crates/rpi-agent) can use.

The point of the crate is the split between *what a tool does* and *where it
runs*. Tools are written against `ExecutionEnv`, so the same `read` tool works
against the real filesystem, an in-memory fake, or a remote backend — and the
test suite never touches your disk.

## Install

```toml
[dependencies]
tools = "0.3"
agent = "0.3"
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

use tools::{create_bash_tool, create_write_tool, ExecutionToolContext, OsExecutionEnv};

let env = Arc::new(OsExecutionEnv::with_cwd("/tmp/workspace".into()));
let ctx = ExecutionToolContext::new(
    env.clone() as Arc<dyn tools::ExecutionEnv>,
    Some(env as Arc<dyn tools::MutatingEnv>),
);

let write_tool = create_write_tool(&ctx);
let bash_tool = create_bash_tool(&ctx, None);

let agent = agent::AgentBuilder::new()
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
use tools::{ExecutionToolContext, InMemoryExecutionEnv, MutationQueueRegistry};

let env = Arc::new(InMemoryExecutionEnv::new());
let ctx = ExecutionToolContext::new(
    env.clone() as Arc<dyn tools::ExecutionEnv>,
    Some(env as Arc<dyn tools::MutatingEnv>),
);
let read = tools::create_read_tool(&ctx, None);
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

`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, `powershell`, `output`, `batch`.
`create_*_tool` constructors take the shared `ExecutionToolContext`; the batch
constructor instead takes the enabled read-only tool registry. It cannot run
shell commands, mutations, or nested batches, and validates all children before
starting any. It returns at most four ordered results with a 128 KiB cap each;
oversized results are explicit failures directing the caller to the standalone
tool.

Batch calls use each tool's fields directly, without an `arguments` wrapper:
`{"calls":[{"tool":"read","path":"package.json"},{"tool":"ls","path":"app"}]}`.
The published schema includes the required fields and options of each enabled
child tool, so invalid child arguments are rejected before any read starts.

The CLI and embedded backend expose these tools by default (PowerShell only on
Windows), alongside the docs tool. `default_tools`, `--tools`, and
`--exclude-tools` still restrict the available registry and batch children.

### Shell execution

Bash and native PowerShell use `tokio::process::Command` with stdin, both output
pipes, child status, deadlines and cancellation driven concurrently. The bounded
`ShellExecOptions::output` channel must be consumed concurrently; synchronous
stdout/stderr callbacks remain available. Idle streams do not reserve channel
capacity. Dropped/aborted calls terminate their process groups; normally
completed calls may intentionally leave background descendants alive.

The capture layer maintains a bounded display tail and persists the complete
sanitized stream after display truncation, batching existing backlogs up to
64 KiB without delaying sparse writes. It also retains the first 6 KiB of
output (once, never grown) so a *failed* truncated command can show a bounded
head window alongside the tail — see below. First progress is immediate; Bash and
PowerShell coalesce later checkpoints with one 16 ms timer, including when
output goes quiet. `RPI_BASH_UPDATE_THROTTLE_MS` overrides that window.
`ShellCaptureOptions::update_interval = None` retains per-chunk callbacks for
other capture consumers.

Display truncation is tail-only by default. When a command failed and its
output was truncated, the terminal notice additionally shows a bounded head
window: the first complete lines, an exact elided-line count, then the tail, then
the full-log path. Nothing is filtered, summarised, reordered or inferred — the
exit code comes from the recorded child status, not from the text — and the window
is refused unless it is strictly smaller than the output it replaces, so it can
never inflate a result. Non-zero output that fits within the limits is unchanged,
as is every successful command.

Both Bash and PowerShell accept `output_mode: "compact"`; the default `raw`
preserves the existing output. Compact mode persists the complete sanitized log
even for small commands, retains unknown text and diagnostics, collapses
recognized Cargo/pytest/Go/JS passing or progress records, and groups adjacent
identical diagnostic lines with exact occurrence counts. A compact view is used
only when its marker and recovery path make it strictly smaller. Truncated
captures are explicitly partial: only repeated retained diagnostics are grouped,
never reported as a complete test summary. Failed persistence disables compaction.

Use `output` with the saved `full_output_path` to inspect or search the log
without rerunning a command. Shell results include capture duration (including
spooling), final formatting duration, raw/displayed bytes and progress-update
counts. Runtime tool details add `performance` measurements and shared counters;
these are observed bytes and timings, not token estimates. Counters include
batch children, are transient, and reset when tools are rebuilt or reloaded.

Post-exit drain allows 100 ms plus at most 1 MiB of immediately ready bytes per
pipe, not arbitrary future descendant output. Slow storage can delay final log
delivery, but child termination remains independently driven. A failed spool is
never advertised as a complete log.

Native benchmark:
`cargo run --manifest-path agent/Cargo.toml -p tools --example bench_shell -- 15`
from the Shob root. See the root `SHELL_TOOL_AUDIT.md` for scope and results.

### Saved command output

`output` reads existing saved logs through stable file handles in 64 KiB chunks,
returning at most 2,000 numbered source lines or 50 KiB. Supply `path`, optional
1-based `offset` and `limit`, and optional `pattern` (`literal` and `ignore_case`
are supported). Follow `next_offset` and, when present, `next_byte_offset` as
`offset` and `byte_offset` to recover clipped text without skipping bytes.
Continuation rescans the same log rather than caching its contents.

Search uses memory-bounded streaming regular expressions. Unsupported assertions
(such as Unicode word boundaries) or expressions exceeding the DFA memory budget
return explicit validation errors; Unicode text and literals remain supported.
Backends without stable ranged reads fail explicitly rather than eagerly loading
the file. Logs are temporary files and may disappear during OS temporary-file
cleanup.

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

`grep` accepts `mode: "content"` (default), `"files"` (one matching filename), or
`"count"` (matching-line counts per file). `offset` is a zero-based logical
result offset; reuse `next_offset` with the same arguments and unchanged inputs.
Pages rescan deterministically without caching filesystem state. Counts are
exact only for fully searched files; partial counts and incomplete searches are
labeled. Each page is capped at 10,000 results, 1,000 context lines per match,
and the existing display byte limit. `files` stops at the first match per file.

## Related crates

| Crate | Role |
| ----- | ---- |
| [`agent`](https://crates.io/crates/rpi-agent) | The `AgentTool` trait these tools implement |
| [`ai`](https://crates.io/crates/rpi-ai) | Message and tool-schema types |
| [`harness`](https://crates.io/crates/rpi-harness) | Sessions and the run loop that drives them |
| [`cli`](https://crates.io/crates/rpi-cli) | The terminal agent |

## Documentation

- Crate docs: <https://docs.rs/rpi-tools>
- Architecture: [docs/architecture/overview.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture/overview.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
