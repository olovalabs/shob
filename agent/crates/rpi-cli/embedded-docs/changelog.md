# Changelog

All notable changes to the `rpi-*` crate family are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The rpi-* crates share one workspace version and are released together
in dependency order. `git tag vX.Y.Z` marks the commit that a release was cut
from; the matching [GitHub Release](../../releases) carries the same notes.

## [Unreleased]

## [0.21.0](https://github.com/bigfish1913/pi-rust/compare/v0.20.0...v0.21.0) - 2026-11-06

### Added

- *(plugin-sdk)* tool_result transforms can rewrite a finished tool result ([#240](https://github.com/bigfish1913/pi-rust/pull/240))
- allow configuring release CDN in settings
- add optional release CDN mirror
- version native extension artifacts for hot reload
- *(release)* support x86_64 musl binaries

### Fixed

- *(tui)* keep runtime notices in transcript order ([#275](https://github.com/bigfish1913/pi-rust/pull/275))
- *(cli)* tokenize docs queries and fall back across topics ([#272](https://github.com/bigfish1913/pi-rust/pull/272))
- *(tools)* classify bash command failures in tool details ([#271](https://github.com/bigfish1913/pi-rust/pull/271))
- *(tui)* gate file autocomplete on path appearance ([#264](https://github.com/bigfish1913/pi-rust/pull/264))

### Other

- Merge remote-tracking branch 'origin/main' into feat/cdn-rustfs
- *(read)* resize images with fast_image_resize ([#241](https://github.com/bigfish1913/pi-rust/pull/241))

## [0.20.0](https://github.com/bigfish1913/pi-rust/compare/v0.19.0...v0.20.0) - 2026-11-05

### Added

- *(plugin-sdk)* add ReloadModels for a catalog-only refresh
- *(config)* add provider update and delete RPCs
- *(config)* persist image input support
- *(config)* persist maximum output tokens
- *(config)* persist provider context window
- *(rpc)* probe provider models and connectivity
- *(rpc)* persist provider configuration

### Fixed

- *(rpc)* raise ACP inline image prompt limit to 1 MiB
- *(tui)* keep completed plan in transcript order ([#257](https://github.com/bigfish1913/pi-rust/pull/257))

### Other

- Merge branch 'main' into feat/web-provider-config
- Merge branch 'main' into feat/web-provider-config

## [0.19.0](https://github.com/bigfish1913/pi-rust/compare/v0.18.0...v0.19.0) - 2026-11-05

### Added

- *(harness)* expose bounded ToolProgram child queries and commit events
- *(runtime)* add scoped native tool programs for Code Mode
- *(agent)* observe genuine finalized ToolProgram children
- *(tool-program)* resolve ordered native content attachments
- *(codemode)* retain native results for bounded attachment
- *(codemode)* preserve business values and guard invocation outputs
- *(runtime)* preserve native results accounting and parent progress
- *(harness)* persist genuine ToolProgram child observations
- *(harness)* archive bounded native bodies in primary sessions

### Fixed

- *(rpc)* expose stable ToolProgram branch identity
- *(rpc)* preserve canonical native tool result signatures
- *(runtime)* drain panicking tool descendants and retain termination

### Other

- *(rpc)* simplify the owned child commit pump
- add official extension compatibility gate ([#255](https://github.com/bigfish1913/pi-rust/pull/255))
- *(harness)* initialize rebinding storage fixtures

## [0.18.0](https://github.com/bigfish1913/pi-rust/compare/v0.17.2...v0.18.0) - 2026-11-04

### Fixed

- *(cli)* make context dismissible and align skill commands with pi ([#251](https://github.com/bigfish1913/pi-rust/pull/251))

## [0.17.2](https://github.com/bigfish1913/pi-rust/compare/v0.17.1...v0.17.2) - 2026-11-04

### Fixed

- *(tui)* preserve startup output above the live transcript
- parse JSON floats exactly so committed costs compare equal

### Other

- update Cargo.toml dependencies

## [0.17.1](https://github.com/bigfish1913/pi-rust/compare/v0.17.0...v0.17.1) - 2026-11-04

### Fixed

- *(resource)* resolve external scope prefixes before leasing

## [0.17.0](https://github.com/bigfish1913/pi-rust/compare/v0.16.0...v0.17.0) - 2026-11-04

### Added

- *(plugin-sdk)* add StopRound for finishing the current turn only
- *(plugin-sdk)* relay host UI dialogs to an external UI
- *(plugin-sdk)* expose prompt templates in command listing
- *(session)* share storage indexes and lazily page JSONL history
- *(agent)* report queued message count on turn_end
- *(cli)* add nonexecuting saved session resource reader
- *(cli)* add explicit same-session resource transitions
- *(cli)* opt in fresh print sessions to experimental native store
- *(cli)* manage local packages skills and extensions
- *(cli)* add exact native session resource selection
- *(cli)* add exact native session resource selection
- *(cli)* add extension update with legacy package compatibility
- *(cli)* cancel provider OAuth calls when the TUI flow ends
- *(extensions)* deliver OAuth cancellation to cancellable plugins
- *(cli)* install runtime assets declared by prebuilt packages
- *(plugin-sdk)* append a cancellable OAuth registration slot
- *(durable)* share native progress checkpoints and restore pending queues
- *(durable)* index experimental v3 queries and UI pages
- *(durable)* add opt-in experimental native session backend

### Fixed

- preserve cold history validation and streamline session restore
- preserve cold fork leaves and initialize verified resource authority before publication
- enforce saved-read budgets and classify changed-branch cursors before anchors
- *(cli)* canonicalize smoke fixtures and reject experimental extension dispatch
- *(cli)* read steering text from the per-message view
- *(cli)* drop unused queued_message_view_payloads
- *(cli)* hand aborted queue back and keep per-queue views
- *(harness)* reclaim queued messages atomically on abort
- complete the relay lifecycle and unblock action 28
- *(cli)* drop the unused merge_command_list wrapper
- *(cli)* keep built-ins in the listing when reserving names
- *(cli)* publish empty reloads and reserve built-in names
- *(cli)* protect managed packages and preserve resource compatibility
- *(cli)* validate selected Skills before startup side effects
- *(cli)* validate selected Skills before startup side effects
- *(cli)* initialize resource identity during native fork
- make the TurnEnd payload ABI-safe and propagate it
- *(cli)* honor deliverAs in SendUserMessage
- *(tui)* derive spinner frames from wall clock at upstream cadence
- *(cli)* keep history browsing alive after a single-line recall
- *(cli)* move the caret through a multi-line draft before history
- *(session)* lazily materialize legacy streaming frames
- *(ai)* share image data between message copies
- *(cli)* keep Linux clipboard data alive after selection copies
- *(cli)* anchor autocomplete candidates to the editor
- *(session)* upgrade locked legacy native sessions for progress
- *(cli)* publish the OAuth flow id and its cancel token together
- *(harness)* make host recovery and message settlement standalone
- *(cli)* claim the OAuth flow before writing or deleting a credential
- *(cli)* remove directories a failed sidecar install created
- *(cli)* never treat an unowned sidecar as owned because of its case
- *(cli)* prune a sidecar whose name changed only in case
- *(cli)* reject Windows device names and trailing-dot sidecar paths
- *(harness)* preserve run ownership and queued handoffs across caller drop
- *(tools)* keep Windows shells suspended until they are in their job
- *(test)* move the trampoline ceiling test into `mod tests`
- *(extensions)* restore the relay test attribute and doc comment
- *(test)* move the trampoline ceiling test into `mod tests`
- *(extensions)* keep delivering the turn_end lifecycle event
- *(extensions)* yield parallel tool scheduling during observer callbacks
- *(tools)* kill Windows shell trees through a job object
- *(harness)* reclaim native handles and classify create conflicts
- *(harness)* recover recognized native hot journals before identity reads
- *(session)* keep retired frame IDs reserved
- *(session)* drop retired assistant frames from memory
- *(harness)* hold AgentEnd until final assistant fallback is durable
- *(harness)* retry final assistant entry ACK with its native ID
- *(tui)* preserve native scrollback during live updates
- *(tui)* share folded paste bodies across undo snapshots

### Other

- integrate latest main StopRound into preview
- Merge main recovery store changes into StopRound preserving stream preconditions
- retain latest preview runtime actions in resource transitions
- preserve bounded history and strict transition reads on current resource base
- Merge main into preview preserving native policy and runtime actions
- Merge main into UI dialog relay and preserve all runtime actions
- reconcile b with current preview storage and guards
- Merge main into command listing and preserve atomic abort actions
- Integrate bounded history and experimental backend policy with resource management
- retain atomic abort and Notice fixes in optimized preview
- Merge Notice and atomic abort fixes while preserving complete implicit forks
- Merge reviewed preview Notice/abort fixes into resource d
- Merge reviewed preview Notice/abort fixes into resource c
- integrate bounded session history optimizations into preview
- Merge reviewed preview Notice/abort fixes into resource b
- Merge atomic abort recovery and restore pending Notice retry scheduling
- *(session)* bound cold history reads and RPC state hydration
- Merge updated resource reader and validate fork source identity
- Merge updated transitions into nonexecuting saved-session reader
- Merge updated resource foundation into same-session transitions
- Merge preview into local resource management and preserve startup ownership
- Merge main into preview and reconcile recovery/resource contracts
- rustfmt the merge_command_list_reserved test call sites
- Merge branch 'main' into pr/4-pending-messages
- Merge branch 'main' into codex/session-resource-foundation
- Merge branch 'main' into docs/skill-frontmatter-fields
- *(skills)* describe the frontmatter fields the loader consumes
- Merge reviewed resource fixes into native fork configuration
- Merge reviewed resource fixes into saved session reads
- Merge reviewed resource fixes into controlled rebind
- synchronize main into preview with session history optimizations
- integrate shared storage and lazy session history into preview
- *(preview)* checkpoint extension updates and resource regression tooling
- Merge branch 'main' into fix/spinner-cadence
- Merge branch 'main' into fix/slash-panel-anchor
- Merge branch 'main' into upstream/runtime-assets
- Merge branch 'main' into fix/linux-clipboard-persistence
- Merge branch 'main' into fix/slash-panel-anchor
- Merge branch 'main' into upstream/runtime-assets
- Merge branch 'main' into fix/linux-clipboard-persistence
- Merge branch 'main' into upstream/oauth-cancel
- sync embedded docs with the changelog entry
- Merge main v0.16.0 into preview
- Merge main into preview for the next preview release
- *(cli)* describe only the runtime-assets.json index the host reads
- *(oauth)* a cancelled login never saves a late credential, across reloads
- *(cli)* list Windows device names among runtime asset path rules
- *(cli)* cover runtime asset path, digest and entry-count cases
- *(cli)* document runtime assets in the install guide
- *(ai)* isolate body timeout from loopback connection timing
- await persisted stream frames and cover integrated runtime actions
- Merge remote-tracking branch 'upstream/main' into fix/windows-kill-job
- Merge tested preview integration into local resource management
- *(oauth)* pin the old keepalive and a live call across a reload
- *(oauth)* keep an in-flight OAuth call's library mapped across an unload
- rustfmt after the rebase onto the turn_end suite
- Merge branch 'main' into fix/retire-frames-in-memory
- *(harness)* cover large progress spill and prune crash boundaries
- *(harness)* verify isolated native transaction process faults
- *(durable)* verify native fact projections against session reducer
- *(durable)* prototype bounded recovery and lazy indexed history
- *(tui)* add native scrollback acceptance contracts

### Fixed

- *(cli)* keep fullscreen mouse-selection copies on Linux: the arboard clipboard
  handle now lives for the whole process instead of being dropped right after
  each copy, which on X11 destroyed the selection data unless a desktop
  clipboard manager happened to take it over (LXQt/Openbox runs none), so the
  "Copied" toast was followed by an empty clipboard

## [0.16.0](https://github.com/bigfish1913/pi-rust/compare/v0.15.0...v0.16.0) - 2026-11-01

### Added

- *(rpc)* add durable message IDs, ordered images and delegated text IO
- *(rpc)* add paginated user summaries and bounded message windows
- *(cli)* hot-reload the active theme file via fs_watch
- *(cli)* consume extension themePaths into the theme registry
- *(cli)* discover theme dirs and list custom themes in /theme
- *(cli)* accept theme file paths in --theme and settings
- *(rpc)* add bounded external tool registration and execution
- *(tui)* parse pi theme JSON files into ThemeColors

### Fixed

- *(cli)* hot-reload through the registry onto the session palette
- *(cli)* resolve themes through the registry at startup and reload
- *(tui)* wrap table cells instead of clipping narrow columns
- *(tui)* expand pasted tabs to the next tab stop, measured per span
- *(cli)* buffer Windows paste bursts before folding
- *(rpc)* cancel foreground before returning external writer failures
- *(tools)* finish a UTF-8 character cut by the pending limit before deciding
- *(tools)* decode independently of read boundaries; finish on bounded drain
- *(tools)* prevent browser launch during doctests
- *(tools)* decode shell output across reads and in the console code page
- *(harness)* retain run admission through image preparation
- *(tui)* keep CRLF rows and post-escape accents through the shared wrapper
- *(tui)* validate slotless theme tokens when loading
- *(tui)* avoid blocking mouse selection on terminal output

### Other

- Merge main before final ACP capability qualification
- Merge branch 'main' into feat/paginated-user-message-queries
- Merge branch 'main' into fix/theme-hot-reload
- Merge branch 'main' into fix/theme-ext
- Merge branch 'main' into fix/theme-dirs
- Merge branch 'main' into fix/theme-load
- Merge main into fix/tui-table-wrap; retain both changelog entries
- *(cli)* format the reload registry block
- *(tools)* assert prompt output after UTF-8 limit lookahead
- Merge branch 'main' into fix/shell-output-decoding
- Merge remote-tracking branch 'origin/main' into codex/acp-stage3-external-tools
- add session recovery and extension regression workflow
- Merge branch 'main' into fix/theme-json

### Changed

- *(tui)* pasted tabs now expand to the next 8-column tab stop (counting wide chars as 2 columns, resetting at each newline) instead of a fixed four spaces, so a paste lines up with how the source it was copied from renders it; text that relied on "tab = exactly four spaces" changes what gets sent

### Fixed

- *(tui)* markdown table cells wrap without splitting a displayed Unicode sequence: `wrap_text_with_ansi`, `visible_width` and the per-column width floor now measure grapheme clusters (and whole ANSI-free spans) instead of summing scalars, so a ZWJ sequence (`👩‍💻`), a skin-tone modifier (`👍🏽`) or a VS16 sequence (`❤️`) stays on one row at ordinary and narrow terminal widths
- *(tui)* pasted tab stops are measured on each complete tab-free span, so a tab after a width-sensitive Unicode sequence (`❤️`, `👩‍💻`, `👍🏽`) or a cross-grapheme ligature (Arabic Lam-Alef `لا`) lands on the same column the terminal shows instead of one or two columns off

## [0.15.0](https://github.com/bigfish1913/pi-rust/compare/v0.14.0...v0.15.0) - 2026-10-30

### Added

- *(extensions)* carry recorded tool results through plugin streams
- *(ai)* add a recorded field to tool calls and stream frames
- *(extensions)* advertise recorded tool calls in provider requests

### Fixed

- *(cli)* render each autocomplete row as exactly one screen line
- *(cli)* compare the expiry too before writing a refresh back
- *(prompt)* align tool snippets and guidelines with Pi
- *(rpc)* [**breaking**] default to compact message deltas with migrated consumers
- *(cli)* drop a refresh result when login or logout changed the credential
- *(cli)* renew OAuth logins per provider, not under one global lock
- *(runtime)* refresh stored OAuth logins before provider requests
- *(agent)* never execute recorded tool calls
- *(plugin-sdk)* refuse recorded calls on hosts that cannot honor them
- *(extensions)* keep plugin call ids out of the fail-closed error text
- *(extensions)* keep a streamed recording through the final message
- *(tui)* drop the selected row's trailing space
- *(tui)* size the settings label column to its labels
- *(tui)* render overlays inside the frame's cell geometry scope
- *(tui)* scope image geometry to the frame's polled cells
- *(tui)* give each renderer its own polled cell geometry
- *(tui)* poll cell geometry on the main screen and harden the probes
- *(tui)* key image caches on cell geometry, not just width
- *(tui)* size inline images from the terminal's real cell size

### Other

- *(extensions)* define recordedToolCalls as the highest decoded version
- *(extensions)* document the recorded capability and the final out check
- Merge branch 'main' into upstream/recorded-tool-calls
- Merge branch 'main' into upstream/oauth-auto-refresh
- Merge branch 'main' into fix/autocomplete-panel-flicker
- Merge branch 'main' into upstream/oauth-auto-refresh
- *(ai)* cover unversioned OpenAI proxy prefixes
- *(ai)* frames written before the recorded field reduce to an ordinary call
- *(agent)* a recorded call never reaches after_tool_call
- *(agent)* an explicit retry of a recorded call runs once
- *(extensions)* collapse two nested if-lets in the recorded-call tests
- add the #183 review's provider-to-agent and JSONL recovery checks
- *(extensions)* a toolcall_end without a recording stays an ordinary call
- *(tui)* probe renderer-local cell geometry isolation

### Changed

- **Breaking (RPC wire format):** `--mode rpc` now defaults to `compact-deltas`. Text, thinking, and tool-call delta updates omit the cumulative `message` and `assistantMessageEvent.partial` snapshots while retaining nested delta fields and the top-level `eventType`, `contentIndex`, and `delta` mirrors. `ready.semantics.messageUpdates` identifies the selected mode; a missing field means legacy `snapshot`. Upgrade remote clients and protocol adapters before deploying the new host. `--rpc-message-updates=snapshot` is an explicit, temporary migration fallback, not the long-term default. Structural events, authoritative final messages, settlement and canonical branch-query contracts, plugin serialization, and `--mode json` are unchanged. This is not a fully snapshot-free or lossless replay protocol. ([#171](https://github.com/bigfish1913/pi-rust/issues/171))

### Fixed

- Remote TUI streaming now accumulates indexed compact text, thinking, and tool-argument updates and reconciles structural/final snapshots without appending deltas twice; older snapshot senders remain supported. Compact tool deltas include the provider's currently known `id`/`toolName` so identity discovered after start is not lost.
- OpenAI Responses function-call deltas include any initial raw argument prefix exactly once on their first delta, matching the existing custom-tool behavior. This provider correctness fix applies to all event consumers; final message and snapshot contents are unchanged.
- OpenAI Completions and Responses flush batched text/thinking before crossing channel/structural boundaries, preventing a snapshot from exposing content whose later compact delta would append it again. Real-provider offline SSE regressions compare both wire modes at every UI update.

## [0.14.0](https://github.com/bigfish1913/pi-rust/compare/v0.13.0...v0.14.0) - 2026-10-30

### Added

- publish session entry commit events

### Fixed

- *(rpc)* bound branch queries and preserve type identities
- *(tui)* keep the editor's wrap width independent of the caret row ([#165](https://github.com/bigfish1913/pi-rust/pull/165))
- *(cli)* honor thinkingBudgets from settings.json ([#169](https://github.com/bigfish1913/pi-rust/pull/169))
- *(rpc)* support multi-type branch filters ([#159](https://github.com/bigfish1913/pi-rust/pull/159))
- *(rpc)* filter session branch reads ([#150](https://github.com/bigfish1913/pi-rust/pull/150))
- *(rpc)* drain commit events and notify partial batch commits
- *(rpc)* publish session commit events for atomic entry writes
- *(ai)* give xhigh and max their own thinking budgets ([#170](https://github.com/bigfish1913/pi-rust/pull/170))
- *(config)* keep compat on anthropic-messages models from models.json ([#168](https://github.com/bigfish1913/pi-rust/pull/168))

### Other

- bring preview canonical branch queries into main
- *(rpc)* [**breaking**] require canonical customTypes branch queries ([#160](https://github.com/bigfish1913/pi-rust/pull/160))
- Merge remote-tracking branch 'origin/main' into HEAD
- Merge branch 'main' into preview
- cover RPC session commit publication
- centralize durable business state

### Changed

- **Breaking (rpc/extensions):** `get_session_branch` / `GetSessionBranch` now require an explicit non-empty `customTypes` array, support exclusive `afterSeq`, order and global limit, and reject unfiltered/legacy query arguments. Upgrade hosts and clients together; require `filtered: true` in replies because older ABI-compatible hosts may ignore filters. Custom-type identifiers match exactly, and bounded multi-type queries limit each storage read before merging. The append/write protocol and binary plugin ABI are unchanged.

### Fixed

- *(tui)* keep the editor's wrap width independent of the caret's row: a row used to wrap one column earlier while the caret sat on it, so the same draft reflowed (and the input box grew a line) once the caret moved away

## [0.13.0](https://github.com/bigfish1913/pi-rust/compare/v0.12.0...v0.13.0) - 2026-10-29

### Fixed

- *(tui)* keep Windows mouse selections aligned with painted frames ([#166](https://github.com/bigfish1913/pi-rust/pull/166))
- *(bash)* enforce a default tool execution timeout ([#164](https://github.com/bigfish1913/pi-rust/pull/164))
- *(cli)* say when the terminal cannot render inline images ([#161](https://github.com/bigfish1913/pi-rust/pull/161))
- *(cli)* classify session storage failures
- *(cli)* stop footer counters freezing after dropped events
- *(harness)* surface session write failures to the user ([#136](https://github.com/bigfish1913/pi-rust/pull/136))
- *(ai)* redact credentials from stream and image option debug output ([#154](https://github.com/bigfish1913/pi-rust/pull/154))
- *(bash)* read output while the command runs ([#163](https://github.com/bigfish1913/pi-rust/pull/163))
- *(tui)* crop kitty images to the viewport ([#158](https://github.com/bigfish1913/pi-rust/pull/158))

## [0.12.0](https://github.com/bigfish1913/pi-rust/compare/v0.11.1...v0.12.0) - 2026-10-28

### Fixed

- *(cli)* return to a restored session's working directory ([#141](https://github.com/bigfish1913/pi-rust/pull/141))
- *(tui)* mark kitty image chunks as continued ([#145](https://github.com/bigfish1913/pi-rust/pull/145))

### Changed

- *(cli)* restoring a session (`-c`, `-r`, `--session`, an existing `--session-id`) now runs in the session's recorded working directory instead of the launch directory, and announces the switch (naming both directories) because tools, context files, and extensions follow it; when that directory no longer exists the launch directory is kept and the fallback is announced. Project trust is re-resolved against the adopted directory, so an untrusted project's settings, skills, and context files stay off until allowed

### Fixed

- *(tui)* switching to a session from another directory says where the session belongs instead of implying the tools moved

## [0.11.1](https://github.com/bigfish1913/pi-rust/compare/v0.11.0...v0.11.1) - 2026-10-28

### Added

- *(rpc)* publish session entry commit invalidations ([#147](https://github.com/bigfish1913/pi-rust/pull/147))

### Fixed

- *(cli)* resolve CLI updates from GitHub releases ([#146](https://github.com/bigfish1913/pi-rust/pull/146))

### Other

- *(agent)* cover provider cache invalidation ([#148](https://github.com/bigfish1913/pi-rust/pull/148))

## [0.11.0](https://github.com/bigfish1913/pi-rust/compare/v0.10.1...v0.11.0) - 2026-10-27

### Added

- *(rpc)* expose registered extension commands

### Fixed

- *(harness)* let the host wait for a session's durable writes
- *(cli)* take the highlighted slash completion on Enter
- *(prompt)* state each guideline once
- *(harness)* run requests carry the session id, so providers can key caching on it
- *(openai-responses)* send the session id as prompt_cache_key
- *(tui)* keep a rewritten Shift+Enter from being dropped

### Other

- rustfmt the new prompt test
- make stable release preparation manual

## [0.10.1](https://github.com/bigfish1913/pi-rust/compare/v0.10.0...v0.10.1) - 2026-10-27

### Added

- *(tui)* expose skill activation and slash invocation

### Fixed

- *(cli)* browse into a directory when a path prefix ends at a separator
- *(cli)* keep the path directory and apply one Enter snapshot
- *(cli)* complete the whole command token on Enter
- *(cli)* classify Windows paste before Enter completion and stamp explicit selection
- *(cli)* resolve slash completion on Enter before submitting the prefix
- *(session)* --fork takes a session file outside the session directory

### Other

- Merge branch 'main' into fix/enter-accept-completion
- Merge branch 'main' into feat/tui-skill-invocation
- *(cli)* compare the completed path reference by normalized separator
- Merge branch 'main' into fix/fork-session-file

## [0.10.0](https://github.com/bigfish1913/pi-rust/compare/v0.9.6...v0.10.0) - 2026-10-27

### Added

- add preview release channel
- *(anthropic)* add per-phase build diagnostics and refactor clamping
- *(harness)* add optional compressed frame sidecars
- *(harness)* add opt-in frame sidecar storage
- *(harness)* add optional frame sidecar codec

### Fixed

- restore RPC compatibility and remove perf artifacts
- *(btw)* align side chat footer
- *(btw)* animate side chat progress
- *(session)* preserve durable commits and validate batched operations

### Other

- sync stable 0.9.6 into reviewed preview
- sync latest stable 0.9.4 before preview build
- integrate reviewed features-1025 fixes into preview
- Merge remote-tracking branch 'origin/main' into preview
- raise minimum supported Rust to 1.88
- 更新文档开篇说明与来源许可章节
- 补充 rpi-extcore 复用边界并更新 SDK 版本
- *(harness)* batch durable recovery mutations
- optimize long Anthropic request preparation
- *(agent)* cache incrementally converted provider messages
- *(agent)* preallocate provider message conversions
- *(session)* 优化上下文构建与帧写入
- *(harness)* await persisted retry frames before crash recovery
- *(harness)* move JSONL append buffers without cloning
- *(harness)* encode JSONL batches into one buffer
- *(harness)* preallocate batched JSONL appends
- *(harness)* remove unused frame sidecar prototype
- *(harness)* keep sidecar codec out of core storage
- *(harness)* deduplicate frame sidecar payloads
- *(harness)* guard extension entries around frames
- *(harness)* measure compressed frame sidecars
- *(harness)* audit durable record sizes

### Changed

- Raise the minimum supported Rust version to 1.88 and check Linux and Windows.
- Reduce repeated request preparation, context projection and stream-frame copies.

### Fixed

- Keep JSONL durable appends and reducer commits consistent after cancellation.
- Reject conflicting JSONL sequence numbers without deleting source records.
- Preserve SQLite concurrent writes and restore state after persistence failure.
- Validate operation starts across batched record/message writes.

## [0.9.6](https://github.com/bigfish1913/pi-rust/compare/v0.9.5...v0.9.6) - 2026-10-27

### Fixed

- *(cli)* preserve the two-argument default system prompt API for existing library callers
- *(prompt)* list only the active tools and drop guidelines for absent ones ([#119](https://github.com/bigfish1913/pi-rust/pull/119))
- *(edit)* the display diff shows replaced lines old then new ([#121](https://github.com/bigfish1913/pi-rust/pull/121))

## [0.9.5](https://github.com/bigfish1913/pi-rust/compare/v0.9.4...v0.9.5) - 2026-10-27

### Fixed

- *(tui)* recognize Shift+Enter on Windows via native key state ([#108](https://github.com/bigfish1913/pi-rust/pull/108))

## [0.9.4](https://github.com/bigfish1913/pi-rust/compare/v0.9.3...v0.9.4) - 2026-10-26

### Fixed

- *(cli)* keep dev build output out of the TUI

## [0.9.3](https://github.com/bigfish1913/pi-rust/compare/v0.9.2...v0.9.3) - 2026-10-26

### Added

- *(tools)* add truncate_file to the FileSystem trait

### Fixed

- *(tui)* give every transcript block the same trailing spacer
- *(tui)* highlight fullscreen mouse selections and extract at the live width
- *(harness)* cut back a failed append before the session can retry

### Other

- keep the workspace buildable on the declared Rust 1.78 MSRV ([#112](https://github.com/bigfish1913/pi-rust/pull/112))

## [0.9.2](https://github.com/bigfish1913/pi-rust/compare/v0.9.1...v0.9.2) - 2026-10-25

### Fixed

- *(cli)* restore the session's model and footer usage on switch ([#104](https://github.com/bigfish1913/pi-rust/pull/104))

## [0.9.1](https://github.com/bigfish1913/pi-rust/compare/v0.9.0...v0.9.1) - 2026-10-25

### Fixed

- *(cli)* stop buffering IME commits as pastes on Windows ([#91](https://github.com/bigfish1913/pi-rust/pull/91))

## [0.9.0](https://github.com/bigfish1913/pi-rust/compare/v0.8.5...v0.9.0) - 2026-10-25

### Added

- *(runtime)* extract reusable runtime and optimize execution
- add optional binary serialization benchmark
- add zero-copy archived payloads

### Fixed

- *(release)* sync workspace lock and embedded docs
- *(cli)* keep temporary model switches out of settings.json

### Other

- Merge origin/main into feature-0.9.0
- Merge branch 'main' into fix/model-switch-not-persisted
- Merge remote-tracking branch 'origin/main' into feature-0.9.0
- reduce repeated context and message copies
- remove zero-copy from plugin sdk
- compare zero-copy with json and cbor

## [0.8.5](https://github.com/bigfish1913/pi-rust/compare/v0.8.4...v0.8.5) - 2026-10-24

### Fixed

- *(cli)* rebuild the banner and start a real session on /new ([#98](https://github.com/bigfish1913/pi-rust/pull/98))

## [0.8.4](https://github.com/bigfish1913/pi-rust/compare/v0.8.3...v0.8.4) - 2026-10-23

### Added

- apply lossless backpressure throughout provider streaming

### Fixed

- drain queued assistant events before terminal delivery

### Other

- *(ai)* clarify bounded stream API and async production
- *(extensions)* await applied metadata before saturation cancellation

## [0.8.3](https://github.com/bigfish1913/pi-rust/compare/v0.8.2...v0.8.3) - 2026-10-23

### Added

- add plugin lifecycle and reusable provider contexts

### Fixed

- *(cli)* invalidate host actions before plugin shutdown
- propagate provider HTTP client construction errors

### Other

- clarify lifecycle ownership and Rust API migration
- record provider context reuse and shutdown checks
- verify plugin workers join before DLL unload

## [0.8.2](https://github.com/bigfish1913/pi-rust/compare/v0.8.1...v0.8.2) - 2026-10-23

### Added

- *(ai)* add stream idle timeout and model compat diagnostics

### Fixed

- *(cli)* preserve session roots and safely fork and import logs
- *(providers)* close cancellation and compat diagnostic gaps
- *(providers)* preserve manifest diagnostics and thinking constraints
- *(ai)* dispatch CR-delimited SSE frames without waiting for bytes
- *(ai)* preserve initial SSE events after a UTF-8 BOM
- *(ai)* merge and remove HTTP headers case insensitively
- *(ai)* classify retries and honor SSE control boundaries
- *(extensions)* saturate oversized stream idle durations

### Other

- consolidate provider transport and session fixes
- *(cli)* isolate and restore session directory overrides

## [0.8.1](https://github.com/bigfish1913/pi-rust/compare/v0.8.0...v0.8.1) - 2026-10-23

### Added

- *(extensions)* drive wake-capable tools without idle blocking workers

### Fixed

- *(tui)* preserve working caret and isolate theme cache revisions
- *(rpc)* release extension session lock before awaiting RPC
- keep wake token allocation compatible with stable Rust

### Other

- Merge remote-tracking branch 'origin/main' into codex/fix-pr74-cursor
- Merge branch 'main' into ui
- add RPI_CODING_AGENT_SESSION_DIR env config
- share transcript rows and slice the viewport in fullscreen
- measure idle tool workers and check Windows ownership
- *(harness)* serialize run budget environment tests

## [0.8.0](https://github.com/bigfish1913/pi-rust/compare/v0.7.1...v0.8.0) - 2026-10-22

### Fixed

- *(cli)* say why a response stopped when the provider hit max_tokens
- *(ai)* never stack the reasoning headroom on a caller-pinned maxTokens
- *(ai)* reserve reasoning headroom when a gateway shares the completion budget

### Other

- compact exactly reconstructible provider text deltas
- use the current stream API in compaction fixtures

## [0.7.1](https://github.com/bigfish1913/pi-rust/compare/v0.7.0...v0.7.1) - 2026-10-21

### Fixed

- *(cli)* reject web startup without its server extension ([#76](https://github.com/bigfish1913/pi-rust/pull/76))
- keep snapshot benchmark features buildable independently

### Other

- remove redundant provider snapshot conversions
- add offline provider pipeline allocation benchmark

## [0.7.0](https://github.com/bigfish1913/pi-rust/compare/v0.6.3...v0.7.0) - 2026-10-21

### Fixed

- *(rpc)* publish native workspace and ACP protocol support ([#71](https://github.com/bigfish1913/pi-rust/pull/71))

## [0.6.3](https://github.com/bigfish1913/pi-rust/compare/v0.6.2...v0.6.3) - 2026-10-20

### Other

- update README website links to rpi.bigfisharc.cn
- *(sdk)* consolidate provider bridge into optional SDK feature

## [0.6.2](https://github.com/bigfish1913/pi-rust/compare/v0.6.1...v0.6.2) - 2026-10-19

### Fixed

- *(tui)* restore iTerm2 queue editing and cross-provider model cycling

## [0.6.1](https://github.com/bigfish1913/pi-rust/compare/v0.6.0...v0.6.1) - 2026-10-19

### Fixed

- *(tui)* repaint the frame when the terminal is resized
- *(tui)* preserve legacy resize repaint behavior

## [0.6.0](https://github.com/bigfish1913/pi-rust/compare/v0.5.1...v0.6.0) - 2026-10-18

### Added

- *(btw)* port native side chat onto main

### Fixed

- *(ai)* preserve Anthropic tool result image payloads
- *(ai)* serialize content with a single type discriminator

### Other

- *(cli)* format side-question module declaration

## [0.5.1](https://github.com/bigfish1913/pi-rust/compare/v0.5.0...v0.5.1) - 2026-10-18

### Fixed

- *(cli)* parse linked changelog headings without losing patch versions
- *(cli)* gate PowerShell tool import to Windows
- avoid deprecated atomic updates while preserving MSRV
- *(tools)* remove unused Unix process extension import
- *(ci)* enforce warning-free builds on pull requests
- *(tui)* apply shared theme spacing to tool panels

### Other

- clarify Pi attribution and 0.5.0 extension capabilities [skip ci]

## [0.5.0](https://github.com/bigfish1913/pi-rust/compare/v0.4.1...v0.5.0) - 2026-10-17

### Added

- *(extensions)* add plugin-owned child agent lifecycle
- add provider login flows
- *(extensions)* add streaming and cancellable plugin providers

### Fixed

- *(interactive_tui)* avoid deadlock in provider login key handling
- *(cli)* switch session providers live and route models by API
- *(auth)* unify package credential resolution and provider isolation
- *(cli)* include provider packages in provider commands
- *(extensions)* correct streaming provider cancellation and compatibility
- harden provider callbacks and model identity

### Other

- *(changelog)* add 0.4.0 and 0.4.1 release notes
- merge feature-stream-porider into feature-0.4.2
- *(providers)* use the unified external provider package
- *(providers)* move DeepSeek policy out of core
- *(providers)* define provider integration and acceptance contract
- *(providers)* unify factories and isolate execution bindings
- *(extensions)* add DeepSeek streaming provider smoke example
- *(providers)* extract optional native provider packages

### Added

- Expose safe live execution metadata and plugin-owned child-agent lifecycle
  actions. Children use independent ephemeral sessions while inheriting the
  current model/provider binding, project tools and approval UI; credentials
  stay inside the host. Bound concurrency, output, turns, timeout and cleanup,
  and cancel children when their parent run/session or owning plugin ends.

- Stream custom plugin providers. A plugin registered with
  `register_streaming_provider` emits per-chunk envelopes on a host callback and
  the host maps them onto `AssistantMessageEvent`s as they arrive, so a plugin
  provider renders progressively instead of appearing all at once when the
  response is complete.
- Cancel a plugin provider mid-generation. The streaming request fn now also
  receives a `ProviderStreamControl` (plus a deferred completion signal reserved
  for a future host). Polling it is the plugin's choice — one registration and one
  signature cover both the fire-and-forget and the cooperative shape. A cancelled
  run stops forwarding content and ends on `Error { reason: Aborted }` immediately,
  and a cooperative plugin stops producing instead of streaming a generation
  nobody will read.

### Fixed

- Bind child-agent actions to live tool invocations rather than a cached root
  host: reject alias recursion and capability expansion, make expired/reverted
  reload handles safe, and retain blocking tool drivers' owned parameters,
  libraries and cleanup capacity until their final callback returns.
- Keep cancelled parent invocations authorized for child cleanup only; propagate
  invocation cancellation through queued starts and stream tool partials live.
  Break the harness/provider/plugin ownership cycle with weak runtime bindings.
  Back off pending tool polls, stop cancellation busy loops and reject late
  success. Track provider and approval drivers against child cleanup capacity,
  and retain provider/lifecycle DLLs inside the foreign callbacks themselves.
- Report an aborted plugin provider as `ErrorReason::Aborted` rather than
  `ErrorReason::Error`, so a user-cancelled run is no longer indistinguishable
  from a provider failure.

## [0.4.1] - 2026-10-17

### Changed

- Allow `rpi install` to upgrade an installed extension without `--force` and
  skip an already-installed version. Require `--force` only to reinstall the
  same version or downgrade, consistently across prebuilt, source and local
  installs; apply the same version policy to `rpi package update`.
- Synchronize the 0.4.0 installation channels and website release metadata.

### Fixed

- Rebuild terminal scrollback when wrapped row counts change, preventing stale
  rows from remaining after content updates or terminal resizing.
- Track the effective ANSI style state across wrapped lines instead of replaying
  the entire style history, preventing escape-sequence growth in long styled
  output while preserving colors and partial resets.

## [0.4.0] - 2026-10-16

### Added

- Configure per-model USD cost rates, long-context pricing tiers and thinking-level
  mappings in `models.json`; preserve those values when saving the configuration.
- Prepare unified workspace Release PRs automatically after main CI succeeds.
  Merging a Release PR publishes crates, five platform binaries, Homebrew/Scoop
  and a retry-safe Microsoft winget submission.

### Changed

- Add `Model.cost_configured` (`costConfigured` in JSON) to distinguish unknown
  prices from explicitly free models. `ModelDefinition` also adds `cost` and
  `thinking_level_map`. Rust callers using exhaustive struct literals must supply
  the new fields; `Model::new` provides its default. These API changes require the
  0.4.0 release; the native extension SDK ABI remains unchanged.
- Expose pricing availability in extension provider hooks and calculate request
  costs using the configured model rates, including long-context tiers.

### Fixed

- Wait for buffered file appends to complete before returning, so immediate
  reads and metadata queries always observe the appended bytes.
- Release extension dialog state locks before submit callbacks, preventing
  free-form answers and editor submissions from deadlocking the TUI.
- Restore enhanced keyboard input on compatible macOS terminals so Shift+Enter
  inserts a newline. Otty retains legacy input for CJK IME compatibility, with
  `RPI_KITTY_KEYBOARD` available as an explicit override.
- Wrap long Markdown code lines instead of clipping their tails with an ellipsis.
- Keep different versions in separate publish queues and synchronize embedded
  release notes before the Release PR becomes ready.

## [0.3.24] - 2026-10-15

### Added

- Install prebuilt native extensions through the versioned `rpi-extension.json`
  protocol. Resolve repositories from crates.io metadata or `--github`, use
  package-specific release tags, and verify archive size, SHA-256, SDK ABI and
  the exact native platform before installing.
- Update the CLI from verified GitHub release binaries using `rpi-release.json`.
  Keep Cargo compilation explicit with `--build`, and preserve the existing
  executable or installed extension when validation or replacement fails.
- Add a dedicated author sponsorship page with WeChat Pay, Alipay and Binance
  Pay QR codes, plus a separate sponsorship button in the website header.

### Changed

- Publish the CLI distribution manifest only after all five platform archives
  and their checksums are available. Retain repository provenance when updating
  extensions installed from a custom GitHub repository.
- Keep language selection across dynamically rendered documentation links,
  guide navigation, reloads and blog navigation. Remove untranslated UI text
  and keep README donation codes in a three-column Markdown table.

## [0.3.23] - 2026-10-13

### Changed

- Refresh the homepage, bilingual documentation navigation and extension catalog;
  show registry-backed release/download metadata and retain SDK access.
- Forbid unsafe Rust in the AI, agent, harness, tools and telemetry crates.
- Require explicit unsafe calls to raw SDK string readers and JSON decoding;
  document buffer validity and lifetime requirements without changing the C ABI.
- Require explicit unsafe calls to raw string/step deallocators and step-result
  adoption, including `take_step_result!`. Add a non-copyable `OwnedString` for
  safe Rust-side reads and automatic producer-side cleanup after ABI adoption.

### Fixed

- Own plugin registration contexts through scoped `Arc` handles instead of
  thread-local raw pointers, restoring enclosing registrations on nested calls
  and unwinding. Keep the native plugin ABI unchanged.
- Use the SDK's safe handle registry and shared string cleanup in `plugin-stub`;
  invalid, retired and cancelled tool handles no longer dereference raw state.
- Validate borrowed ABI input strings as UTF-8 before exposing them to callback
  bodies, replacing unchecked decoding while retaining pointer validity and
  lifetime requirements at the FFI boundary.
- Route the stub's discovery, prompt, provider and renderer callbacks through
  SDK string trampolines that reject missing output slots. Replace raw tool-state
  pointers in adapter tests with safe handles and reuse SDK string deallocation.
- Keep standard-library ABI string helpers available with the SDK's default
  JSON feature disabled, so string callbacks compile in minimal builds.
- Reclaim tool partial callbacks through the producing plugin's registered
  deallocator instead of the host allocator, including closed receiver paths.
  Adopt callback outputs, tool results and schema strings with scoped ownership
  so early returns and host-side unwinding reclaim each allocation once.
- Remove duplicate raw string deallocation, leaked prompt-marker contexts and
  raw callback state in extension/CLI tests, using SDK views and safe handles.
- Give tool execute callbacks an owned-parameter trampoline that reclaims host
  inputs on refusal or malformed call IDs. Remove redundant schema `Send`/`Sync`
  overrides and require explicit unsafe operations inside SDK unsafe functions.

## [0.3.22] - 2026-10-13

### Changed

- Align `send_message` with Pi's `triggerTurn` and `deliverAs` (`steer`,
  `followUp`, `nextTurn`) semantics. Default idle sends now append context;
  callers that need an immediate reply must pass `triggerTurn: true`.
  Running sends enqueue without reentering the agent. Accept Pi custom-message
  payloads and flat or nested delivery options without changing the plugin ABI.

### Fixed

- Durably queue context messages appended during a run until its tool batch has
  settled. Preserve accepted messages through cancellation, retries and session
  recovery without starting extra model turns or splitting tool calls/results.
- Repair legacy tool-result ordering within each assistant batch, preserving
  reused call IDs across turns and moving message payloads without deep copies.
- Preserve reserved entry IDs for steering, follow-up and next-turn messages,
  commit them before considering their queue items consumed, and keep next-turn
  messages pending across automatic run recovery.
- Reset retry message indices and compaction cuts only when the next attempt
  starts, preserving successful replies after compaction and avoiding duplicate
  tool results when retry waits are cancelled. Bind assistant frame streams to
  their reserved entry IDs before committing, so recovery does not replay
  replies already written at a context boundary or discard uncommitted progress.
- Link retry intents to their actual assistant entry reservations and inspect
  only the latest intent during recovery. Successful retries no longer start an
  extra turn after a crash, and a begun retry retains its own streamed progress.
- Settle queued messages of every accepted role under their reserved entry IDs,
  including stored assistant and tool-result messages, so delivered messages do
  not return to the queue after restarting.
- Rebuild all lane delivery queues from the target session on a session switch,
  keeping pending messages in their original session and clearing stale resume
  state. Existing lane handles and session clones now follow the shared backing
  when it changes. Wait for an active operation to finish before the TUI swaps
  sessions, and update plugin session identity only after a successful swap.
- Finish context callbacks before admitting a new operation and capture run
  outcomes and `RunEnd` from the same leaf snapshot. Listener writes remain
  durable without changing the completed run's result.
- Reserve run admission before automatic resume preparation, so a concurrent
  session switch cannot redirect the resumed run. Preserve pending resumes on
  rejected admission and persist their chain and attempt count explicitly.
- Serialize external usage writes with operation completion and session swaps;
  associate them only with an open operation on the same lane, keeping start
  and end callback records valid.
- Keep all delivery queue records independent of run admission and completion,
  including `steer`, `follow_up` and `next_run`. Allow cancellation of pending
  queues from older sessions after their accepting run ends, while preserving
  enqueue matching and validation of other operation records.

## [0.3.21] - 2026-10-11

### Added

- Show successfully loaded system prompt and project instruction files in the
  TUI startup Context section. `/context` and the startup listing refresh after
  a successful `/reload`, including project/global precedence and removed files.

### Fixed

- Batch confirmed Windows console text pastes instead of dispatching each
  character separately, substantially reducing the delay before large pasted
  text folds. Preserve Unicode, tabs, line endings, shortcuts and trailing Enter.
- Track rpi release notes independently from imported Pi settings so updates
  appear once on the next new interactive session. `/changelog` defaults to the
  latest release; `/changelog all` shows the complete history.
- Read scroll position and follow-output state from the primary scroll view,
  and ignore the jump-to-latest click while another UI owns terminal rendering.
- Keep context-file opt-outs consistent between startup and reload.

## [0.3.20] - 2026-10-11

- Add plugin-owned text components: register layout once, update content separately,
  configure positions, show/hide, and automatically clean up on unload or reload.
- Keep legacy footer status text and remove the experimental panel payload.
- Improve footer/sidebar rendering, scroll navigation, Unicode line updates,
  and terminal stdio lifecycle support for plugins.


## [0.3.19] - 2026-10-10

### Fixed

- Restore IME text input in iTerm2 after the 0.3.18 keyboard regression by
  using Pi's Kitty flags 7 instead of all-key reporting (flags 15). Preserve
  UTF-8 text commits, modified keys and functional-key repeat/release events.

## [0.3.18] - 2026-10-10

### Added

- New interactive sessions show bundled release notes once after an upgrade.
  Fresh installs record a baseline without a notice, resumed sessions defer it,
  and `collapseChangelog` offers a compact notice linking to `/changelog`.

### Fixed

- macOS TUI keyboard setup stays on the active screen buffer and requests
  printable-key release events where supported. Local Terminal.app recognizes
  Shift+Enter through native modifier state; suspension restores terminal modes.
- Plugin shortcuts no longer intercept modal dialog, selector or search input.
  Passive panels render at their allocated width, stay clear of the editor/footer,
  clamp offsets and avoid each other while space permits.
- `/changelog` displays the real packaged changelog instead of a static summary,
  including offline installs and Keep a Changelog version headings.

## [0.3.17] - 2026-10-08

### Added

- TUI plugin panels support positioned transcript, sidebar, and popup layouts,
  including pet companion and live run statistics panels. Task lists and plans
  retain their current state across tool updates and restored sessions.
- TUI prompts accept multiple pasted image paths, clipboard screenshots, and
  copied image files on Windows and macOS. Image-only prompts, steering, and
  follow-up messages retain their attachments; dequeuing restores them to the
  draft, and clearing the draft removes pending images.
- User and tool images are normalized for model input with Pi's default
  2000-pixel dimensions and 4.5 MiB base64 limit, including orientation handling,
  format conversion, and explicit notices when an image cannot be processed.

### Fixed

- Empty assistant starts and whitespace-only streaming updates no longer insert
  a blank row before the reply. Message spacing appears only with visible content.
- The footer displays the current thinking level when starting or restoring a
  session. Tool output hints consistently identify Ctrl+O for expansion and
  Ctrl+T for showing or hiding thinking blocks.
- Image visibility settings now apply to previews and restored history. Terminal
  image detection includes Ghostty, WezTerm, and Warp; Kitty rendering preserves
  aspect ratio, reserves layout rows, and removes obsolete placements.
- Supporting Unix terminals enable enhanced keyboard reporting so Shift+Enter
  can be distinguished from Enter and inserts a newline rather than submitting.
- The self-update tests resolve fixture paths consistently with production
  validation, avoiding macOS ancestor-link path mismatches and exercising the
  stale staging cleanup branch.
- Documentation topic descriptions reference the unified C ABI rather than
  obsolete ABI v2/v3 entrypoints.

## [0.3.16] - 2026-10-06

## [0.3.15] - 2026-10-04

## [0.3.13] - 2026-10-02

### Added

- Recovering a session that was interrupted mid-tool-call no longer poisons the
  provider context. When an assistant `toolCall` has no matching `toolResult` —
  the crash landed between the two durable writes — OpenAI-compatible endpoints
  rejected the whole request with `No tool output found for function call …`.
  The harness now repairs the in-memory context before conversion: it inserts a
  synthetic error result ("Tool call was interrupted before its result was
  persisted. The tool was not re-executed during session recovery.") directly
  after the call, in call order for parallel calls, and logs how many it
  repaired. The durable session is untouched, no interrupted tool is re-run,
  and normal history round-trips unchanged.

### Fixed

- Plugin `message_update` events now carry native Pi's `assistantMessageEvent`
  delta envelope (`text_delta`, `thinking_delta`, `toolcall_delta`, …) instead
  of a growing `AgentMessage` snapshot. A streaming consumer reads each delta's
  real `delta` field once, so it no longer has to diff successive cumulative
  snapshots and stream size stays linear — matching the JSON/RPC contract the
  other Pi runtimes emit.
- `rpi update` no longer stages a brand new build of the version that is
  already running. It now asks crates.io first and reports
  `rpi is up to date (0.3.13 is the latest release)` instead of compiling and
  scheduling a replacement for the same version — the behaviour that made a
  working update look broken. `--force` keeps an explicit reinstall available,
  and an unverified answer (cached, or a registry that could not be reached) is
  labelled as such rather than asserted. A `pending-*` directory that already
  holds a validated binary counts as "about to happen", so re-running
  `rpi update` while an update is queued does not start a second compile.
- `rpi update` prunes the debris previous interrupted runs left in
  `~/.rpi/agent/self-update`. A Windows update persists its `pending-*` staging
  directory so the PowerShell helper can install from it after the parent exits;
  when the helper died first — a reboot at the wrong moment, a machine-wide
  `taskkill` — the directory stayed forever, because nothing in the update path
  removed it and its still-`preparing` status file meant the next start never
  looked at it either. Staging directories no live status file claims are now
  reclaimed, along with the `preparing`/`waiting` records that named them.
- The status file `rpi update` prints is readable by the time you go looking for
  it. A `succeeded` record used to be deleted by the very next rpi start, so the
  path in that message was dead on arrival and a finished update read as one
  that never ran. Completed records now survive as the receipt for the installed
  version (aging out after a day), and the `staged` line names the version and
  says what to do: `rpi 0.3.13 staged; quit this rpi and it will be applied.`

## [0.3.12] - 2026-10-01

### Added

- Plugins can now replace the system prompt at runtime via
  `register_before_agent_start` — the mechanism pi's extensions use as a
  `before_agent_start` handler returning `systemPrompt`. rpi already broadcast
  `BeforeAgentStart` so a plugin could watch a turn start, but the event handler
  ABI is fire-and-forget (`i32` only), so it could not influence one. The new
  slot carries an `out` pointer instead, which is the shape
  `register_resources_discover` already used for the same reason.

  Semantics match pi: the return value **replaces** the prompt (a plugin that
  wants to append reads the envelope's `systemPrompt` and concatenates),
  handlers chain in registration order, a handler that errors or panics is
  skipped without aborting the fan-out, and the result applies to that run only.
  It is computed once per user prompt, so a tool loop's several provider calls
  see the same prompt rather than re-running handlers mid-run.

  The slot is appended at the end of the plugin API struct, which keeps every
  earlier field offset valid — existing plugins keep loading unchanged and a
  plugin checks for the slot before using it, so there is no ABI version bump.

## [0.3.11] - 2026-10-01

### Fixed

- A spawned shell no longer inherits rpi-langfuse's subagent-nesting channel
  (`LANGFUSE_PI_PARENT_TRACE_ID` / `_SPAN_ID` / `_SESSION_ID` / `_DEPTH`). The
  extension publishes those variables so a launcher can attach a nested `rpi` to
  the turn that spawned it, and decides it is a subagent by reading them back —
  but a bash/powershell child inherits the environment, so a plain `rpi` started
  from inside a shell adopted its ancestor's trace: it reused the parent trace
  id, hung its root off the parent's root span, and named it `Subagent Turn`
  instead of opening its own trace. Only `RPI_SESSION_ID` was on the
  host-private exclusion list, because the prefix-based check could not see
  these. They are now excluded; a launcher that sets them deliberately still
  gets through, which is the supported nesting path. Two tests pin both halves.
- The subagent channel's variable names moved into the plugin contract
  (`rpi_plugin_sdk::SUBAGENT_PARENT_*`, with `SUBAGENT_PARENT_ENV` as the
  family), and `rpi-tools` now consults that declaration instead of restating
  the strings. Restating them is what made the leak above possible in the first
  place: the exclusion and the extension each held a private copy of the names,
  across two repositories, so a rename on either side left the list matching a
  name nobody sets — no compile error, no failing test, the leak simply came
  back. A newly added family member is now excluded automatically, and
  `rpi-cli` pins the wire values so changing one is caught as the cross-repo
  change it is.

## [0.3.10] - 2026-10-01

### Added

- The plugin contract now owns the host identity: `rpi_plugin_sdk::HOST_NAME`
  (`"rpi"`) and `HOST_VERSION`, with `rpi_cli::APP_NAME` / `VERSION` defined as
  aliases of them so the two cannot drift. `BeforeAgentStart`'s payload carries
  `host: {name, version}`, giving an extension a way to label its own output (a
  trace name, a tag, `service.name`) from the host instead of hardcoding a brand
  that a rename silently invalidates. `rpi-langfuse` had been reporting the
  upstream TypeScript plugin's `"Pi Turn"` / `"pi"` for exactly that reason.
- The host now publishes the session id twice, so an extension that observes a
  run can name the session it is watching. `RPI_SESSION_ID` (constant:
  `rpi_plugin_sdk::SESSION_ID_ENV`) is written whenever a session becomes active,
  and `BeforeAgentStart`'s payload carries `sessionId` next to
  `prompt`/`imageCount`, recomputed each turn. Previously the only channel was
  `__rpi.sessionId` on a plugin **tool** call, so a tracing extension reported a
  placeholder session for a pure-chat run and for every turn before the first
  tool call. The payload field is the one that makes the first turn correct; the
  environment variable is a convenience for plugins that never see a tool call.

### Fixed

- Switching sessions no longer leaves plugins naming the previous one. The
  session id reaches a plugin on two channels — the `ToolCallContext` (read per
  plugin **tool** call) and `RPI_SESSION_ID` (read per turn by the provider
  hooks) — and only the startup build republished them. An in-process swap
  (`/import`, `/fork`, the `/session` selector, and the plugin
  `NewSession`/`Fork`/`SwitchSession` runtime actions) changed the session the
  harness serves without touching either, so a plugin keyed to the old id went
  on serving it. All of them now route through one funnel that republishes both.
- `RPI_SESSION_ID` no longer leaks into the shell that `bash`/`powershell`
  spawn. The shell inherited the whole process environment, so the variable
  reached anything the command ran — including another `rpi`, which had no way
  to tell its ancestor's session from its own. A small deny-list now keeps the
  host's own plumbing out of a spawned shell, while explicit per-call values
  still pass through.
- `rpi update` no longer refuses to run when the running `rpi` was launched
  through a link, junction, or `subst` drive. The path validation demanded that
  `canonicalize()` reproduce the launch path byte for byte, so any link *above*
  the executable — a `C:\tools\current\rpi.exe` junction, a `subst`, scoop's
  `current` directories — aborted the update with "current rpi executable
  resolves through a link or junction". The message named neither the path nor
  the link, and the check was the wrong shape: the leaf is what matters, so a
  linked *file* is still refused, now with both paths printed, while links
  *above* the executable resolve to the real install directory the update
  should rewrite.
- `rpi update` no longer tells you to add the staging directory to `PATH`.
  Cargo's install advice named `<agent>/self-update/pending-*/bin`, which the
  atomic replace consumes, so following it added a path that stopped existing a
  second later; the child `cargo install` now runs `--quiet` (errors still
  print).

### Changed

- `docs/README.md` is marked as a historical 0.1.13 release post,
  and its update-commands section now carries a correction. It still told
  readers to run `rpi pi-update` and `rpi self-update`, neither of which exists
  any more — the article only documented 0.1.13-era behavior and was never
  revisited when the Pi layer was removed and the alias was dropped.

## [0.3.9] - 2026-10-01

### Added

- `rpi update` performs a real self-update again. `a5506dd` had reduced it to a
  bare `cargo install` that ignored its arguments, so `rpi update --help`
  reinstalled the crate instead of printing help and `--offline` did nothing.
  Restored: argument validation, `--help`, the `cargo binstall`/`cargo install`
  path, direct binary replacement on Unix, the staged PowerShell replacement on
  Windows (a running `.exe` cannot replace itself), staged-binary version
  validation, rollback, and the status report shown on the next start.
- The startup update check covers the rpi release and installed Rust extensions
  again, with the 6-hour cache and per-item fallback that survived a registry
  outage.

### Changed

- The Pi compatibility layer is *not* restored: `rpi package`, `pi-package
  update`, `install-pi`, `uninstall-pi` and `--enable-pi-packages` stay removed,
  along with the npm/Git package stores and the Node extension host.

### Fixed

- Removed subcommands no longer fall through to the prompt path. `rpi package
  update` used to be sent to the model as the message "package update" and
  appeared to hang; it now fails immediately with a pointer to `rpi install` /
  `rpi uninstall` / `rpi update`. `rpi self-update` reports the same way and
  points at `rpi update`.

## [0.3.8] - 2026-09-30

## [0.3.6] - 2026-09-30

## [0.3.5] - 2026-09-29

## [0.3.4] - 2026-09-27

### Added

- **Long pastes fold into a `[paste #N …]` marker instead of flooding the
  editor.** A bracketed paste longer than 10 lines or 1000 characters collapses
  to a single `[paste #N +M lines]` / `[paste #N M chars]` token, matching native
  pi, so dropping a stack trace or a file into the prompt still leaves a readable
  draft. The editor keeps the full text beside the marker — submit sends the
  expansion — and the marker is atomic: one backspace or arrow press removes or
  steps over the whole thing, and deleting an earlier marker renumbers the later
  ones so `#N` stays gapless. Pastes under the threshold still insert verbatim,
  and Windows' Ctrl+V clipboard fallback takes the same path.
- **`Ctrl+X` copies the way native pi's `app.message.copy` does.** The editor
  selection is copied when there is one, otherwise the last assistant reply.
  Native pi binds the same action to the same key; this is where selection-copy
  lives now that `Ctrl+C` clears.

### Changed

- **Transcript repaints are now O(1) per unchanged message, closing the
  long-session lag against native pi.** rpi re-parsed every finished message's
  markdown/layout on every frame, so a 400-message session cost ~18 ms per
  frame (over the 16 ms budget) and got slower as it grew. It now mirrors
  native pi's caching layers: `Markdown` and `Text` memoize
  `(content, width) -> lines`, `UserMessageComponent` builds its `Box` +
  `Markdown` once instead of per frame, finalized `ToolExecutionComponent` /
  `BashExecutionComponent` panels cache their output, and the layout pass has a
  frame-local `renderCached` map so a component measured for an auto basis is
  not rendered again when painted. A live theme switch bumps a global revision
  that every cache keys on, so `apply_theme_preset` cannot leave stale colours.
  A 400-message transcript drops from ~18 ms to ~0.8 ms per frame (~22×) and
  stays under budget past 3000 messages; `crates/rpi-tui/examples/bench_transcript.rs`
  reproduces the measurement.
- **The terminal window/tab title now carries the startup mark.** An idle
  session sets `▃ ▅ ▂ rpi · 🦀` instead of `🦀π rpi`, keeping the activity
  suffix it had before (`⟳` in flight, `↻` retrying), so the tab and the startup
  lockup read as one brand: the title's three eighth-blocks are the lockup's
  3:5:2 bar silhouette flattened into a single row, and a test reads the bar
  heights back out of the lockup so the two cannot drift apart. Each bar is one
  cell wide — the first cut used `██` per bar and read as three fat slabs in a
  tab strip. The title is built by `pi-cli`'s `brand` module, so it drops the
  crab with `RPI_NO_EMOJI` exactly like the lockup — and it drops `π`, which is
  East-Asian *Ambiguous* and therefore measured differently under a CJK locale.
  Block elements share `█`'s width class, so the title is still single-width
  everywhere.
- **`Ctrl+C` clears the editor instead of aborting the run, matching native pi's
  `handleCtrlC`.** One press clears the draft — text *and* selection — and a
  second press within 500ms quits. Aborting stays on `Esc` (`app.interrupt`), so
  a stray `Ctrl+C` while a turn is streaming can no longer kill it, which is easy
  to do while a half-typed steering message sits in the editor. Auto-repeat is
  ignored, so holding the key cannot turn one tap into the quit; the quit still
  forces the process out when the async loop is parked inside a run. The footer,
  `/help`, and the remote-shell footer now read `Ctrl+C: Clear/Exit` and
  `Esc: Abort` instead of `Ctrl+C: Abort/Exit`.
- **`Ctrl+D` and the model-cycle keys now match native pi too.** `Ctrl+D` no
  longer aborts a run: with text in the editor it deletes the character forward
  (`tui.editor.deleteCharForward`), and on an empty editor it quits
  (`app.exit`). Aborting is now `Esc` and nothing else. Model cycling moved from
  `Ctrl+M` to native's `Ctrl+P`, joined by a new `Shift+Ctrl+P` (`Alt+P` on
  Windows) for the previous model — every hint, selector description, and doc
  comment that named the old key was updated with it.
- **The last native-only keybindings are wired up.** `Ctrl+Z` suspends to the
  background on Unix (`app.suspend` — native binds no key on Windows, so neither
  does rpi): it stops the renderer, hands the terminal back to the shell, and
  re-enters the alternate screen when the process is resumed with `fg`. On
  Windows, `Ctrl+Q` joins `Alt+Enter` for follow-ups and `Alt+V` joins `Ctrl+V`
  for image paste — native's Windows keys, added alongside rpi's rather than
  replacing them, because the Windows clipboard-*text* fallback still rides
  `Ctrl+V`. The registry now matches native's defaults for every app-level
  action rpi implements.

### Fixed

- **Cancelling a run hands the messages you queued while it was in flight back
  to the editor.** Esc / Ctrl+C / Ctrl+D aborted the lane, which drains its
  steering and follow-up queues — and the helper meant to pull them into the
  editor first existed but was never called, so anything staged during a run
  vanished on cancel. The three cancel paths now drain the queue into the draft
  (queued text first, then whatever you had typed, caret at the end of the last
  line) *before* the abort, mirroring native pi's
  `restoreQueuedMessagesToEditor({ abort: true })`. Dequeuing with Alt+Q / Alt+Up
  reports `Restored N queued message(s) to editor` (or `No queued messages to
  restore`) like native `handleDequeue`, and the restored draft's caret lands on
  the last line at a byte offset — the old `(0, char_count)` call parked it at
  the end of the first line and split multi-byte characters.

## [0.3.3] - 2026-09-26

### Added

- **Plugin tools now receive the session they run in.** The host merges a
  reserved `__rpi` object — `{"cwd", "sessionId"}` — into every plugin tool
  call's arguments, in the tool adapter's `execute` rather than in the agent loop
  or `prepare_arguments`, so the field reaches the plugin and nothing else: not
  the model, not the `ToolExecutionStart`/`Update` events, not the session log.
  A model cannot forge it either — the host's value overrides one that matches.
  Built-in tools are unaffected; they already receive an
  `ExecutionToolContext`. This is what lets a plugin keep per-session state
  without asking the model to guess an identity. `docs/extensions/authoring.md`
  documents the contract, including the one failure mode worth naming: a
  `#[serde(deny_unknown_fields)]` parameter struct will reject the injected key.
- `cargo binstall rpi-cli` works: `[package.metadata.binstall]` points at the
  release assets, which are named after the binary (`rpi`) rather than the crate,
  so the URL template uses `{ bin }`.

### Changed

- A tool result can ask to be rendered as markdown by returning
  `details.markdown = true`. The TUI renders those bodies with its markdown
  component — real table borders, list markers, inline code — instead of printing
  the source literally; every other tool is untouched. Markdown rows are not
  re-wrapped (the panel already wraps) and drop the `│` gutter so table borders
  stay clean.

### Fixed

- **A session log written by two processes at once can be opened again.** Two
  writers diverged their sequence counters: a mutation landed with an
  already-consumed seq, a later writer's seqs jumped forward, and an entry
  chained to a leaf the local state never reached — at which point the v4 loader
  rejected the whole session, so it could not be opened at all. Loading now drops
  a stale-seq append, resyncs the counter across a divergent writer's forward gap
  (raw seqs untouched, so later appends stay consistent), and re-chains an entry
  onto the lane leaf; when it dropped anything the file is republished
  atomically.
- A session already open in another `rpi` process is refused with `session … is
  already open in another rpi process` instead of being written to by both. The
  host holds an exclusive OS advisory lock per session (`<path>.lock`) for its
  lifetime.
- `Could not open the saved session:` was printed twice when a session could not
  be opened.

## [0.3.2] - 2026-09-25

### Added

- Session pickers (`rpi -r` and `/session`) show readable rows instead of a wall
  of near-identical timestamped file names. Each row comes from a cheap
  head/tail summary of the session file — display name, first user prompt
  (single-lined and truncated to a label), and on-disk size — so saved sessions
  can be told apart without parsing logs that can run to tens of megabytes.
  Search matches the label, the short id and the file name, and the primary
  column is wide enough that a long label is not clipped.
- `--name` / `-n` records a session's display name durably, for a fresh session
  as well as a restored one; an empty value clears a name set earlier.

### Changed

- The interactive header now shows the three-bar brand mark from the site
  favicon, labelled with the crab in place of the language name, instead of the
  previous crab-and-π logo.
  The old mark mixed an emoji with a box-drawing glyph, whose east-asian width is
  ambiguous, so the header rendered inconsistently depending on the terminal.
  The mark follows the active theme's accent colour for its middle bar.
  `RPI_NO_EMOJI=1` renders the lockup without the crab (and without the
  separator that would otherwise dangle).
- The workspace is now `rustfmt`-clean. The sweep is listed in
  `.git-blame-ignore-revs` so `git blame` skips it.
- `rpi-ai`, `rpi-agent`, `rpi-tools`, `rpi-harness` and `rpi-cli` now carry the
  crates.io `artificial-intelligence` category, so they are reachable by
  browsing that category and not only by keyword search.

### Fixed

- A superseded runtime-context update could publish its `activeTools` list after
  a newer one had already been applied. Concurrent `set_runtime_context` calls
  run on separate threads, and the thread holding the older response could reach
  the recording step last, resurrecting a tool list the newer context had
  replaced. The result is now discarded unless its revision is still the newest,
  matching the rule Node already applies to the request itself.
- `-c` / `--continue` and `-r` / `/session` no longer offer sessions that hold
  only their header, the leftovers an abandoned launch leaves behind. Resuming
  one of them previously opened an empty transcript; `-c` now picks the newest
  session that has content, falling back to the newest overall only when every
  session is empty.

## [0.3.1] - 2026-09-25

No functional change to the agent, tools or CLI. This release repairs the build
on `main` and fixes how every crate presents itself on crates.io and docs.rs.

### Fixed

- **`rpi-plugin-sdk` did not compile on `main`.** Duplicate definitions added
  after 0.3.0 shipped — a `pub type PluginApi = PluginApiVt` alias beside the
  `PluginApi` struct, and a second `register_entrypoint_unified` — produced
  `E0428`, `E0119` and `E0609`, so `cargo test --workspace` failed immediately
  for anyone cloning the repository. The duplicates are removed and the build is
  green again. The published 0.3.0 artifacts were built before they landed and
  were never affected.

### Changed

- Every crate declares `homepage` and `documentation`, and ships its own
  `README.md` instead of the workspace README. Previously all nine crates.io and
  docs.rs pages showed the same generic document, and its relative links
  (`docs/architecture/overview.md`, `LICENSE`, `examples/plugin-stub`) resolved to 404 on
  those sites.
- docs.rs now builds with `all-features` for every crate except `rpi-cli`, whose
  clipboard feature needs platform libraries that are not present there.

### Docs

- Added `docs/performance/benchmark-vs-pi.md` and `scripts/bench-vs-pi.mjs` — a reproducible,
  same-machine comparison against native Pi over the same JSONL command channel,
  with isolated config directories and both tools offline. rpi is 9.7× faster to
  start, 10.7× faster to cold start, 7.6× smaller in memory and ~18× smaller
  installed. **Corrected after release:** an earlier run set only pi's `PI_*`
  isolation variables, so rpi read the real `~/.rpi/agent` and loaded the
  machine's global plugins (reported 103.6 ms / 1.7×); a second bug started the
  cold-start stopwatch after `spawn()`. With both fixed, rpi's `--version` (17.5 ms) and
  a cold start (17.7 ms) are effectively equal — the cost is process creation.
- `crates/rpi-cli/embedded-docs/` — the documentation snapshot compiled into the
  `rpi` binary for the `docs` tool — is refreshed and now guarded: CI runs
  `scripts/sync-embedded-docs.sh` and fails if the snapshot drifts from the
  repository documents. It had fallen behind by several releases.

## [0.3.0] - 2026-09-25

### Changed

- **Breaking:** the plugin ABI is unified on a single `rpi_plugin_register`
  entrypoint (`RPI_PLUGIN_ABI_VERSION_UNIFIED`), with the version carried inside
  the `PluginApi` struct instead of the symbol name. `rpi-extensions` still
  resolves `rpi_plugin_register_v3` and then `rpi_plugin_register_v2`, so plugins
  built against an older ABI keep loading; extension authors should rebuild
  against the current SDK when convenient.
- Workspace dependencies (`rpi-*` internal crates) bumped to `0.2.0`, then to
  `0.3.0` for the whole family.

### Added

- A model-visible system prompt section that makes the agent aware of reasoning
  replay, developed alongside `docs/debugging/llm-repetition-forensics.md`.

### Docs

- `docs/debugging/llm-repetition-forensics.md` updated with the latest evidence and the
  corrections to earlier conclusions in that document.

## [0.2.0] - 2026-09-25

### Changed

- **Breaking:** plugin ABI unified — see 0.3.0 for the entrypoint negotiation
  order.

## [0.1.28] - 2026-09-25

### Added

- **Crash recovery, end to end.** Frame-level progress (`LaneRecord::AssistantFrame`),
  commit-on-settle for assistant messages carrying tool calls, startup
  continuation from three entry points (before the first request, during tool
  execution, while waiting to retry), and retry-intent records so a crash during
  backoff retries rather than salvaging a failed attempt as history.
- **Durable queues.** Steering, follow-up and `nextRun` messages are recorded on
  enqueue and rebuilt on start, so a message typed while the agent works survives
  a restart.
- **Deferred (long-poll) provider responses.** New `DeferredProvider` capability
  and a defaulted `Provider::deferred()` in `rpi-ai`; `run_agent_loop_from_assistant`
  in `rpi-agent` executes parked tool calls exactly once; `resume_deferred` in
  `rpi-harness` polls, records, and re-suspends as needed.
- **Settings parity.** `retry`, `compaction`, `steeringMode` and `followUpMode`
  now come from `settings.json` instead of hardcoded defaults. `sessionDir`,
  `shellCommandPrefix` and `httpProxy` are wired, and native Pi's key names
  `enabledModels` / `httpIdleTimeoutMs` are accepted.

### Fixed

- A run that kept dying can no longer restart the same work forever: every
  continuation point is bounded by a resume budget.

### Docs

- `docs/debugging/llm-repetition-forensics.md` §11 documents the per-state comparison
  against native Pi's durable state machine.
- `docs/native-pi-missing-features.md` §7/§15 corrected against verified
  behaviour.

## [0.1.27] - 2026-09-24

### Added

- CBOR binary protocol (framing + codec) in the CLI.
- Image generation API (`openrouter-images`) in `rpi-ai`.
- Remote model catalog refresh (`pi.dev`).
- TUI: rich footer status bar with live git branch refresh; code-block syntax
  highlighting.
- CLI: file-watch auto reload (`fs-watch`); HTTP proxy and pooled idle timeout;
  startup timing / experimental switches, session cwd restore, structured
  resource diagnostics, and cache-waste statistics.
- Custom provider, user-agent, guidance and attribution support, plus
  `--changelog`.

### Fixed

- Anthropic request failures no longer write to stderr, which previously
  corrupted fullscreen TUI rendering.
- Extensions: a plugin panic is now isolated instead of taking down the host
  process.
- `rpi-ai`: corrected a `constrained.rs` doc example that referenced a
  non-existent `ToolDefinition`.

## [0.1.26] - 2026-09-23

### Fixed

- `clipboard`: gate the `arboard` dependency and clipboard functions on
  non-Android targets so `cargo install` works on Android/Termux.

## [0.1.25] - 2026-09-23

### Added

- Extension UI prompt events and fullscreen copy-on-select.
- Transcript search (`Ctrl+Shift+F`).
- Paste-content detection to fix multi-line pasting on Windows; bracketed paste
  mode is enabled explicitly.
- PowerShell tool registered on Windows, plus a `default_tools` setting.
- `rpi-agent`: updated agent construction API and restructured documentation.

### Fixed

- Bash tool throttled-flush behaviour.
- `@` symbol is preserved when accepting file autocomplete.
- `arboard` gated to non-Android targets.

## [0.1.24] - 2026-09-22

### Added

- **Remote mode.** `rpi --mode rpc` is a real JSONL command loop, `rpi --server`
  runs the agent headless over TCP and prints a token at startup, and
  `rpi --connect <host:port>` attaches a terminal client that holds no local
  provider, tools, extensions or session files. Token auth is connection-level
  (`-32001` on missing/invalid token); `--no-token` disables it, and
  `RPI_SERVER_TOKEN` is honoured. Shared wire types (`RemoteEvent` /
  `RemoteCommand` / `RemoteResponse`) live in one module so the two ends cannot
  drift.
- **Extension lifecycle events (P0–P5) with veto support** — a handler can
  refuse startup by returning `EVENT_HANDLER_ABORT` (new `BeforeTuiStart` hook,
  exit code `3`).
- **ABI v3** (`PluginApiVt3Ext.declare`) adds priority and platform declarations.
- Optional JSONL event journal (`RPI_EVENT_LOG=1`) with `rpi events tail` and
  `rpi events path`.

### Docs

- New `docs/remote/user-guide.md` and `docs/README.md`.

## [0.1.23] - 2026-09-21

### Added

- Headless RPC mode behind the `--server` flag.

### Fixed

- `register_entrypoint` is now marked `unsafe fn`, making the host/plugin safety
  boundary explicit (breaking change for extension authors).
- Restored the render scheduler and reload error reporting.

## [0.1.22] - 2026-09-19

### Added

- `rpi dev-local` degrades gracefully to skills-only mode when no Cargo `cdylib`
  is present.

### Fixed

- Restore the editor state on abort so error text can no longer leak into the
  input box.
- Render Markdown content in the `ask_user` dialog.
- Correct error handling for local development mode without a Cargo `cdylib`.

## [0.1.21] - 2026-09-19

### Changed

- CI: removed a redundant `release.yml` that conflicted with the `ci.yml`
  release job; the crates.io publish now runs on pushes to `main` and supports
  trusted publishing.

### Docs

- Added repository status badges and improved repository discoverability.

## [0.1.20] - 2026-09-19

### Fixed

- Preserve steering messages in the active transcript when a blocking tool is
  cancelled, so queued input is consumed during tool-batch cleanup instead of
  waiting indefinitely for a future run.
- Wait for Windows process-tree termination and stop stdout/stderr readers on
  cancellation, preventing listener commands from hanging the TUI.

## [0.1.19] - 2026-09-16

### Fixed

- Soft-wrap long editor drafts inside the bordered input area.
- Preserve row/column after multiline file or command autocomplete replaces text.
- Clear stale autocomplete suggestions when selectors close.
- Keep regular-mode 405, authentication and network diagnostics visible.
- Sanitize provider error bodies so terminal control sequences and oversized
  responses cannot corrupt the layout.

## [0.1.18] - 2026-09-15

### Fixed

- Redraw only the actual changed line range, so status updates no longer rewrite
  the input editor and footer.
- Avoid all terminal output for unchanged frames, stopping background render
  ticks from pulling the native viewport back to the bottom.
- Track terminal height and cursor position precisely across renders and resizes.
- Clear removed tail rows without adding artificial scrollback lines.

## [0.1.17] - 2026-09-15

### Changed

- macOS default interactive sessions use regular/main-screen mode so the
  terminal owns both text selection and scrollback; Windows and Linux keep
  fullscreen as the default. `--tui-mode regular|fullscreen` overrides on every
  platform.

## [0.1.16] - 2026-09-15

### Fixed

- Reconcile the live assistant component from the harness's authoritative final
  message when prompt completion wins the race with asynchronous event delivery,
  preventing truncated response tails.
- Keep `/copy` output consistent with the rendered transcript.

## [0.1.15] - 2026-09-15

### Added

- `--timeout <seconds>` / `--timeout=<seconds>` CLI override with strict
  positive-integer validation.

### Changed

- Consistent 600-second default timeout for Anthropic Messages, OpenAI Chat
  Completions and OpenAI Responses, including streamed bodies.
- Mouse tracking is off by default on macOS so native text selection works;
  wheel behaviour on Windows and Linux is unchanged.

## [0.1.14] - 2026-09-14

### Added

- Retry progress in the TUI (retry number, budget, live backoff countdown) and
  structured `retry_scheduled` events in JSON mode.
- Retry transient provider failures up to 10 times per request by default;
  authentication, parameter, quota and billing failures stay terminal.

### Fixed

- Parse standard YAML block sequences in skill frontmatter, so lists such as
  `triggers:` are loaded instead of discarded.
- Preserve provider and abort diagnostics when a request ultimately fails.
- Removed the duplicate `Working...` footer status.

## [0.1.13] - 2026-09-14

### Added

- `rpi dev-local` and `rpi dev --local-only` load only resources from the current
  project and the active development extension, including project-local
  `.rpi/skills`, `.pi/skills`, configured project resource paths, and the
  extension's own resources. `/reload` matches the local-only scope.

## [0.1.12] - 2026-09-14

### Added

- Pi-compatible managed npm and Git package stores: structured `npmCommand`
  argv, npm aliases, trusted project settings, bounded subprocesses and
  conservative provenance checks.
- Independent rpi and package update checks at startup, with `PI_OFFLINE`
  support across startup and manual update commands.
- Resumable, clean-tree release workflow for the nine published crates.

### Changed

- `models.json` is read as an ordered provider/model catalog and native Pi's
  authenticated default-selection rules are applied without leaking first-party
  API keys into custom compatible providers.
- Git package updates are staged before atomically replacing the installed
  checkout; build or activation failures preserve the previous package.
- Self-updates are staged and validated before replacing the running executable
  on Unix and Windows.
- Install, update and uninstall of Rust-native extension artifacts share one
  mutation lock with rollback for ordinary filesystem and registry failures.

### Known limitation

- Rust-native multi-file artifact transactions recover from reported errors but
  keep no crash journal. Termination in the narrow interval between filesystem
  renames can require manual repair of the extension registry or artifact
  directory.

## [0.1.11] - 2026-09-13

### Added

- Image attachments and clipboard paste, native keybinding configuration, model
  and thinking controls, settings panels, external-editor drafts, session
  navigation and improved tool rendering in the TUI.
- CLI parity: `--list-models`, `--offline`, project trust controls, streamed JSON
  lifecycle events, and HTML/JSONL session export.

### Changed

- Default provider and model selection follows native Pi's precedence (saved
  defaults first, then known provider defaults) while preserving declaration
  order of custom providers.
- OpenAI Responses streaming handles sparse output indexes, reasoning and text
  phases, function/custom tool calls, terminal failures, incomplete responses,
  usage accounting, replay identifiers and malformed frames.
- Anthropic SSE decoding streams incrementally, preserves split UTF-8, surfaces
  transport failures and responds promptly to cancellation.
- Pi JavaScript/TypeScript packages are opt-in via `--enable-pi-packages`.

### Fixed

- The startup update checker no longer re-reads untrusted project package
  declarations; it uses the same trust-gated package set as the session.
- Rust 1.78 remains the supported minimum, verified with the 1.78 toolchain for
  both source APIs and the locked dependency graph.
- The published `rpi-cli` crate embeds its documentation so the crates.io tarball
  builds independently of the workspace.

## [0.1.10] - 2026-09-12

### Added

- `rpi dev` builds and watches a local Rust extension; `/reload` forces a fresh
  build without replacing a working extension when compilation fails.
- `rpi uninstall` and `rpi uninstall-pi`.
- Multiplexed Node extension transport allowing concurrent requests with
  out-of-order responses; tool and command cancellation propagates through
  `AbortSignal`.

### Fixed

- Provider context compatibility keeps Pi responses without an explicit `role`
  field usable in side threads.

> **Beta notice:** the Node/TypeScript extension bridge is an experimental
> compatibility feature. Use it for local evaluation only.

## [0.1.9] - 2026-09-10

### Added

- The welcome screen lists every active tool, including tools registered by
  runtime extensions, plus discovered skills. Empty skill sets render as
  `Skills (0) none`.

### Fixed

- Crate metadata now points at the correct GitHub repository.

## [0.1.8] - 2026-09-10

### Changed

- Session history, sharing and exports request entries in chronological order,
  matching the terminal transcript.
- The package catalog distinguishes core Rust libraries (`cargo add rpi-tools`)
  from loadable `cdylib` extensions (`rpi install rpi-todo`).

## [0.1.7] - 2026-09-09

### Added

- `rpi install <crate>` for crates.io `cdylib` extensions, with `--version`,
  `--path`, `--locked` and `--force`.
- Nine published extensions added to the website catalog: `rpi-mcp-adapter`,
  `rpi-web-access`, `rpi-subagents`, `rpi-background-tasks`, `rpi-lens`,
  `rpi-todo`, `rpi-codegraph`, `rpi-memory`, `rpi-token-usage`.

## [0.1.6] - 2026-09-09

### Added

- First public crate family: async streaming agent runtime; Anthropic Messages,
  OpenAI-compatible Chat Completions and faux providers; `read`, `write`, `edit`,
  `bash`, `grep`, `find`, `ls` tools; JSONL sessions, context compaction, hooks,
  queues and prompt templates; `rpi-plugin-sdk` with a stable ABI.

## [0.1.0] – [0.1.5]

Early crates.io publications while the workspace layout, provider layer and
agent loop were being established. See `git log` for details.

[Unreleased]: https://github.com/bigfish1913/pi-rust/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/bigfish1913/pi-rust/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/bigfish1913/pi-rust/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/bigfish1913/pi-rust/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/bigfish1913/pi-rust/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bigfish1913/pi-rust/compare/v0.1.28...v0.2.0
