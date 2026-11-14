# rpi — a Rust-native coding-agent runtime

[![crates.io](https://img.shields.io/crates/v/rpi-cli.svg)](https://crates.io/crates/rpi-cli)
[![downloads](https://img.shields.io/crates/d/rpi-cli.svg)](https://crates.io/crates/rpi-cli)
[![docs.rs](https://docs.rs/rpi-cli/badge.svg)](https://docs.rs/rpi-cli)
[![license](https://img.shields.io/crates/l/rpi-cli.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue.svg)](#install)
[![CI](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml)
[![GitHub stars](https://img.shields.io/github/stars/bigfish1913/pi-rust?style=flat)](https://github.com/bigfish1913/pi-rust/stargazers)
[![Latest release](https://img.shields.io/github/v/release/bigfish1913/pi-rust)](https://github.com/bigfish1913/pi-rust/releases/latest)

`rpi` is a library-first coding-agent runtime written in Rust, plus a terminal
agent built on top of it. Nine composable crates span provider adapters, a
durable crash-resumable agent loop, a terminal UI, and a stable plugin ABI — so
the same code works as an embedded SDK or as a ready-to-run `rpi` command.

rpi began as a Rust reimplementation of the [Pi TypeScript SDK](https://github.com/earendil-works/pi)
by Mario Zechner. It now includes its own Rust-native plugin ABI and execution
infrastructure. See [Acknowledgements](#acknowledgements) and the repository's
[NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE) for upstream attribution.

Website: <https://rpi.bigfisharc.cn/> · Docs: <https://rpi.bigfisharc.cn/docs.html>

![rpi interactive session](docs/images/rpi-interactive.png)

<sub>Interactive TUI. See [`docs/guides/user-guide.md`](docs/guides/user-guide.md) for the full command surface.</sub>

![rpi SDK demo — an agent loop and a real tool round-trip, offline with the faux provider](docs/images/rpi-sdk-demo.gif)

<sub>The agent loop and a real `write` tool round-trip, recorded with no API key and no network — the same path the tests take. Source of truth is [`demo/sdk.tape`](demo/sdk.tape); regenerate with `task demo`.</sub>

## Three ways to use it

| You want | Depend on | Start at |
| -------- | --------- | -------- |
| A working terminal coding agent | nothing | `cargo install rpi-cli` |
| Your own agent inside your process | `rpi-agent` (+ `rpi-ai`, `rpi-tools`) | [`examples/minimal`](examples/minimal) |
| A tool/provider that extends every session | `rpi-plugin-sdk` | [`examples/plugin-stub`](examples/plugin-stub) |

The three share one implementation. Build your tools once, debug them inside an
embedded agent, and ship the same code as a `cdylib` plugin that the global `rpi`
loads. See [`docs/agent/project-structure.md`](docs/agent/project-structure.md) for the recommended
project layout.

## Install

**Prebuilt binary** (no Rust toolchain needed):

```bash
curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
```

Verifies the published SHA-256 checksum and installs to `/usr/local/bin` or
`~/.local/bin`. Preview first with `--dry-run`.

**From crates.io** (requires Rust 1.88+):

```bash
cargo install rpi-cli
rpi --version
```

**Package managers:**

```bash
brew tap bigfish1913/tap && brew install rpi          # macOS (Apple silicon), Linux
scoop bucket add bigfish1913 https://github.com/bigfish1913/scoop-bucket
scoop install rpi                                     # Windows
```

The Homebrew formula currently supports Apple silicon on macOS. Intel Macs can
use the prebuilt installer for releases with an `x86_64-apple-darwin` asset, or
`cargo install rpi-cli`. A winget manifest is [open for
review](https://github.com/microsoft/winget-pkgs/pull/444745).

**From source:**

```bash
git clone https://github.com/bigfish1913/pi-rust.git
cd pi-rust && cargo run -p minimal   # offline agent, no API key required
```

Real output of the two offline examples — no credentials, no network, identical
bytes every run. Regenerate with `task demo-transcript`:

```text
$ cargo run -q -p minimal
minimal example: 2 messages after run
  - user
  - assistant
assistant reply: Hello from the faux provider!
AgentEnd observed.

$ cargo run -q -p tools-example
tools example: 4 messages after run
wrote out.txt (28 bytes): "hello from the tools example"
AgentEnd observed; tool ran against OsExecutionEnv.
```

## Quick start

### The CLI

```bash
export ANTHROPIC_API_KEY=...          # or OPENAI_API_KEY, or ~/.rpi/agent/models.json
rpi                                   # interactive TUI
rpi -p "summarize the README"         # one-shot
rpi --mode json -p "list the crates"  # machine-readable event stream
```

Sessions persist as JSONL and survive a crash mid-run; `rpi` resumes from the
last committed frame rather than replaying the turn. Custom Anthropic or
OpenAI-compatible gateways are declared in `~/.rpi/agent/models.json` and
selected with `--model <provider>/<model>`.

### Runtime diagnostics

To investigate slow model preparation, context construction, tool execution, or
persistent assistant-frame writes, temporarily enable stderr diagnostics:

```bash
RPI_AGENT_DIAGNOSTICS=1 RPI_FRAME_DIAGNOSTICS=1 \
  rpi -p "reproduce the issue" 2>rpi-diagnostics.log
```

`RPI_AGENT_DIAGNOSTICS` reports context, request, tool, and session append
latencies. `RPI_FRAME_DIAGNOSTICS` reports durable frame batching, flush barriers,
and writer failures. Diagnostics are disabled when the variables are unset or
set to `0`; they do not change the JSONL format. See
[`docs/debugging/rust-debugging.md`](docs/debugging/rust-debugging.md) for the
field reference. The built-in read-only `docs` tool can retrieve it with
`{"topic":"debugging","query":"RPI_FRAME_DIAGNOSTICS"}`.

### Request timeouts and model compatibility

`--timeout 600` limits a whole LLM request, including its streamed response.
`--stream-idle-timeout 300` independently limits silence between response body
bytes after the response starts. Every non-empty chunk resets the idle timer,
including SSE keepalives and partial frames. A stalled stream produces a
retryable error; the configured retry policy controls whether it is retried.
Healthy streams may run longer than the idle timeout, up to the total deadline.
Use `--stream-idle-timeout 0` to disable the idle timer. You can also set
`"streamIdleTimeout": 300` (seconds) in `~/.rpi/agent/settings.json`; the CLI flag
takes precedence. `httpIdleTimeout` only expires unused pooled connections and
does not control an active response.

For providers with `"api": "openai-completions"`, put compatibility flags in
each model's `compat` object in `~/.rpi/agent/models.json`, for example:

```json
"compat": {
  "thinkingFormat": "deepseek",
  "requiresReasoningContent": true
}
```

`requiresReasoningContent` replays assistant thinking in `reasoning_content`,
including an empty string for assistant messages without thinking. The pi name
`requiresReasoningContentOnAssistantMessages` is accepted as an alias; if both
are present, the rpi spelling wins and conflicting values produce a warning.
`thinkingFormat` supports `"openai"` (the default request
shape) and `"deepseek"` (also sends `thinking: { "type": "enabled" | "disabled" }`
for reasoning-capable models). `supportsReasoningEffort: false` suppresses
`reasoning_effort` if the gateway does not accept it. `requiresThinkingAsText`
instead places thinking in ordinary text content. Unknown compat fields and
invalid values produce warnings naming the provider and model; invalid compat
objects are ignored. Other pi thinking formats are not currently supported.

### The SDK

Build an agent against a deterministic in-process provider — no API key, no
network, no flakiness:

```rust
use rpi_agent::AgentBuilder;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;

#[tokio::main]
async fn main() {
    // `FauxProvider::new` already returns an `Arc<Self>`.
    let provider = FauxProvider::new(FauxScript::new().with_text("Hello from the faux provider!"));
    let model = provider.default_model().clone();

    let agent = AgentBuilder::new()
        .model(model)
        .system_prompt("You are a helpful assistant.")
        .stream_fn(make_stream_fn(provider))
        .build()
        .expect("agent builds");

    let mut events = agent.subscribe();
    agent.prompt("hi").await.expect("prompt completes");

    while let Ok(ev) = events.try_recv() {
        let _ = ev; // AgentEvent::{MessageUpdate, ToolEnd, AgentEnd, …}
    }
    println!("{} messages after the run", agent.state().messages.len());
}
```

`make_stream_fn` bridges the synchronous `StreamFn` contract to the async
provider — the full version is in [`examples/minimal/src/main.rs`](examples/minimal/src/main.rs)
(`cargo run -p minimal`).

## Crates

Ten workspace crates, one shared version. Dependency direction is one-way
and enforced in review:

```
rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness
```

Five crates sit outside that chain:

- `rpi-tui` depends on no other `rpi-*` crate.
- `rpi-runtime` provides embeddable model catalogs, provider-scoped credentials and execution bindings on top of `rpi-ai`.
- `rpi-plugin-sdk` is a leaf too (it has no `rpi-*` deps), but it is not only
  the host's plugin contract: `rpi-tools` also reads its env-var names, so the
  host and its plugins cannot disagree about the strings they exchange across
  the ABI. That edge is names-only — nothing is linked or called.
- `rpi-extensions` is `rpi-plugin-sdk` plus `rpi-ai`/`rpi-agent`, since it hosts
  plugins that run tools.
- `rpi-cli` depends on all nine of the others, which is how the `rpi` binary
gets the TUI, the harness and the plugin loader at once.

| Crate | What it is |
| ----- | ---------- |
| [`rpi-telemetry`](https://crates.io/crates/rpi-telemetry) | Span/event contracts with a no-op default — instrumentation is opt-in, and pulls in no tracing or OpenTelemetry backend |
| [`rpi-ai`](https://crates.io/crates/rpi-ai) | Provider-agnostic message, streaming and tool-schema types; Anthropic + OpenAI-compatible + `faux` providers |
| `rpi-runtime` | Embeddable model catalog, credential resolution, model selection and private execution bindings (not yet published) |
| [`rpi-agent`](https://crates.io/crates/rpi-agent) | The agent loop: `AgentTool`, events, hooks, queues, cancellation |
| [`rpi-tools`](https://crates.io/crates/rpi-tools) | `read`/`write`/`edit`/`bash` (+ `grep`, `find`, `ls`, `powershell`) and the `ExecutionEnv` seam |
| [`rpi-harness`](https://crates.io/crates/rpi-harness) | Session tree, JSONL persistence, compaction, crash recovery, the run loop |
| [`rpi-tui`](https://crates.io/crates/rpi-tui) | Terminal UI primitives: components, layout, editor, transcript rendering |
| [`rpi-cli`](https://crates.io/crates/rpi-cli) | The `rpi` binary: TUI, one-shot and JSON modes, config, plugin loading |
| [`rpi-plugin-sdk`](https://crates.io/crates/rpi-plugin-sdk) | The stable `#[repr(C)]` ABI contract for plugins. No `rpi-*` deps; only `serde_json`, behind a default `json` feature that can be turned off |
| [`rpi-extensions`](https://crates.io/crates/rpi-extensions) | Host-side plugin loader and the async-across-ABI tool bridge |

Every crate has its own README and docs.rs page. The `crates/rpi-*`
directories and the published crate names use the same `rpi-` prefix.

## Why rpi

Most coding agents are CLIs. `rpi` is a **runtime with a CLI attached**, which
changes what you can build with it:

- **Embeddable.** The agent loop is a library. `rpi-agent` + `rpi-ai` is enough
  to run an agent inside your own process, with your own event handling and
  output — no shelling out to a tool and scraping stdout.
- **Testable offline.** A deterministic `faux` provider and an in-memory
  execution environment are first-class, not mocks bolted on. `rpi-ai` puts its
  real HTTP providers behind a `providers` feature flag, so your test suite
  cannot silently depend on the network for the paths you exercise.
- **Durable.** Sessions are a tree with JSONL persistence and per-frame
  progress. A crash mid-tool-call resumes instead of losing the turn or
  double-executing the tool.
- **Extensible across a stable boundary.** Plugins are Rust `cdylib`s behind a
  hand-written, panic-contained C ABI with version negotiation — the ABI
  survives compiler and crate version skew, so extensions do not break on every
  release.
- **One implementation, two surfaces.** The global CLI and an embedded agent load
  the same tools. You do not maintain a plugin and a library variant separately.

### How it compares

Structural differences against other terminal coding agents. Facts about other
tools are as of 2026-09 and change often — check their documentation; only the
`rpi` column is maintained here.

| | rpi | Claude Code | Codex CLI | opencode | aider |
| --- | --- | --- | --- | --- | --- |
| Language | Rust | TypeScript | Rust | TypeScript/Go | Python |
| Source available | ✅ MIT | ❌ | ✅ | ✅ | ✅ |
| Usable as an embedded library | ✅ | ❌ | ❌ | ⚠️ | ⚠️ |
| First-class plugin ABI | ✅ | hooks/MCP | ❌ | plugin API | ❌ |
| Headless server + remote client | ✅ | ⚠️ | ✅ | ✅ | ⚠️ |
| Provider-agnostic gateways | ✅ | ❌ | ❌ | ✅ | ✅ |

If you want the most polished single-vendor experience, use the vendor's tool.
`rpi` is for people who want to own the loop.

## Performance

Measured on Windows 11 (x86_64), rustc 1.97.1, `--release`. Both scripts
rebuild and re-measure on your own machine.

`sh scripts/bench.sh` — standalone numbers:

| Metric | Value |
| ------ | ----- |
| Release binary size | **21.8 MiB** |
| `rpi --version` wall time | **~17 ms** median |
| RSS once the agent is ready | **12.0 MiB** |

`node scripts/bench-vs-pi.mjs --pi <path-to-pi>` — against native Pi on the same
machine, both offline, each with a fresh isolated config directory:

| Metric | rpi (Rust) | pi (TypeScript) | Difference |
| ------ | ---------: | --------------: | ---------- |
| `--version` | **17.5 ms** | 169.5 ms | **9.7× faster** |
| Cold start | **17.7 ms** | 189.5 ms | **10.7× faster** |
| RSS at ready | **12.0 MiB** | 91.5 MiB | **7.6× smaller** |
| Install footprint | **21.8 MiB** (one binary) | ~385 MiB (+ ~91 MiB Node) | **~18–22× smaller** |

**Cold start** is spawn → the agent answering its first command: process creation
plus full initialisation, excluding teardown and any LLM work. rpi's `--version`
(17.5 ms) and cold start (17.7 ms) are effectively equal, so startup is process
creation, not initialisation; Pi is dominated by Node boot.

Isolation caveat: rpi reads `RPI_CODING_AGENT_DIR` / `RPI_OFFLINE`, native Pi reads
`PI_CODING_AGENT_DIR` / `PI_OFFLINE`. Set the matching pair for each, or rpi
silently loads the machine's global plugins.

Method, raw per-run numbers, and what is *not* measured (LLM latency, tool-loop
throughput, long sessions, TUI frame cost) are in
[`docs/performance/benchmark-vs-pi.md`](docs/performance/benchmark-vs-pi.md).

## Plugins

Plugins are Rust `cdylib` libraries behind a stable `#[repr(C)]` ABI. A plugin
can register tools, providers, slash commands, event handlers, markdown
renderers and discovered resources.

```bash
rpi install rpi-todo                    # prebuilt extension; no Rust toolchain needed
rpi install my-ext --path ../my-ext     # local, for development
rpi extension update [name]             # all eligible extensions, or one by name
rpi dev                                 # build + watch + live reload
```

Use `rpi extension update` to update all project-linked extensions and globally
installed registry/prebuilt extensions, or `rpi extension update <name>` to select
one by name. Updates preserve the original Cargo build backend or explicit GitHub
source. Global development installs made with `--path` are excluded. The old
`rpi package update [name]` remains compatible; prefer `extension update` for
extension updates. Project linking commands remain `rpi package link`,
`rpi package list`, and `rpi package unlink`.

The host loads project `.rpi/extensions`, then global
`~/.rpi/agent/extensions`, then any `--extensions-dir`.

For a new Rust plugin, start with [`docs/extensions/authoring.md`](docs/extensions/authoring.md)
and `examples/plugin-stub`. Use `rpi-plugin-sdk` for the `cdylib` ABI. If the plugin
project also needs reusable session, workspace, native RPC or durable-runtime logic
for a Web/ACP adapter, add [`rpi-extcore`](https://crates.io/crates/rpi-extcore)
(`rpi-extcore = "0.1"`). It is a normal Rust library, not the `cdylib` entry point,
and it does not replace `rpi-plugin-sdk`; a simple tool plugin should not add it
unnecessarily.

Ready-to-install extensions live in the companion
[`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package) repository, with
an [online catalog](https://rpi.bigfisharc.cn/packages.html).

### Optional web and subagent tools

```bash
rpi install rpi-websearch                # websearch: titles, URLs and snippets
rpi install rpi-webfetch                 # webfetch: readable text from public pages
rpi install rpi-subagents                # delegate_task: host-managed child agents
```

Restart rpi after installation. These are independent extensions, not default
CLI tools. Web search/fetch do not provide browser clicks, login flows or page
JavaScript execution.

Subagents inherit a snapshot of the parent's current model/provider binding,
thinking level, prompt, resources, request options, active tools, cwd and
approval hooks. Credentials stay inside the host. Children use independent
in-memory sessions without copying or modifying the parent history; their tool
set can only be narrowed, and recursive delegation is rejected. Parent approvals
remain in effect, and parent completion or cancellation stops the child. This is
not an OS sandbox. Use compatible host and extension versions; unsupported hosts
fail explicitly rather than starting a subprocess with different defaults.

The SDK entry point is `AgentHarness::create_child`; extensions use
`GetExecutionContext` and `ChildAgent` runtime actions. See the
[subagent protocol](https://github.com/bigfish1913/pi-rust/blob/main/docs/extensions/subagents.md)
for lifecycle, limits and compatibility details.

### Authoring an extension

```rust
use rpi_plugin_sdk::{export_plugin, StbStringRef};

export_plugin!(|api| {
    if let Some(declare) = api.declare {
        declare(StbStringRef::from_str(r#"{"priority":60,"platforms":["linux"]}"#));
    }
    // register tools / handlers through `api`
    0
});
```

Start from [`examples/plugin-stub`](examples/plugin-stub) — a complete plugin
with the full `execute`→`poll`→`cancel`→`destroy` lifecycle — and read
[`docs/extensions/authoring.md`](docs/extensions/authoring.md) for the safety
rules, which are load-bearing rather than stylistic.

## Remote mode

```bash
rpi --server                        # headless agent over TCP, prints an auth token
rpi --connect host:port --token T   # TUI client with zero local agent resources
```

The client holds no provider, tools, extensions or session files — all agent
work happens on the server, so a thin terminal anywhere can drive a beefy
machine. See [`docs/remote/user-guide.md`](docs/remote/user-guide.md).

## Project status

| | |
| --- | --- |
| Current release | **[0.21.0](https://github.com/bigfish1913/pi-rust/releases)** (nine crates, published together) |
| Stability | `rpi-ai`, `rpi-agent`, `rpi-tools`, `rpi-harness`, `rpi-plugin-sdk` are the intended stable surface |
| MSRV | 1.88 |
| Platforms | Linux (GNU and musl), macOS, Windows (CI runs the suite on Linux) |
| Binary release targets | Linux (x86_64 GNU/musl, aarch64 GNU), macOS (Intel/Apple silicon), Windows (x86_64) |

`rpi` is a self-contained Rust coding-agent stack. No Node component is
required and none is loaded.

## Documentation

- [User guide](docs/guides/user-guide.md) — commands, config, sessions, skills
- [Architecture](docs/architecture/overview.md) — how the crates fit together and why
- [Building an agent project](docs/agent/project-structure.md) — recommended structure
- [Extension authoring](docs/extensions/authoring.md) — plugin templates and ABI rules
- [Subagents](https://github.com/bigfish1913/pi-rust/blob/main/docs/extensions/subagents.md) — configuration inheritance, permissions and lifecycle
- [Remote mode](docs/remote/user-guide.md) — protocol, tokens, limitations
- [Roadmap](ROADMAP.md) · [Changelog](CHANGELOG.md)
- [docs.rs](https://docs.rs/rpi-cli) for per-crate API docs

## Contributing

Issues and PRs are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md) for the
build/test commands and the repository layout, and the
[roadmap](ROADMAP.md) for what is already planned. Security problems should go
through [SECURITY.md](SECURITY.md), not a public issue.

The whole test suite runs offline: `cargo test --workspace --locked`.

## Support / 赞赏支持

If rpi helps you, consider buying the author a coffee. Tips and donations support continued development and maintenance. Thank you!

如果 rpi 帮到了你，欢迎请作者喝杯咖啡，赞赏或捐助支持持续开发与维护。感谢每一份支持！

| 微信支付 / WeChat Pay | 支付宝 / Alipay | Binance Pay |
| :---: | :---: | :---: |
| ![微信支付赞赏码](../images/weixinsk.png) | ![支付宝赞赏码](../images/alipay.jpg) | ![Binance Pay 赞赏码](../images/bian.png) |

扫码或点击图片查看原图；手机上可保存图片后，在对应 App 中识别。 / Scan the code or click to view the original. On mobile, save the image and scan it from the matching app’s photo library.

## Community

Questions, ideas and early builds are welcome in either group:

<table>
  <tr>
    <td align="center" width="50%">
      <img src="docs/images/weixin.png" width="230" alt="WeChat group QR code"><br>
      <sub><b>WeChat</b> — <a href="https://weixin.qq.com/g/AQYAAD8z3FmKPk143hi_HUVOtGvNqlHWMtVZu1VGGB_46JKoNpyhJvS7kDwQwUTr">微信交流群</a></sub>
    </td>
    <td align="center" width="50%">
      <img src="docs/images/tg.png" width="230" alt="Telegram group QR code"><br>
      <sub><b>Telegram</b> — <a href="https://t.me/+7YfN4TZPP7Y0M2Q1">t.me/+7YfN4TZPP7Y0M2Q1</a></sub>
    </td>
  </tr>
</table>

<sub>Group invite codes expire — WeChat's rotates every few days. If a QR code stops working, open an issue and it will be refreshed.</sub>

## Acknowledgements

Thank you to Mario Zechner and the [Pi contributors](https://github.com/earendil-works/pi)
for the original TypeScript SDK and the agent, tool and session designs that
formed rpi's starting point. rpi reimplements that foundation in Rust and adds
its own runtime and extension infrastructure; it does not load the TypeScript
SDK or its npm extensions. Upstream copyright and MIT license notices are
retained in the repository's LICENSE and NOTICE files.

## License

MIT. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The `rpi-*` crates are a Rust-native coding-agent stack.

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=bigfish1913/pi-rust,pi-rust/rpi-package&type=Date)](https://www.star-history.com/#bigfish1913/pi-rust&pi-rust/rpi-package&Date)
