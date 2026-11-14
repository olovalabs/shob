# rpi-harness

[![crates.io](https://img.shields.io/crates/v/rpi-harness.svg)](https://crates.io/crates/rpi-harness)
[![docs.rs](https://docs.rs/rpi-harness/badge.svg)](https://docs.rs/rpi-harness)

Durable sessions and the run loop that drives [`rpi-agent`](https://crates.io/crates/rpi-agent).
Where the agent crate gives you a loop over one prompt, the harness gives you
a session tree that survives process death.

## Install

```toml
[dependencies]
rpi-harness = "0.3"
rpi-agent = "0.3"
rpi-ai = "0.3"
```

## What it adds on top of the agent loop

- **Session tree.** Entries are appended to a persistent store and can be
  branched, navigated and labelled — the session is a tree, not a linear log.
- **JSONL persistence.** A durable backend that writes every entry to disk, plus
  an in-memory backend for tests.
- **Crash recovery.** Frame-level progress is recorded as it streams, so a
  process that dies mid-run resumes from the last committed frame instead of
  losing the work. Queued steering and follow-up messages survive the restart
  too.
- **Compaction.** When the context grows past a threshold, older turns are
  summarised rather than dropped, under a split-turn invariant that keeps the
  two-LLM-call structure valid.
- **Deferred responses.** Providers with a long-poll API park an operation and
  resume it later without re-running the tool calls it already executed.
- **Resources.** Skills, prompt templates and system-prompt assembly are loaded
  and versioned with the session.

## Quick start

```rust
use std::sync::Arc;

use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;
use rpi_harness::agent_harness::{AgentHarness, AgentLane};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::{DefaultIdGenerator, Session};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool, RetryPolicy};
use rpi_tools::{create_read_tool, ExecutionToolContext, InMemoryExecutionEnv};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = Arc::new(FauxProvider::new(
        FauxScript::new().with_text("Done."),
    ));
    let model = provider.default_model().clone();

    let env = Arc::new(InMemoryExecutionEnv::new());
    let ctx = ExecutionToolContext::new(
        env.clone() as Arc<dyn rpi_tools::ExecutionEnv>,
        Some(env as Arc<dyn rpi_tools::MutatingEnv>),
    );
    let read = create_read_tool(&ctx, None);
    let tools = vec![HarnessTool::new(read)];
    let active_tool_names = tools.iter().map(|t| t.tool.schema().name.clone()).collect();

    let session = Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata { id: "demo".into(), created_at: 0, parent_session_id: None },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    );

    let harness = AgentHarness::create(AgentHarnessOptions {
        model,
        active_tool_names,
        tools,
        models: vec![provider as Arc<dyn Provider>],
        session,
        retry: RetryPolicy::default(),
        thinking_level: Default::default(),
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session: false,
    }).await?;

    let result = harness.prompt_text("Say hi.", Vec::new()).await?;
    println!("{:?}", result.outcome);
    Ok(())
}
```

Swap `InMemorySessionStorage` for the JSONL backend to get a durable session, and
set `allow_existing_session: true` to resume one that already has records.

> The options struct is deliberately explicit rather than `..Default::default()`:
> `model`, `session` and `models` have no sensible default, and a silent default
> for the rest would hide the two-LLM-call and retry configuration from a reader.
> A builder is on the roadmap.

## Observing a run

`harness.events()` returns the harness event bus, and `harness.lane("main")`
gives the run lane. `harness.watch_session(|change| …)` reports session-tree
changes. Cancellation goes through `harness.request_abort(run_id)`, and a
deferred operation is resumed with `harness.resume(suspended_id)`.

## Recovery semantics

A run that was interrupted is not silently restarted. Recovery repairs the
session to a valid state, and `harness.has_pending_resume()` tells the caller
whether there is unfinished work to continue — the caller decides *when*,
because driving a run has to happen wherever the output is rendered.

An assistant message whose tool calls have no results is invalid for every
provider, so recovery either replays the call (only when both the recorded
policy and the current tool declaration say the call is safe) or records the
outcome as unknown. Retrying is bounded by a resume budget.

The end-to-end expectations are pinned in
[`crates/rpi-harness/tests/harness_run_e2e.rs`](https://github.com/bigfish1913/pi-rust/blob/main/crates/rpi-harness/tests/harness_run_e2e.rs),
including one test per crash point.

## Related crates

| Crate | Role |
| ----- | ---- |
| [`rpi-agent`](https://crates.io/crates/rpi-agent) | The loop this crate drives |
| [`rpi-tools`](https://crates.io/crates/rpi-tools) | Built-in tools and execution environments |
| [`rpi-cli`](https://crates.io/crates/rpi-cli) | The terminal agent built on the harness |

## Documentation

- Crate docs: <https://docs.rs/rpi-harness>
- Design notes: [docs/architecture/overview.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture/overview.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).

## Native child journal and archival-body API

`tool_program_body::{append_body, read_body_page}` stores bounded canonical JSON
chunks and a descriptor-last commit in native primary custom entries. The harness
now installs a native child journal using it, composed with an optional host sink;
child RPC capabilities/events and adapter/UI projection are still pending. See the
[host contract](../../docs/extensions/tool-program.md#latest-installed-native-child-journal-queries-and-ui-still-pending)
for limits, ownership, consumer integrity checks and qualification boundaries.

`EntryQuery` adds `exclude_tool_program_bodies: bool` (default false). Downstream
exhaustive struct literals must add this field or use `..Default::default()`.
Custom storage implementations must apply it before reading payloads. Exact
reserved body types are not transform/projector input; ordinary custom entries
remain visible. Explicit generic history/export reads still include archives.
`SessionStorage`/`SessionTree` add default `branch_ancestry()` methods returning
`EntryAncestry` identities; existing implementers compile, but their fallback can
hydrate full entries until a metadata-only override is provided. Native metadata
walks are O(history), ordinary SQLite/memory remain eager, and no RSS guarantee is
made. Default journaling additionally requires real `history_revision()` and
atomic conditional-entry appends from custom storage implementations/wrappers.
Ordinary native facade mutations share per-binding admission; journal CAS/body
transactions take exclusive admission without retry. Exact native child begin/phase
custom types are reserved on generic and guarded public append APIs. Closed
private mappings retire, and normal close joins the writer; abrupt Drop is abort-
only. Bounded child queries/recovery projection, native events and Web/ACP remain
pending.
