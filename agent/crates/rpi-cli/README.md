# rpi-cli

[![crates.io](https://img.shields.io/crates/v/rpi-cli.svg)](https://crates.io/crates/rpi-cli)
[![docs.rs](https://docs.rs/rpi-cli/badge.svg)](https://docs.rs/rpi-cli)
[![CI](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/bigfish1913/pi-rust/actions)

The terminal coding-agent CLI — the `rpi` binary — built on the `rpi-*` library
crates. It is the ready-to-run surface of the
[rpi](https://github.com/bigfish1913/pi-rust) project: an interactive TUI, a
one-shot `-p` mode, a JSON event mode, durable sessions, and Rust `cdylib`
plugin loading.

## Install

```bash
cargo install rpi-cli
```

Requires Rust 1.88 or newer. Then point it at a provider and start it:

```bash
export ANTHROPIC_API_KEY=...     # or OPENAI_API_KEY, or ~/.rpi/agent/models.json
rpi
```

One-shot, and machine-readable output:

```bash
rpi -p "summarize the README"
rpi --mode json -p "list the crates"
```

## Authentication

Credentials are resolved in priority order:

1. `--api-key` on the command line
2. `~/.rpi/auth.json`, written by `rpi auth login` (mode `0o600` on Unix)
3. the `apiKey` field of a provider in `~/.rpi/agent/models.json`
4. provider environment variables: `OPENAI_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
   `ANTHROPIC_API_KEY`

`~/.rpi/` can be relocated with `RPI_CODING_AGENT_DIR`.

## Custom providers and gateways

Any Anthropic-Messages or OpenAI-compatible endpoint can be declared in
`~/.rpi/agent/models.json`:

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "authHeader": true,
      "headers": { "x-portkey-key": "…" },
      "models": [{ "id": "custom-claude", "name": "Custom Claude" }]
    }
  }
}
```

```bash
rpi --model gateway/custom-claude -p "hi"
```

Use `"api": "openai-completions"` for an OpenAI-compatible endpoint. Anthropic
endpoint overrides also honour `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`.

## Sessions and resources

- Sessions are JSONL v4 files under the session directory, with branching and
  context compaction.
- Project resources are discovered from `.rpi/` (skills, prompts, themes, and
  `SYSTEM.md`), with `~/.rpi/agent/` as the global layer. When both define the
  same skill or prompt name, the project copy wins.
- Plugins are loaded from project `.rpi/extensions`, global
  `~/.rpi/agent/extensions`, and `--extensions-dir`.

## Plugins

```bash
rpi install rpi-todo                           # prebuilt: repository discovered via crates.io
rpi install rpi-todo --version 0.2.0
rpi install my-extension --github owner/repo --version 1.2.3
rpi install rpi-todo --build                    # explicitly compile source
rpi install my-extension --path ../my-extension --force   # local
rpi dev                                        # build + watch a local extension
```

`rpi install` defaults to the versioned `rpi-extension.json` prebuilt protocol: it resolves a non-yanked stable package version and repository through crates.io, loads the `<package>-v<version>` Release manifest, and verifies the archive and declared library before transactionally installing it into `~/.rpi/agent/extensions`. Explicit `--github owner/repo` installs use package-specific tags and also support unpublished extensions. Both repository forms consume the same protocol; they never infer library filenames or use repository-wide latest bundles. Missing prebuilt releases fail with an actionable error; `--build` and `--path` explicitly select Cargo compilation. Installed provenance preserves the source for update discovery, reinstalls and uninstall. Actual plugin ABI/API-size validation still happens when loading the library.

`rpi install <name>` installs or upgrades an extension without `--force`. An already installed version is skipped; use `--force` to reinstall the same version or allow a downgrade. These rules also apply to explicit Cargo builds and local installs.

`rpi extension update` updates all project-linked extensions and globally installed registry/prebuilt extensions; `rpi extension update <name>` selects one extension by name. Updates retain the original Cargo build backend or explicit GitHub source. Global development installs made with `--path` are excluded. The old `rpi package update [name]` remains compatible, but `rpi extension update [name]` is recommended for extension updates. Project linking commands remain `rpi package link`, `rpi package list`, and `rpi package unlink`.

## Remote mode

```bash
rpi --server                      # headless agent over TCP, prints a token
rpi --connect host:port --token T # zero-local-resource TUI client
```

The client holds no local provider, tools, extensions or session files; all
agent work happens on the server. See
[docs/remote/user-guide.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/remote/user-guide.md).

## Default tools

The CLI loads the Pi-compatible `read`, `write`, `edit` and `bash` tools.
Additional implementations (`grep`, `find`, `ls`, `docs`, `powershell`) remain
available in the [`rpi-tools`](https://crates.io/crates/rpi-tools) library and
through the `default_tools` setting.

## Library usage

The crate also exposes its internals, so the CLI surface can be embedded:

```toml
[dependencies]
rpi-cli = "0.3"
```

For building your own agent, depend on
[`rpi-agent`](https://crates.io/crates/rpi-agent) and
[`rpi-harness`](https://crates.io/crates/rpi-harness) directly instead — see
[docs/agent/project-structure.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/agent/project-structure.md).

## RPC message identity, images and client text files

`rpi --mode rpc` exposes additive, explicitly negotiated native contracts for
optional durable message identity, ordered inline PNG/JPEG prompt input, and
client-authoritative text I/O through the existing built-in read/write/edit
tools. Legacy text/local operation remains the default; client terminal support
and an OS filesystem sandbox are not implied. See the
[native protocol contract](../../docs/message-image-file-io-rpc.md) for gates,
wire shapes, limits, authority combinations and validation scope. Consumers must
check `ready` capabilities rather than infer support from a crate version.

## Custom-entry branch queries (RPC and runtime actions)

`get_session_branch` is a filtered custom-entry query, not an unfiltered
transcript export. RPC example:

```json
{"id":"preview-1","type":"get_session_branch","customTypes":["durable-state","other-custom-type"],"afterSeq":42,"limit":20,"order":"newestFirst"}
```

`customTypes` is required and must be a non-empty array of non-blank strings.
Type identifiers match exactly as written by `append_entry`: whitespace and
case are preserved, and only exact duplicate identifiers are deduplicated.
Only `afterSeq` (non-negative integer), `limit` (positive integer), and `order`
(`oldestFirst`, the default, or `newestFirst`) are optional query arguments.
The cursor is exclusive in the selected order: `seq > afterSeq` for
`oldestFirst`, `seq < afterSeq` for `newestFirst`. Type filtering and
branch ancestry apply before global ordering and limit across all requested
types. Results are `{"entries":[...],"count":N,"filtered":true}`.

Runtime action `GetSessionBranch` accepts the same query, without RPC's `type`
and `id` envelope. Both surfaces reject `{}`, unknown fields, and the retired
query keys `customType`, `custom_type`, `custom_types`, and `after_seq`, even
when mixed with valid modern keys. This canonical contract has no alias fallback.
`append_entry` / `AppendEntry` still use singular `customType` for writes.

### Migration from unfiltered hosts

This is a breaking query-contract change: replace unfiltered `{}` or legacy
arguments with an explicit `customTypes` list. Upgrade the host and consumers
together. Older hosts may accept the new arguments but ignore them; clients
requiring filtering **must require `filtered == true`** in the reply and reject
unsupported hosts rather than treating an unfiltered branch as filtered data.
Plugin ABI compatibility alone does not establish this query capability.
`get_messages`, `get_history`, and the append/write protocol are unchanged.

## ToolProgram live branch identity (additive)

`ready.toolProgramBranch` is exactly
`{"version":1,"command":"get_tool_program_branch","identity":"bindingAndLaneGeneration"}`.
`{"type":"get_tool_program_branch","id":"branch-1"}` accepts no query arguments.
Its data is `{sessionId, lane, bindingId, generation, leafId}`: one pinned native
binding and atomic lane snapshot, bounded to validated identity strings.
`bindingId` is a live UUIDv4, shared by clones/pins and replaced on every facade
storage rebind (including back to the same storage); it is not authorization.
`generation` is the sequence of the latest explicit lane create/move mutation
(zero for the initial main lane), not the ordinary entry sequence. Appending
entries does not change it; navigation away/back does. Memory, JSONL cold/warm
replay and SQLite derive it from existing authoritative lane mutations, without
a new transcript field or migration. Unsupported custom/experimental backends
reject the identity read rather than guessing a generation.

Consumers bracket bounded snapshot/live proofs with this identity and keep
checking retained native begin/phase membership; equal generation alone is not
child-event proof. The frozen child-query/event v1 profiles, SDK/ABI, cursors,
permissions, journal and accounting semantics are unchanged. Older hosts retain
read-only child recovery but do not qualify the new live-proof path.

## Documentation

- User guide: <https://rpi.bigfisharc.cn/docs.html>
- Architecture: [docs/architecture/overview.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture/overview.md)
- Extension authoring: [docs/extensions/authoring.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/extensions/authoring.md)
- Changelog: [CHANGELOG.md](https://github.com/bigfish1913/pi-rust/blob/main/CHANGELOG.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).

This crate is part of a Rust port of the MIT-licensed TypeScript `pi` SDK
(© Mario Zechner).

### CLI self-update

`rpi update` downloads the matching GitHub prebuilt CLI using the versioned `rpi-release.json` distribution protocol. It verifies target, archive size, both SHA-256 hashes, and the executable version before using the existing atomic replacement and Windows staging helper. Version discovery uses crates.io. Only `rpi update --build` selects local Cargo compilation; missing manifests and failed checks preserve the installed executable. `--force` reinstalls and `--offline` reports cached information without downloads or builds.
