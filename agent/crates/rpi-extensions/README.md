# rpi-extensions

[![crates.io](https://img.shields.io/crates/v/rpi-extensions.svg)](https://crates.io/crates/rpi-extensions)
[![docs.rs](https://docs.rs/rpi-extensions/badge.svg)](https://docs.rs/rpi-extensions)

The host side of the plugin system: discovers, loads and drives Rust `cdylib`
plugins, and bridges their tools and events into the runtime's native async
types.

The ABI contract itself lives in
[`rpi-plugin-sdk`](https://crates.io/crates/rpi-plugin-sdk); this crate is the
other half.

## Install

```toml
[dependencies]
rpi-extensions = "0.3"
```

Most users never depend on this directly — the `rpi` CLI uses it, and the
harness consumes its adapters through injection.

## What it does

- **Loading.** `load_one` loads a single library, `load_dir` scans a directory,
  and `load_session` resolves the whole set for a session. Entrypoints are
  resolved through the single `rpi_plugin_register` symbol. Plugins must use
  the unified ABI; incompatible registrations are reported as load failures.
- **Registry.** `ExtensionRegistry` holds what plugins registered: tools,
  providers, handlers, renderers and resource-discovery callbacks, with a
  deterministic snapshot the runtime can read.
- **Tool bridge.** `PluginToolAdapter` implements the agent's `AgentTool` trait
  on top of a plugin's four-function lifecycle (`execute` → `poll` → `cancel` →
  `destroy`). The blocking `poll` loop runs on `spawn_blocking` inside the
  ambient runtime — the adapter does not own a runtime of its own. Tools registered
  with `register_tool_with_wake` use short, serialized blocking calls and sleep
  asynchronously on `Pending`, without retaining an idle blocking worker. Wakes
  during callback installation or poll are retained. Cancellation or caller drop
  cancels, unregisters the callback and destroys the handle exactly once, while
  retaining the library and invocation context through cleanup.
- **Partial results.** Tool progress flows back through an unbounded channel and
  is forwarded to `on_update`, behind a `catch_unwind` trampoline so a panicking
  callback cannot unwind across the FFI boundary.
- **Lifecycle events.** Dispatch wrappers for data, empty and lifecycle events,
  a veto path that lets a handler refuse startup, and a tee emitter so a plugin
  can observe events without stealing them from the TUI.
- **Providers.** `PluggableProvider` and `ExtensionProviderHooks` let a plugin
  serve a model or patch request options. A plugin that registers
  `register_streaming_provider` emits per-chunk envelopes that the host maps onto
  `AssistantMessageEvent`s as they arrive, so a plugin provider renders
  progressively instead of appearing all at once when the response completes. The
  same request fn also receives a `ProviderStreamControl`, and a plugin that polls
  it stops generating on a cancelled run instead of streaming a response nobody
  will read — one registration, and the plugin decides whether to check.

## Crate dependencies

`rpi-extensions` depends on `rpi-plugin-sdk`, `rpi-ai` and `rpi-agent` only —
never on `rpi-harness`. The harness receives these adapters as trait objects, so
the dependency graph stays acyclic.

## Where plugins are discovered

| Location | Scope |
| -------- | ----- |
| `.rpi/extensions` | Project (preferred) |
| `~/.rpi/agent/extensions` | Global |
| `--extensions-dir <path>` | Explicit, added by the CLI |

## Installing and developing

```bash
rpi install my-extension                  # build from crates.io and install
rpi install my-extension --path ../local  # local development
rpi dev                                   # build + watch + live reload
```

## Docs

- Crate docs: <https://docs.rs/rpi-extensions>
- [Extension authoring guide](https://github.com/bigfish1913/pi-rust/blob/main/docs/extensions/authoring.md)
- [Plugin ABI contract](https://docs.rs/rpi-plugin-sdk)
- [Ready-to-install extensions](https://github.com/pi-rust/rpi-package)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).

Plugins may register the optional two-phase lifecycle hooks. Reload calls
`begin_shutdown` outside session locks to cancel work and reject new requests.
The last FFI/string-free keepalive schedules `finish_shutdown` on a blocking
cleanup thread before unloading the library. A failed finish permanently
retains the DLL and callback context and emits a diagnostic; cleanup has no
unload timeout. The same ownership path handles partial registration failure
and ordinary session teardown.

This changes the published Rust `LoadedPlugin` layout: `library` is now a
`ManuallyDrop<Library>`, and private ownership fields prevent external struct
literals or field moves. Use `load_one` and retain the returned owner until all
plugin calls finish. This source API change needs a breaking release; compiled
plugins keep their existing ABI compatibility.

Provider streaming uses two lossless bounded queues: the typed stream and the
synchronous FFI callback bridge. Each defaults to 64 nonterminal events and
4 MiB of accounted payload. The bridge accounts incoming JSON bytes; the typed
stream walks retained content and metadata without serialization. One oversized
event occupies the entire byte budget alone. These are queue payload budgets,
not RSS limits; current mapper state, one in-flight frame, final results and
consumer-held snapshots remain outside them.

A synchronous `emit` callback may now wait for capacity. Cancellation or consumer
drop releases that wait, still freeing every supplied string through the plugin's
deallocator. Terminal delivery has its own slot. Only successful downstream
enqueues commit partial content; applied metadata-only frames commit immediately.
Already-built plugins gain host bridge backpressure. Rebuild against the updated
Rust stream API to bound the plugin's internal generation queue as well.

The source-breaking `push(...).await` / `next()` / `result()` migration is intended
for the maintainer's next breaking release. This series does not change package
versions. Agent subscriptions and UI event buses are unchanged.
