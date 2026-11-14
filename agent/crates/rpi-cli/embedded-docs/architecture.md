# pi-rust architecture

A Rust coding-agent stack, layered so each crate stays small and testable. The workspace is shaped so a CLI crate can sit on top of the library crates without pulling the libraries toward CLI concerns.

## 1. What the TypeScript Pi SDK actually is

Read from `.reference/pi/packages/`. Five layers, each an npm package:

| TS package          | Responsibility                                                  | Key files (under `packages/`) |
|---------------------|-----------------------------------------------------------------|-------------------------------|
| `pi-ai`             | Unified multi-provider LLM API. Types, `Model`, `Provider`, `Models`, `AssistantMessageEventStream`, provider adapters, auth. | `ai/src/types.ts`, `ai/src/models.ts`, `ai/src/utils/event-stream.ts`, `ai/src/providers/*.ts` |
| `pi-agent-core` (`packages/agent`) | The agent runtime. `Agent`, `agent-loop`, `AgentTool`, events, hooks, steering/follow-up queues. | `agent/src/agent.ts`, `agent-loop.ts`, `types.ts` |
| `pi-agent-core` harness | Stateful session tree, compaction, tools, JSONL persistence. | `agent/src/harness/agent-harness.ts`, `session/`, `compaction/`, `tools/` |
| `pi-telemetry`      | Span/event contracts, noop impl.                                | `telemetry/` |
| `pi-tui`            | Terminal UI. **Out of scope for the SDK port.**                | `tui/` |

### The core insight: the loop is provider-agnostic

`agent-loop.ts` runs the conversation loop:

```
prompt → transformContext(AgentMessage[]) → convertToLlm(AgentMessage[]) → Message[]
       → streamFn(model, Context{systemPrompt, messages, tools}, options) → AssistantMessageEventStream
       → stream events → if toolUse → executeToolCalls → push ToolResultMessage → loop
```

The **only** LLM boundary is `StreamFn`:

```ts
type StreamFn = (model, context, options?) => AssistantMessageEventStream | Promise<...>
```

Everything else — `Agent`, tools, events, hooks, queues — talks in `AgentMessage` and never touches provider wire formats. So a minimal port is: types + `EventStream` + one `Provider` + the loop + `Agent`. The harness and persistence sit *above* the loop, not inside it.

### Message model

From `ai/src/types.ts`:

- `UserMessage { role:"user", content: string | (Text|Image)[], timestamp }`
- `AssistantMessage { role:"assistant", content: (Text|Thinking|ToolCall)[], api, provider, model, usage, stopReason, ... }`
- `ToolResultMessage { role:"toolResult", toolCallId, toolName, content: (Text|Image)[], isError, ... }`
- `Message = User | Assistant | ToolResult`

`AgentMessage = Message | Custom` — apps add their own message kinds (UI notifications, artifacts) and `convertToLlm` strips them before the LLM call. The TS version does this via declaration merging; Rust uses an open enum (see §3).

### Streaming protocol

`AssistantMessageEvent` (`ai/src/types.ts:523`) is a tagged union:

```
start | text_{start,delta,end} | thinking_{start,delta,end}
  | toolcall_{start,delta,end} | done | error
```

`AssistantMessageEventStream` (`ai/src/utils/event-stream.ts`) is a single-producer/multi-consumer async queue with a `.result()` future holding the final `AssistantMessage`. The loop folds deltas into a growing `partial` `AssistantMessage` and emits `message_update` to the app.

### Tool execution

From `agent-loop.ts:411-797` and `types.ts:386`:

- `AgentTool { name, description, parameters: TSchema, label, execute(toolCallId, params, signal, onUpdate) -> AgentToolResult }`
- Execution mode per-batch: `sequential` or `parallel` (preflight sequentially, then run concurrently).
- Hooks: `beforeToolCall` (block/terminate), `afterToolCall` (override content/details/isError/usage/terminate).
- Validation via `validateToolArguments` (`ai/src/utils/validation.ts`) — TypeBox schema compile + JSON-coerce. Rust uses `serde_json` + `jsonschema`.
- Truncation safety: if `stopReason === "length"`, all tool calls in that message are failed, not executed (`agent-loop.ts:381`).

### Queues / steering

`Agent` owns two `PendingMessageQueue`s (`agent.ts:125`):
- **steering** — injected mid-run after the current turn; default `one-at-a-time`.
- **follow-up** — injected after the agent would otherwise stop; default `one-at-a-time`.

### Harness (phase 2, what it adds)

`agent-harness.ts` + `session/` + `compaction/`:
- **Session tree**: entries (write-once) forming a conversation DAG with branches/forks; `Lane`s as cursors.
- **Persistence**: JSONL session files (`session/jsonl/`) + optional SQLite backend (`session-backends/sqlite-node/`).
- **Compaction**: when context nears the window, summarize old turns into a `CompactionEntry`.
- **Skills/prompt-templates**: injected into the system prompt.
- **Per-turn snapshot**: `AgentHarnessStreamOptions`, `AgentHarnessToolContextSource`.

The harness is a *stateful wrapper* over the stateless `Agent` loop — exactly the layering we'll mirror.

## 2. Workspace design (crates.io-style workspace)

```
pi-rust/
  Cargo.toml              (workspace root)
  crates/
    pi-telemetry/         → pi-ai
    pi-ai/                → pi-agent
    pi-agent/             → pi-tools  (optional dep; tools are pluggable)
    pi-tools/             → pi-harness
    pi-harness/           → pi-cli
    pi-cli/               (future binary; reserved)
  examples/
    minimal/  tools/  persistent/
  docs/
  .reference/pi/          (TS source, read-only)
```

**Dependency direction is strictly one-way** (left arrows = "depends on"):

```
pi-telemetry ◀ pi-ai ◀ pi-agent ◀ pi-tools ◀ pi-harness ◀ pi-cli
```

### Why these crate boundaries

- **`pi-ai`** is reusable on its own (LLM client + types). A user who just wants "call Anthropic with tool-use, no loop" depends only on `pi-ai`.
- **`pi-agent`** depends only on `pi-ai`. It owns the loop, `Agent`, `AgentTool`, events, hooks, queues. No filesystem, no shell — those live in tools.
- **`pi-tools`** owns the `ExecutionEnv` trait (file/shell abstraction, mirrors `harness/types.ts`) and built-in tools `read`/`write`/`edit`/`bash`. Apps that bring their own tools skip this crate entirely.
- **`pi-harness`** owns session tree/compaction/JSONL. Heavy and optional; only for agents that need durable branching.
- **`pi-cli`** is the future command-line coding agent. Having it as a separate crate means the library crates never link argparse/TUI/prompt handling.

### Async runtime

Tokio. The loop, streaming, and tools are all `async`. `pi-harness` JSONL writes go through a blocking thread pool (`tokio::task::spawn_blocking`) since `std::fs` is the persistence layer for phase 2.

## 3. Core types (`pi-ai`)

Mirror `ai/src/types.ts`.

```rust
// crates/rpi-ai/src/types.rs
pub struct Api(pub Cow<'static, str>);           // "anthropic-messages" | "openai-responses" | ...
pub struct ProviderId(pub Cow<'static, str>);

pub enum Content {
    Text { text: String, text_signature: Option<String> },
    Thinking { thinking: String, signature: Option<String>, redacted: bool },
    Image { data: String, mime_type: String },     // base64
    ToolCall { id: String, name: String, arguments: serde_json::Value, namespace: Option<String> },
}

pub enum StopReason { Pending, Stop, Length, ToolUse, Error, Aborted, Deferred }

pub struct Usage { input, output, cache_read, cache_write, reasoning, total_tokens: u64, cost: Cost }

pub enum Message {
    User { content: UserContent, timestamp: i64 },
    Assistant(Box<AssistantMessage>),   // Box: it's the heavy variant
    ToolResult(Box<ToolResultMessage>),
}

pub struct AssistantMessage {
    content: Vec<Content>,
    api: Api, provider: ProviderId, model: String, response_model: Option<String>,
    usage: Usage, stop_reason: StopReason, error_message: Option<String>,
    end_turn: Option<bool>, timestamp: i64,
}

pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
}

pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Schema,                // serde_json::Value (JSON Schema)
    pub constrained_sampling: Option<ConstrainedSamplingConfig>,
}

pub struct Model<Api marker> {
    id, name, base_url, provider, reasoning, input: Vec<InputModality>,
    cost, context_window, max_tokens, thinking_level_map, sampling_params, compat, ...
}
```

### Schema representation

TS uses TypeBox (`TSchema`) — a JSON-Schema builder with a compiled validator. Rust equivalent:
- **Type**: `serde_json::Value` (a raw JSON Schema object). No newtype builder in v0.1; we ship a small `schema!` macro for ergonomics and re-export `schemars` (optional) for derive-based schemas.
- **Validation + coercion**: `jsonschema` crate for validation; custom coercion pass mirroring `validation.ts:coercePrimitiveByType` (string→number/integer/bool, null→default) so lax model outputs still validate.

### Event stream

```rust
// crates/rpi-ai/src/event_stream.rs — mirrors ai/src/utils/event-stream.ts
pub enum AssistantMessageEvent {
    Start { partial: AssistantMessage },
    TextStart { content_index: usize, partial: AssistantMessage },
    TextDelta { content_index: usize, delta: String, partial: AssistantMessage },
    TextEnd { content_index: usize, content: String, partial: AssistantMessage },
    ThinkingStart/ThinkingDelta/ThinkingEnd { ... },
    ToolCallStart/ToolCallDelta/ToolCallEnd { ... },
    Done { reason: DoneReason, message: AssistantMessage },
    Error { reason: AbortReason, error: AssistantMessage },
}

pub struct AssistantMessageEventStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<AssistantMessageEvent>,
    result: tokio::sync::oneshot::Receiver<AssistantMessage>,
}
impl AssistantMessageEventStream {
    pub async fn next(&mut self) -> Option<AssistantMessageEvent>;
    pub async fn result(self) -> AssistantMessage;   // resolves on Done/Error
}
```

A `Provider`'s `stream_simple()` returns `(Sender, Stream)`; the provider task pushes events and finalizes the oneshot on `Done`/`Error`.

### `Provider` and `Models`

```rust
// mirrors models.ts Provider<Api> + Models
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn models(&self) -> &[Model];
    async fn stream_simple(&self, model: &Model, ctx: &Context, opts: &SimpleStreamOptions)
        -> AssistantMessageEventStream;
    // stream(), complete(), complete_simple() default-implemented on top of stream_simple
}

pub struct Models { providers: Vec<Box<dyn Provider>>, /* auth, refresh */ }
impl Models {
    pub fn stream_simple(&self, model, ctx, opts) -> AssistantMessageEventStream;
    pub async fn complete_simple(...) -> AssistantMessage;
}
```

Auth: phase 1 only takes `api_key: Option<String>` per call (`SimpleStreamOptions.api_key` mirrors `ProviderRequestOptions.apiKey`). OAuth flows (`auth/oauth/*.ts`) and credential stores are **deferred** — they're CLI concerns.

### Providers to ship

User said "保持一致" (keep consistent with the TS source) → port the provider list. Realistically tier them:

- **Tier 0 (v0.1, blocks everything):** `faux` (`providers/faux.ts` — a deterministic scripted provider used by every test, no network). The whole test suite runs against faux.
- **Tier 1 (v0.1):** `anthropic` (`api/anthropic-messages.ts`). Primary real provider. Covers thinking, tool-use, prompt-cache `cache_control`, image input.
- **Tier 2 (v0.2):** `openai-responses` + `openai-completions` (covers OpenAI, Azure, DeepSeek, Groq, Together, Fireworks, OpenRouter since they're all OpenAI-compatible — one adapter + `OpenAICompletionsCompat` flags).
- **Tier 3 (later):** google, bedrock, mistral.

Rust providers are each their own submodule under `crates/rpi-ai/src/providers/`. HTTP via `reqwest` (streaming SSE via `reqwest::Response::bytes_stream()`). Each provider **does not** use the official SDK — the TS source calls raw REST too (`anthropic-messages.ts` uses `@anthropic-ai/sdk` but only as an HTTP/payload helper; we go straight to reqwest to keep it dependency-light and uniform across providers).

## 4. Agent runtime (`pi-agent`)

Mirrors `packages/agent/src/agent.ts` + `agent-loop.ts` + `types.ts`.

### `AgentMessage` open enum

TS uses declaration merging for custom messages. Rust:

```rust
pub enum AgentMessage {
    Llm(Message),                      // User | Assistant | ToolResult
    Custom(Arc<dyn Any + Send + Sync>),
}
```

`Agent::builder().convert_to_llm(my_fn)` sets the closure that maps `Vec<AgentMessage> → Vec<Message>`, filtering `Custom` out. Default: keep only `Llm`. `Custom` is `Arc<dyn Any>` so app messages are cheap to clone (the loop clones the transcript snapshot per turn).

### `AgentTool` trait

```rust
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn schema(&self) -> &Tool;                       // name, description, parameters JSON-Schema
    fn label(&self) -> &str { self.schema().name.as_str() }
    fn execution_mode(&self) -> ToolExecutionMode { ToolExecutionMode::Parallel }
    fn prepare_arguments(&self, args: Value) -> Result<Value> { Ok(args) }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,                               // validated+coerced against schema()
        signal: CancellationToken,
        on_update: &dyn Fn(ToolResult),              // partial updates → ToolExecutionUpdate event
    ) -> Result<ToolResult>;
}

pub struct ToolResult {
    pub content: Vec<Content>,     // Text | Image
    pub details: Value,            // arbitrary, for UI/logs (not sent to LLM)
    pub usage: Option<Usage>,
    pub added_tool_names: Vec<String>,
    pub terminate: bool,           // hint: stop after this batch if all results set it
}
```

A blanket impl lets you implement a tool from a plain `async fn` + a `Tool` schema:

```rust
#[pi_agent::tool]   // proc-macro (phase 1.5) — derives schema from a serde struct
struct ReadInput { path: String, offset: Option<u32>, limit: Option<u32> }
async fn read(_id: &str, args: ReadInput, _sig, _u) -> Result<ToolResult> { ... }
```

Until the proc-macro lands, tools are built struct-style with a `Tool::for_fn(name, json_schema, fn)` helper.

### `Agent` and the loop

`Agent` owns:
- state: `system_prompt`, `model`, `thinking_level`, `tools: Vec<Arc<dyn AgentTool>>`, `messages: Vec<AgentMessage>`, `is_streaming`, `streaming_message`, `pending_tool_calls`, `error_message`
- two queues: `steering`, `follow_up` (each `QueueMode::All` or `OneAtATime`)
- an `AbortHandle` for the active run
- subscribers: `tokio::sync::broadcast::Sender<AgentEvent>`

Public API mirrors `agent.ts`:

```rust
impl Agent {
    pub fn builder(model: Model) -> AgentBuilder;
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent>;
    pub async fn prompt(&mut self, input: PromptInput) -> Result<()>;   // string | Message | Vec<Message>
    pub async fn continue_run(&mut self) -> Result<()>;
    pub fn steer(&self, m: AgentMessage); pub fn follow_up(&self, m: AgentMessage);
    pub fn abort(&self); pub async fn wait_for_idle(&self);
    pub fn reset(&mut self);
    pub fn state(&self) -> &AgentState;
}
```

The loop (`agent-loop.rs`, porting `runLoop`, `streamAssistantResponse`, `executeToolCalls*`, `prepareToolCall`, `finalizeExecutedToolCall`) is a free `async fn run_agent_loop(...)` so it's testable without `Agent`. `Agent::prompt` is a thin owner+subscriber wrapper around it, exactly like the TS split.

### Events

```rust
pub enum AgentEvent {
    AgentStart,
    AgentEnd { messages: Arc<Vec<AgentMessage>> },
    TurnStart, TurnEnd { message: AgentMessage, tool_results: Arc<Vec<ToolResultMessage>> },
    MessageStart { message: AgentMessage },
    MessageUpdate { message: AgentMessage, assistant_event: AssistantMessageEvent },
    MessageEnd { message: AgentMessage },
    ToolExecutionStart { tool_call_id, tool_name, args },
    ToolExecutionUpdate { tool_call_id, tool_name, args, partial_result },
    ToolExecutionEnd { tool_call_id, tool_name, result, is_error },
}
```

`broadcast` because multiple subscribers (UI, logger, telemetry) read the same events; `Arc<Vec<...>>` on the payload variants so cloning is cheap.

### Hooks

All optional, all on `AgentBuilder`:

```rust
.before_tool_call(|ctx, signal| async move { Ok(Some(BeforeToolCallResult{block:true,..})) })
.after_tool_call(|ctx, signal| async move { Ok(Some(AfterToolCallResult{content:Some(..),..})) })
.should_stop_after_turn(|ctx| async move { Ok(false) })
.prepare_next_turn(|ctx| async move { Ok(None) })
.transform_context(|messages, signal| async move { Ok(messages) })
.convert_to_llm(default | my_fn)
.get_api_key(|provider| async move { Ok(env::var(..)) })
```

Closures are `Arc<dyn Fn(...) -> BoxFuture<...> + Send + Sync>`. This is the trickiest part of the port — TS just uses function values; Rust needs boxed futures. We'll define shared `BoxFuture`-returning `Arc` type aliases per hook to keep builder syntax clean.

**Two `transform_context` layers, not one.** pi has both a low-level and a
harness-level hook of this name, and they are not interchangeable:

| | pi | rpi |
|---|---|---|
| `agent-loop.ts` | `(messages) => messages` | `TransformContext` on `AgentLoopConfig` — **same** |
| `harness` hook map | `{messages, systemPrompt} => {messages?, systemPrompt?}` | the prompt half lives on `ProviderHooks::transform_context` returning `ContextPatch`; the messages half is **not** implemented |

A plugin reaches the prompt half through `register_before_agent_start`
(`before_agent_start` returning `systemPrompt`, i.e. pi's `forceSystemPrompt`).
So `TransformContext` above staying `messages -> messages` is correct, not an
abbreviated port: it mirrors the loop, and the prompt lives one layer up where a
provider request is assembled.

### Cancelation / abort

TS uses `AbortSignal`. Rust uses `tokio_util::sync::CancellationToken` (graceful cooperative cancel, child tokens for nested work). `Agent::abort()` cancels the run's token; tools receive a child token and must check/await it. Partial-update callbacks are no-ops after the tool future resolves (`acceptingUpdates` flag → a `bool` behind the closure).

## 5. Built-in tools + execution env (`pi-tools`)

Mirrors `harness/tools/*.ts` + `harness/env/nodejs.ts` + `harness/types.ts::ExecutionEnv`.

### `ExecutionEnv` trait

```rust
#[async_trait]
pub trait ExecutionEnv: Send + Sync {
    async fn read_text_file(&self, path: &Path, signal: &CancellationToken) -> Result<String, FileError>;
    async fn read_binary_file(&self, path: &Path, signal: &CancellationToken) -> Result<Vec<u8>, FileError>;
    async fn write_file(&self, path: &Path, data: &[u8], signal: &CancellationToken) -> Result<(), FileError>;
    async fn edit_file(&self, path: &Path, old: &str, new: &str, signal) -> Result<EditOutcome, FileError>;
    async fn list_dir(&self, path: &Path, signal) -> Result<Vec<FileInfo>, FileError>;
    async fn stat(&self, path: &Path, signal) -> Result<FileInfo, FileError>;
    async fn mkdir(&self, path: &Path, signal) -> Result<(), FileError>;
    async fn remove(&self, path: &Path, recursive: bool, signal) -> Result<(), FileError>;
    async fn shell(&self, cmd: ShellExecOptions, signal) -> Result<ShellOutput, ExecutionError>;
    fn cwd(&self) -> &Path;
}
```

This is the seam between the tools and the host. Default impl: `NodejsEnv` (real `tokio::fs` + `tokio::process::Command`), mirroring `env/nodejs.ts`. Tests use an `InMemoryEnv` (mirrors the conformance test scaffolding under `harness/session/testing/`).

### Tools

- `read` — `read.ts`: text + image, line/byte truncation (`utils/truncate.ts`), offset/limit, `ReadImageProcessor` plug.
- `write` — `write.ts`: create/overwrite.
- `edit` — `edit.ts` + `edit-diff.ts`: old→new string replace, `FileMutationQueue` serializes edits to the same file.
- `bash` — `bash.ts`: shell exec with timeout, stdout/stderr capture, returncode.

Each is `pub fn create_read_tool(opts) -> Arc<dyn AgentTool>` etc. — same factory shape as the TS exports.

## 6. Harness + persistence (`pi-harness`) — phase 2

Mirror `harness/agent-harness.ts` + `session/` + `compaction/`. Spec is in `.reference/pi/packages/agent/docs/harness.md` (2941 lines — the canonical impl contract; we follow it directly).

### Scope

- `SessionTree`: write-once entries, branch/fork, `Lane` cursors, facts, usage ledger.
- `SessionStorage` trait with two backends:
  - `JsonlStorage` — `session/jsonl/` (codec + repo + storage). v0.1 default.
  - `SqliteStorage` — `session-backends/sqlite-node/` (phase 3).
- `AgentHarness`: wraps `Agent`, drives runs/compaction/navigation through the session tree, emits `RunOutcome`/`CompactionOutcome`/`NavigationOutcome`.
- `compaction` (`compaction/compaction.ts`): `should_compact`, `find_cut_point`, `generate_summary`, `prepare_compaction`, `compact`.
- `skills` + `prompt-templates` system-prompt injection.

### Why it's a separate crate

The harness is genuinely complex (the 2941-line spec exists precisely because the state machine is subtle). Keeping it out of `pi-agent` means a user building a simple agent links only ~3 crates and gets the stateless loop. Only agents that need durable sessions, branching, or compaction pay for it.

## 7. Telemetry (`pi-telemetry`)

Minimal: `TelemetryContext` trait with a `Noop` impl (mirrors `NOOP_TELEMETRY_CONTEXT`) and an `InMemory` impl for tests. `start_span`/`start_event` return RAII guards. The TS telemetry schemas (`AiSpan`, `HarnessSpan`) become typed newtypes in `pi-agent`/`pi-harness` that call into the context. Full OTLP export is out of scope.

## 8. `pi-cli` (future, reserved)

Empty crate with a `Cargo.toml` and a stub `main.rs` so the workspace compiles. When built, it will depend on `pi-harness` + `pi-tools` and provide the interactive coding agent. Library crates never depend on it.

## 9. Build order

**Phase 1 — minimal end-to-end (CRATE 1-3):**
1. `pi-telemetry` noop.
2. `pi-ai`: types, `Schema`, `validate_tool_arguments`+coerce, `AssistantMessageEventStream`, `faux` provider. No real network yet.
3. `pi-agent`: `AgentTool` trait, `Agent`, loop, events, hooks, queues. All tests against faux.

→ Deliverable: an agent that runs against a scripted provider, executes custom tools, streams events. This alone is "conveniently build your own agent."

**Phase 2 — real provider (CRATE 2):**
4. `pi-ai/providers/anthropic`: SSE streaming → `AssistantMessageEvent`, thinking/tool-use, `cache_control`, image input. Reqwest + `tokio` SSE.

→ Deliverable: real Claude agent with tools.

**Phase 3 — built-in tools (CRATE 4):**
5. `pi-tools`: `ExecutionEnv` + `NodejsEnv`, `read`/`write`/`edit`/`bash`. `InMemoryEnv` for tests.

→ Deliverable: coding-agent-capable library.

**Phase 4 — harness + persistence (CRATE 5):**
6. `pi-harness`: session tree, JSONL, compaction, `AgentHarness`. Follow `harness.md` spec directly.

**Phase 5 — more providers:** openai family, google, bedrock. Each is additive behind the `Provider` trait.

**Phase 6 — `pi-cli`:** the interactive binary.

## 10. Conventions

- Every Rust module's first doc comment names the TS file it mirrors: `//! Mirrors `packages/ai/src/types.ts`.`
- Public API names keep the TS spelling where idiomatic (`prompt`, `continue_run`, `steer`, `follow_up`, `agent_loop`, `stream_simple`). `snake_case` for Rust-style exported fns per rustfmt.
- No `unwrap`/`expect` in library code outside tests; `Result<T, PiError>` with a crate error enum per crate.
- Async everywhere; no blocking calls in library code paths.
- All timestamps `i64` ms-since-epoch (matches TS `timestamp: number`).
- Owned `String`/`Vec` in public types; `&str`/`&[T]` on inputs.
- License: MIT (Pi is MIT).
