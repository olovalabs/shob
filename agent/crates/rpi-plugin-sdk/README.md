# rpi-plugin-sdk

[![crates.io](https://img.shields.io/crates/v/rpi-plugin-sdk.svg)](https://crates.io/crates/rpi-plugin-sdk)
[![docs.rs](https://docs.rs/rpi-plugin-sdk/badge.svg)](https://docs.rs/rpi-plugin-sdk)

The stable `#[repr(C)]` ABI contract for rpi plugins. Depend on this crate, and
nothing else, to build a Rust `cdylib` that extends the `rpi` agent with tools,
providers, slash commands, event handlers, renderers and discovered resources.

The default SDK has **no dependency on the rpi runtime** on purpose. A plugin
must not link the host — both sides share only the ABI types defined here.
The optional `provider` feature adds plugin-side AI helpers, without depending
on the CLI or host loader.

## Install

```toml
[package]
name = "my-rpi-extension"

[lib]
crate-type = ["cdylib"]

[dependencies]
rpi-plugin-sdk = "0.3"
serde_json = "1"
```

## Minimal plugin

```rust
use rpi_plugin_sdk::{export_plugin, StbStringRef};

export_plugin!(|api| {
    // Optional: declare a load priority and the platforms this plugin supports.
    if let Some(declare) = api.declare {
        declare(StbStringRef::from_str(r#"{"priority":60,"platforms":["linux"]}"#));
    }

    // ... register tools, providers, commands and handlers through `api` ...

    0 // status code: 0 = success
});
```

The macro emits the `rpi_plugin_register` symbol. The version lives *inside* the
`PluginApi` struct (`abi_version`, `struct_size`) rather than in the symbol name,
so the entrypoint signature does not change when the struct grows.

## Tool prompt metadata

Like pi, a tool's provider-facing schema and its system-prompt contributions
are independent. After registering a tool, optionally register `promptSnippet`
and `promptGuidelines` through the appended `PluginApi::register_tool_prompt` slot:

```rust,ignore
if let Some(register_prompt) = api.register_tool_prompt {
    let status = register_prompt(
        StbStringRef::from_str("my_tool"),
        StbStringRef::from_str(r#"{"promptSnippet":"Brief tool summary","promptGuidelines":["Use my_tool for its specific purpose"]}"#),
    );
    if status != 0 { return status; }
}
```

The tool must already exist in the registering plugin's local registry. The host
copies the borrowed arguments immediately. Status codes are `0` (success), `1`
(unknown tool), `2` (invalid metadata object), and `-1` (outside registration).
Missing fields default to no contribution; an empty object clears the metadata.
Snippets and rules are trimmed and empty entries ignored. Only active tools
contribute; rules are deduplicated in active-tool order, even across tools and
core rules. Rules do not require a snippet. Metadata follows the winning tool
through name collisions, snapshots and reload; an extension replacing a built-in
does not inherit the built-in's prompt metadata.

A tool without a snippet is not repeated in the prompt's text list, but its
schema still makes it callable. Older plugins remain compatible: neither
`StableToolSchema` nor the ABI version changes. On older hosts the optional slot
is `None`; skip metadata registration, **not** tool registration. The CLI does not
invent rules for external tools (including `todo`); extensions must opt in.

## Safe plugin helpers

`handles::ToolHandles<T>` keeps tool state inside the plugin and returns opaque
numeric `StepHandle` tokens. `get` retains an `Arc`, `lock` serializes state
updates, and `cancel` sets a separate atomic flag without waiting for a poll.
`remove` cancels and retires the token; repeated removal and unknown handles are
no-ops. An in-flight callback keeps its state alive until it returns. Workers
can share the flag through `insert_with_cancel`.

`event_handler!` defines an event callback with a tag-checked `EventView` and
panic containment. Its accessors return owned strings, so handlers never read
the ABI union directly or retain host pointers. Event buffers are borrowed
during fan-out and must not be freed by handlers.

```rust
rpi_plugin_sdk::event_handler! {
    fn observe_message(event, _user_data) {
        if let Some(message) = event.message_text() {
            println!("{message}");
        }
        rpi_plugin_sdk::EVENT_HANDLER_CONTINUE
    }
}
```

These helpers preserve the C ABI. They require an SDK release containing the
helpers; packages using a local path dependency must publish that SDK before
publishing dependent packages.

`string_handler!` adapts command, prompt and runtime-action callbacks to `&str`
inputs and a lifetime-bound `callbacks::Output`. The tuple-input form
`fn discover((cwd, reason), output, user_data)` supports resource discovery.
`Output::write` handles null slots and reclaims earlier writes without freeing
host-owned input. The SDK's
`callbacks::free_string` is the producer-side ABI deallocator to register for
SDK-allocated outputs. The two-argument `execute_handler!` form adapts borrowed
tool call IDs and gives the body an `OwnedString` parameter, reclaiming it even
on refusal or invalid UTF-8 in the call ID. Its three-argument form preserves
raw parameters and their deallocator for custom ABI handling.
`take_step_result!` copies and reclaims a plugin-owned result into the pointer-free
`callbacks::Step` enum. It requires an explicit unsafe call site, just like the
underlying function; the macro cannot prove an arbitrary result's ownership.
`callbacks::take_step_result_with` uses a specified producer's deallocator when
receiving a result from another plugin. These are ABI boundaries: the host must still honor
buffer validity and producer allocation ownership.

Raw string readers `StbString::to_string_lossy` and `json::from_stable` require
an explicit `unsafe` call: their public ABI pointer/length fields cannot prove
that a buffer is still valid. Keep the allocation live and unchanged during
the copy, and free owned strings exactly once through their producer afterward.
This tightens the Rust source API without changing the C ABI. Code using
`EventView` or the callback trampolines continues to use owned Rust strings in
its handler body.

`callbacks::OwnedString` owns a string together with its producer's deallocator.
It cannot be copied or cloned; moving it transfers ownership, and dropping it
reclaims the allocation even on early returns or unwinding. `from_string` safely
creates an SDK-owned string. Adopting raw ABI output through `from_raw` requires
one explicit unsafe ownership contract, after which reading and cleanup are safe.
`into_raw` transfers ownership back to an ABI receiver without deallocating it.
Raw `StbString::free_with`, `FreeStringFn`, and SDK deallocation callbacks now
require unsafe calls because their public pointer inputs cannot prove unique
ownership or allocator identity. Tool partial callbacks reclaim plugin-produced
buffers through the tool's registered plugin deallocator, never the host allocator.

The optional `process` feature provides `process::ProcessTree` for terminating
descendants and closing inherited pipes. Its Win32/libc calls and owned handles
stay in the SDK; packages can enable `forbid(unsafe_code)` while using the
platform-independent interface.

## Provider plugins

Enable the optional `provider` feature when implementing an `rpi_ai::Provider`:

```toml
[dependencies]
rpi-plugin-sdk = { path = "/path/to/pi-rust/crates/rpi-plugin-sdk", features = ["provider"] }
rpi-ai = { path = "/path/to/pi-rust/crates/rpi-ai", features = ["providers"] }
serde_json = "1"
```

Replace the paths with the same compatible core checkout. Released SDK versions
through 0.6.2 do not include this feature; use the migrated source until a version
containing it is published.

`provider::completions(model, options, anonymous)` returns
`Result<Arc<dyn Provider>, String>` and reuses the OpenAI-compatible protocol
without reading vendor credentials from the process environment. Propagate
client construction errors through a `ContextFactory`; transport failures never
fall back to an unconfigured HTTP client. Changing this helper's return type
from `Arc<dyn Provider>` to `Result<Arc<dyn Provider>, String>` is a Rust source
API break included in the maintainer-managed breaking release. Migrate callers
to a fallible `ContextFactory` and propagate the result with `?`.
`export_provider!` exports a single-provider plugin from a Rust factory. For
multiple providers, keep one `export_plugin!` entrypoint and use
`provider::register` and `provider::drive` for each provider instead.
The bridge forwards streaming events, honors host cancellation and owns a
per-request runtime, so no worker survives plugin unloading.
`provider::host_capabilities()` returns the request's `capabilities` object
(`null` on older hosts) on the request thread while the bridge is driving it, so
a factory can pass it to its provider. On any other thread, including tasks
spawned under `drive_with_context`, it is `null`; read it in the factory. A provider may set `ToolCall::recorded` only when
`recordedToolCalls` is at least `1`; on a host without it, the bridge turns a
final message carrying a recorded call into an error.

The `provider` feature enables `json` and the AI, Tokio and HTTP dependencies.
It is **off by default**; tool-only plugins retain the lightweight ABI SDK,
and `default-features = false` still provides the std-only ABI core.

This replaces the former `rpi-provider-runtime` crate. Remove that dependency,
enable the SDK's `provider` feature, replace `rpi_provider_runtime::` helper
paths with `rpi_plugin_sdk::provider::`, and invoke the export macro as
`rpi_plugin_sdk::export_provider!`.

## Registered TUI text components

Use `RuntimeActionId::UiComponent` (22). Each plugin owns its own component IDs.
Register once, then send content updates without repeating the layout:

```json
{"op":"register","id":"monitor","type":"text","slot":"footer.after","order":10,"width":240,"maxHeight":1,"border":false,"visible":false}
{"op":"update","id":"monitor","text":"TPS 252 · TTFT 0.42s"}
{"op":"show","id":"monitor"}
{"op":"configure","id":"monitor","slot":"sidebar.right","width":40,"maxHeight":8}
{"op":"hide","id":"monitor"}
{"op":"remove","id":"monitor"}
```

Registration is idempotent and preserves existing content, visibility and layout.
`update` accepts `text` and optional `title`; layout fields are rejected. `configure`
changes layout while preserving text and visibility. Hidden components retain state.
Updating an unregistered ID returns an error. Plugin unload removes its components.
Use `RuntimeActionId::SetStatus` with `key` and `value` for legacy footer statuses;
the old `panel` payload has been removed.

Slots: `footer.after`, `sidebar.left`, `sidebar.right`, `overlay`.
Components sort by `order` (default 0), then plugin identity and ID.
Layout fields: `anchor`, `offsetX`, `offsetY`, `width`, `maxHeight`,
`minScreenWidth`, `border`. Anchors use the nine conventional positions from
`top-left` to `bottom-right`, with `left-center` and `right-center`.
Width is 4–240 columns, maxHeight is 1–80 rows. Content is limited to 64 lines,
2048 UTF-8 bytes per line and 256 bytes per title. Controls are stripped by the
renderer and Unicode display widths are respected. Each plugin may register 32
components. Invalid changes preserve the previous state.

Footer rows render below usage in both terminal modes. Sidebars reserve space in
fullscreen mode; native scrollback shows their content above the editor. Overlays
use the visible transcript viewport. Passive components do not change input focus.
Headless hosts store components without rendering them. Older hosts reject action 22;
plugins should handle that error. This API supports text components; interactive
questions continue to use `UiDialog`.

## Why a hand-written C ABI

Both sides are Rust, but they are compiled separately and may use different
compiler versions and different versions of this crate. That rules out passing
any Rust type that is not `#[repr(C)]` or that has a `Drop` impl. The design
rules the SDK enforces:

1. **Every crossing type is `#[repr(C)]`**, and enums in unions carry an explicit
   `#[repr(u32)]` so the discriminant width is pinned.
2. **No `Drop` type crosses.** No `Vec`, `String`, `serde_json::Value`, `Option`
   of those, or `Result`. Owned strings cross as `StbString` (pointer + length)
   together with an explicit `free_string` function the producer exports, so each
   allocation is freed exactly once, by the side that received it.
3. **Structured data crosses as JSON** in a `StbString`. `serde_json` must be
   configured consistently on host and plugin (`preserve_order` +
   `arbitrary_precision`), or integers beyond `u64`/`i64` lose precision and
   object key order can change.
4. **Unions hold `Copy` payloads only**, so a variant read is `unsafe` and the
   caller discriminates on the tag.
5. **Unwinding never crosses the boundary.** Every call is `extern "C"`, and both
   sides contain panics with `catch_unwind`; a panic is converted into a status
   code rather than aborting the process.

These are not stylistic preferences — violating any of them is undefined
behaviour at the boundary.

### Query custom entries with runtime actions

`RuntimeActionId::GetSessionBranch` uses the same canonical query as RPC
`get_session_branch`, without the RPC `type`/`id` envelope. For example, pass
this JSON to the runtime-action SDK call:

```rust
let args = serde_json::json!({
    "customTypes": ["durable-state", "other-custom-type"],
    "afterSeq": 42,
    "limit": 20,
    "order": "newestFirst"
});
let action = rpi_plugin_sdk::RuntimeActionId::GetSessionBranch;
// Submit `action` and `args` through the runtime_action slot.
```

`customTypes` is required and non-empty; each identifier must be non-blank.
Identifiers match the exact stored string, preserving case and whitespace.
The other three fields are optional;
order defaults to `oldestFirst`. `afterSeq` is exclusive in the selected order:
`seq > afterSeq` for `oldestFirst`, `seq < afterSeq` for `newestFirst`.
Entries must be on the selected branch and match a requested custom
type; ordering and limit apply globally across those types. The result is
`{"entries":[...],"count":N,"filtered":true}`.

Empty `{}`, unknown keys, and retired `customType`, `custom_type`, `custom_types`,
or `after_seq` query keys fail explicitly, including mixed old/new requests.
This does **not** change `AppendEntry`'s singular `customType` write argument.

This query contract is a breaking change from the older unfiltered action.
Upgrade hosts and consumers together. Consumers requiring filtered semantics
must check for **`filtered == true`** and reject a successful response without
that marker: older hosts can ignore the arguments and return the whole branch.
Action ID 20 and the binary ABI/layout are unchanged, so a successful plugin
load or ABI/size check alone does not prove support for this query contract.
Do not silently fall back to `{}` or replay an unfiltered response.

## Compatibility

The host loader only resolves `rpi_plugin_register`, exported with
`export_plugin!`. The host passes a `PluginApi` containing registration slots,
`runtime_action` and `declare` (priority and platforms).

The SDK checks `abi_version` against `RPI_PLUGIN_ABI_VERSION_UNIFIED` and
requires `struct_size` to cover the `PluginApi` expected by the plugin before
calling its registration body. A mismatch returns a nonzero status and the
host reports a load failure. Extensions using retired entrypoints must be
updated to `export_plugin!` and rebuilt with the current SDK.

## Versioning

A plugin using only the ABI SDK has a version decoupled from the rest of the
family. The optional `provider` helpers also depend on a compatible `rpi-ai`.
ABI compatibility is checked through `abi_version` and
`struct_size`; each optional capability must also be checked before use.
Rebuild extensions when the SDK contract they require changes.

## Examples and docs

- [`examples/plugin-stub`](https://github.com/bigfish1913/pi-rust/tree/main/examples/plugin-stub) —
  a complete plugin registering an `echo` tool with the full four-function
  lifecycle (`execute` → `poll` → `cancel` → `destroy`), an event handler, and a
  resource-discovery handler.
- [Extension authoring guide](https://github.com/bigfish1913/pi-rust/blob/main/docs/extensions/authoring.md) —
  templates, safety rules, testing and release checklist.
- Host-side loader: [`rpi-extensions`](https://crates.io/crates/rpi-extensions).
- Ready-to-install extensions:
  [`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package).

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).

## Exclusive stdio servers

During registration, `declare {"ownsStdio":true}` requests exclusive ownership
of the process's stdin/stdout. Each plugin has its own runtime context; two
owners cause a startup error, including when their file stems are identical.
Failed registration releases its claim. The host leaves stdio untouched and
runs the normal session lifecycle without rendering a TUI.

Use `RuntimeActionId::StdioLifecycle` (21) with `{"op":"status"}` to check
support and with `{"op":"finish","exitCode":0}` to signal EOF or server
completion. Only the owner can finish, and early completion is retained. The
host dispatches `SessionShutdown` and returns the code. Stop and join plugin
workers in that handler; do not terminate the process from a plugin.

The optional `stdio` feature provides `stdio::StdinLines::poll_line`. It polls
pipe/file input without a background stdin task that can block shutdown.
Wait between `InputLine::Pending` results using the server's own cancellation
mechanism. The caller must exclusively own stdin. Windows console input is
unsupported by this pipe/file server helper.

## Dialog purpose

`UiDialog` (17) keeps its open/poll/cancel protocol. A presentation `kind` such
as `selector` does not imply permission. Omitted `ui.purpose` means `question`;
`purpose: "permission"` requires a nonempty `toolCallId`, a flat single-choice
`confirm`/`selector` and explicit decision metadata on every option:

```json
{
  "op": "open",
  "requestId": "approval-1",
  "toolCallId": "call-1",
  "ui": {
    "purpose": "permission",
    "kind": "confirm",
    "question": "Allow this tool?",
    "options": [
      {"title": "Allow once", "permission": "allow_once"},
      {"title": "Reject", "permission": "reject_once"}
    ]
  }
}
```

Decisions are `allow_once`, `allow_always`, `reject_once`, or `reject_always`.
Titles are nonempty, unique and have no surrounding whitespace. Permission
questions do not accept nested questions, multiple answers or freeform input.
`dialogs::validate` and `dialogs::permission_options` share these checks between
hosts and adapters. Answers retain the selected original title in `values`.
Transport adapters that cannot show ordinary questions cancel them explicitly;
they must not reinterpret them as permission choices.

## Wake-driven tools

`PluginApi::register_tool_with_wake` accepts the existing tool lifecycle and
producer deallocator plus `ToolSetWakerFn`. It is an optional appended slot;
fall back to `register_tool` when it is absent on an older host. Installing a
waker must notify immediately if progress is already available. After returning
`Pending`, notify whenever another poll could make progress. Notifications may
come from any thread and coalesce. The host serializes all lifecycle calls per
handle and passes `None` to unregister before destroy. A racing late wake is safe;
the opaque callback token must never be dereferenced. Partial callbacks still
run synchronously within `poll` and must not be retained.

Opt into registration-scoped resources with `provider::ProviderContext`, a
`provider::ContextFactory`, and `export_provider_with_context!`. The context
lazily starts one runtime worker; concurrent request threads drive their own
streams through its handle, and emit callbacks stay on those request threads.
`context.completions(model, options, anonymous)` returns a fallible provider
using a bounded cache of 16 HTTP clients keyed by origin and effective proxy.
Client construction keeps the existing proxy and idle-timeout rules and
propagates errors. API keys and request headers stay on each request.

The optional `PluginApi::register_plugin_lifecycle` slot follows the tool wake
slot without changing earlier offsets or the ABI version. Rust `PluginApi`
struct literals must add `register_plugin_lifecycle: None` (or their registrar).
Register a pair of hooks for each owned resource context:
`begin_shutdown` must be idempotent and return without joining workers, and
`finish_shutdown` joins and releases all work on a blocking cleanup thread.
A zero finish status guarantees that no background plugin code can run again.
The host waits until active FFI calls and producer-side string frees finish;
if shutdown fails, it logs and retains the DLL and host context for the process
lifetime. It never uses a timeout to unload live code. Older hosts use the
context helper's request-scoped runtime/client fallback. Existing `Factory`,
`register`, `drive`, and `export_provider!` remain available.

Run the offline runtime/client construction comparison with:

```sh
cargo test -p rpi-plugin-sdk --features provider --release benchmark_context_resources -- --ignored --nocapture
```

The fixture drives actual context requests with a local faux provider and HTTP
client construction/cache lookup, without network requests. It reports runtime
and client builds plus request p50/p95 for scoped/shared resources, sequential
calls and four concurrent accounts; every response checks account isolation.
Timing includes request driving and is diagnostic, without pass/fail thresholds.
