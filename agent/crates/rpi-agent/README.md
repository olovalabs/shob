# rpi-agent

[![crates.io](https://img.shields.io/crates/v/rpi-agent.svg)](https://crates.io/crates/rpi-agent)
[![docs.rs](https://docs.rs/rpi-agent/badge.svg)](https://docs.rs/rpi-agent)

The provider-agnostic agent runtime: the agent loop, the `AgentTool` trait,
events, hooks, queues and cancellation. This is the layer you embed when you
want to build your own agent rather than use the `rpi` CLI.

## Install

```toml
[dependencies]
rpi-agent = "0.3"
rpi-ai = "0.3"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

For built-in file and shell tools, add
[`rpi-tools`](https://crates.io/crates/rpi-tools). For durable sessions and
compaction, add [`rpi-harness`](https://crates.io/crates/rpi-harness).

## Design

The only LLM boundary is `StreamFn`, which returns an
`AssistantMessageEventStream` synchronously. Everything else in the loop speaks
`AgentMessage` and never touches a provider wire format, so a provider, a test
double and a recorded transcript are interchangeable.

The invariants that the loop guarantees:

- Tools may finish in any order, but tool results are emitted in the order the
  calls appeared, so the transcript stays valid for the next request.
- A `MessageEnd` is emitted exactly once per assistant message, including when a
  late streaming update arrives after the run already settled.
- Truncated or failed tool calls never leave a dangling call without a result.

## Optional native message identity

Public `AgentEvent::{MessageStart, MessageUpdate, MessageEnd}` now carry
`message_id: Option<String>`. The direct agent loop emits `None`; a native
session/harness can attach its reserved entry ID before publishing the event
and commit that same ID to history. Provider response IDs and tool-call IDs are
not substitutes. Tool-execution and unrelated lifecycle events do not acquire
this field. Consumers constructing these Rust enum variants must supply
`message_id` (use `None` without a session owner); pattern matches that do not
need identity can use `..`. Optional wire fields do not make adding a Rust enum
field source-compatible with exhaustive old constructors/patterns.

`AgentEmitter::emit_injected(event)` marks an actual queued message delivery
separately from ordinary generation, so the session owner can consume the
correct durable reservation even for identical-content messages. Its default
implementation calls `emit(event)`, preserving existing custom emitter behavior
without requiring an override. It does not generate an ID or change provider
message content. See the [RPC identity contract](../../docs/message-image-file-io-rpc.md#message-identity)
for native advertisement and adapter scope.

## Quick start

Deterministic, offline, no API key — the `faux` provider scripts one text reply:

```rust
use std::sync::Arc;

use rpi_agent::AgentBuilder;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;

#[tokio::main]
async fn main() {
    let provider = Arc::new(FauxProvider::new(
        FauxScript::new().with_text("Hello from the faux provider!"),
    ));
    let model = provider.default_model().clone();

    let agent = AgentBuilder::new()
        .model(model)
        .system_prompt("You are a helpful assistant.")
        .stream_fn(make_stream_fn(provider))
        .build()
        .expect("agent builds");

    let mut rx = agent.subscribe();
    agent.prompt("hi").await.expect("prompt completes");

    while let Ok(ev) = rx.try_recv() {
        // AgentEvent::MessageUpdate { .. }, AgentEvent::ToolEnd { .. }, …
        let _ = ev;
    }

    println!("{} messages after the run", agent.state().messages.len());
}

fn make_stream_fn(provider: Arc<FauxProvider>) -> rpi_agent::StreamFn {
    rpi_agent::stream_fn(move |model, ctx, opts| {
        let p = Arc::clone(&provider);
        let (model, ctx, opts) = (model.clone(), ctx.clone(), opts.clone());
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { p.stream_simple(&model, &ctx, &opts).await })
        })
    })
}
```

A runnable version of this lives in
[`examples/minimal`](https://github.com/bigfish1913/pi-rust/tree/main/examples/minimal)
(`cargo run -p minimal`), and a version that drives the real filesystem through
the built-in tools lives in
[`examples/tools`](https://github.com/bigfish1913/pi-rust/tree/main/examples/tools).

## Defining a tool

Tools implement `AgentTool`. A tool owns the provider-facing schema, a display
label, and the execution function; parameters are validated against that schema
before `execute` is called:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::types::Tool;
use tokio_util::sync::CancellationToken;

struct MyTool {
    schema: Tool,
}

#[async_trait]
impl AgentTool for MyTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "My tool"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        // Long-running tools call `on_update(partial)` to stream progress.
        let _ = on_update;
        Ok(AgentToolResult::default())
    }
}
```

`execute` receives a `CancellationToken` for cancellation and an `on_update`
callback for partial results. Calls to `on_update` after the run has settled are
dropped by the loop, so a tool never has to track settlement itself. Returning
`Err(AgentError)` turns into an error tool result, not a failed run.

For real implementations, wrap the JSON Schema derivation with `schemars` and
start from [`rpi-tools`](https://crates.io/crates/rpi-tools), which contains
complete examples of a filesystem tool and a streaming shell tool.

## Observing a run

`agent.subscribe()` returns a broadcast receiver of `AgentEvent` covering the
run (`AgentStart`/`AgentEnd`), each assistant message
(`MessageStart`/`MessageUpdate`/`MessageEnd`), and each tool
(`ToolStart`/`ToolUpdate`/`ToolEnd`). Cancellation goes through `AbortHandle`.

## Related crates

| Crate | Role |
| ----- | ---- |
| [`rpi-ai`](https://crates.io/crates/rpi-ai) | Provider-agnostic message and streaming types |
| [`rpi-tools`](https://crates.io/crates/rpi-tools) | `read`/`write`/`edit`/`bash` and friends |
| [`rpi-harness`](https://crates.io/crates/rpi-harness) | Sessions, JSONL persistence, compaction |
| [`rpi-cli`](https://crates.io/crates/rpi-cli) | The `rpi` terminal agent built on all of the above |

## Documentation

- Crate docs: <https://docs.rs/rpi-agent>
- Architecture: [docs/architecture/overview.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture/overview.md)
- Project structure guide: [docs/agent/project-structure.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/agent/project-structure.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
