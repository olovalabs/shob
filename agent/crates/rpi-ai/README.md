# rpi-ai

[![crates.io](https://img.shields.io/crates/v/rpi-ai.svg)](https://crates.io/crates/rpi-ai)
[![docs.rs](https://docs.rs/rpi-ai/badge.svg)](https://docs.rs/rpi-ai)

The unified, provider-agnostic LLM layer: message and content types, the
`Provider` trait, streaming events, and concrete provider implementations.

Every other crate in the family talks in these types. Nothing above this layer
knows whether it is talking to Anthropic, an OpenAI-compatible gateway, or the
deterministic in-process `faux` provider.

## Install

```toml
[dependencies]
# Types + the faux provider only. No HTTP stack, no network at build time.
rpi-ai = "0.3"
```

```toml
[dependencies]
# Adds the real HTTP providers (Anthropic Messages, OpenAI-compatible Chat
# Completions / Responses, and OpenAI-compatible gateways).
rpi-ai = { version = "0.3", features = ["providers"] }
```

| Feature | Default | What it enables |
| ------- | ------- | --------------- |
| `providers` | off | HTTP providers, `reqwest` + `eventsource-stream`, `images`, `http` |
| `smoke` | off | `providers` plus a live network smoke test, gated at runtime on `ANTHROPIC_API_KEY` |

Core builds stay network-free on purpose: a test double is not a special case,
it is the default.

## What lives where

- `types` — `Message`, `Content`, `AssistantMessage`, `AssistantMessageEvent`,
  `Context`, `Tool`, `Usage`. Serialized in the provider wire shape.
- `model` — `Model` and the streaming-protocol compatibility structs.
- `schema` — tool-parameter schema generation plus the coercion that recovers
  JSON an LLM emitted slightly wrong.
- `event_stream` — the producer/consumer streaming queue behind
  `AssistantMessageEventStream`.
- `provider` — the `Provider` trait seam, `ProviderHooks`, `SimpleStreamOptions`,
  and the optional `DeferredProvider` capability for long-poll APIs.
- `providers` — concrete implementations: `faux` (always), `anthropic`,
  `openai_completions`, `openai_responses` (behind `providers`).
  DeepSeek, OpenRouter, llama.cpp and Proxy live together in the optional
  [rpi-providers package](https://github.com/pi-rust/rpi-package/tree/feature-1014/packages/rpi-providers);
  see the [migration guide](../../docs/architecture/provider-packages.md).

## The `Provider` trait

```rust
use rpi_ai::model::Model;
use rpi_ai::provider::SimpleStreamOptions;
use rpi_ai::types::Context;
use rpi_ai::AssistantMessageEventStream;

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn models(&self) -> &[Model];
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream;
}
```

`stream_simple` returns the consumer end of a stream whose producer is already
running, and reports failures as `Error` events rather than `Err`. That keeps
the harness and the agent loop uniform across providers: one code path handles
a network failure, a malformed frame, and a cancelled request.

## Stream backpressure and Rust API migration

`AssistantMessageEventStreamProducer::push(event)` is now async: await it and
stop ordinary production when it returns `false`. `split()` was removed; use
`stream.next().await` or `stream.result().await`. `result()` drains and discards
intermediate events so providers can finish even when only a result is wanted.

Ordinary events default to 64 queued entries and 4 MiB of accounted payload.
The payload walk includes message content, tool arguments, signatures and other
metadata without serializing; it measures queued payload rather than RSS or
allocator capacity; producer state, one in-flight event, and consumer-held events
are outside that budget. Shared snapshots are charged for every queued event. A
single oversized event consumes the byte budget exclusively. Terminal events
and the final result have independent slots and never wait for ordinary capacity;
the first terminal wins.

Providers should create streams with
`create_assistant_message_event_stream_with_cancellation(&options.signal)` and
use `producer.cancellation_token()` for upstream work. Dropping the stream
cancels that child token without cancelling its caller. Tests can inject limits
with `create_assistant_message_event_stream_with_limits(count, bytes, &signal)`.
An enqueue cancelled while waiting does not commit its snapshot to the aborted
result. Metadata-only frames can use `producer.commit_metadata(&message)`;
this preserves the last queued content.

## Testing without a network

The `faux` provider scripts exact turns, so a test pins the model's behaviour
rather than mocking it:

```rust
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;

let provider = FauxProvider::new(
    FauxScript::new()
        .with_tool_call("write", serde_json::json!({ "path": "out.txt", "content": "hi" }))
        .with_text("Done."),
);
let model = provider.default_model().clone();
```

A tool call followed by a text turn is enough to exercise the whole
tool → result → follow-up cycle offline. See
[`examples/tools`](https://github.com/bigfish1913/pi-rust/tree/main/examples/tools).

## Related crates

| Crate | Role |
| ----- | ---- |
| [`rpi-agent`](https://crates.io/crates/rpi-agent) | The agent loop built on this layer |
| [`rpi-harness`](https://crates.io/crates/rpi-harness) | Sessions, persistence, compaction |
| [`rpi-cli`](https://crates.io/crates/rpi-cli) | The `rpi` terminal agent |

## Documentation

- Crate docs: <https://docs.rs/rpi-ai>
- Architecture: [docs/architecture/overview.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture/overview.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
