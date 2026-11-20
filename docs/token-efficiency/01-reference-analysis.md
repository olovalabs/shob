# 1. What oh-my-pi and opencode do for token efficiency

> Part 1 of 4. The full set is in `docs/token-efficiency/`: `01-reference-analysis.md`
> (this file), `02-design.md`, `03-token-savings.md`, `04-tradeoffs.md`.

Both references were cloned at `HEAD` (2026-11-19) and read from source. File paths
below are relative to each repository root. Anything I could not confirm in source is
marked as unconfirmed; I did not infer behavior from README prose.

## 1.1 oh-my-pi

### Prompt-cache placement, provider-aware

oh-my-pi does not scatter `cache_control` markers through each provider module. It
computes a *policy* once, before the per-protocol body builder, then each protocol
lowers the hints onto its own wire format.

- `packages/ai/src/cache-policy.ts` — the policy pass. The default (`"auto"`) shape is
  `{ tools: true, system: true, messages: { tail: 1 } }`: one breakpoint on the last
  tool definition, breakpoints on the first *and* last distinct system part, and one
  advancing on the conversation tail. The header comment states the intent plainly —
  expose reusable tool / base-agent / project / session prefixes while advancing the
  tail so recent conversation prefixes stay reusable during long runs.
- The same file hard-codes the four-breakpoint budget (`BREAKPOINT_CAP = 4`) and, more
  importantly, *narrows the policy per model family*. `RESPECTS_INLINE_HINTS` skips the
  whole pass for protocols with implicit prefix caching (OpenAI family, Gemini). `QWEN`
  drops the tool and first-system markers because Qwen ignores tool breakpoints and
  caches tools with the system prompt. `gatewayPolicy()` returns `NONE` for gateway
  upstreams that cache without breakpoints, and Gemini is explicitly excluded from the
  conversation-tail marker because "a conversation-tail breakpoint writes a new cache
  every step and costs more than none". That last point is the important one: they treat
  an unnecessary breakpoint as a *cost*, not a no-op.
- `packages/ai/src/protocols/utils/cache.ts` — the breakpoint budget object
  (`newBreakpoints(4)`, `take()`, `warnDropped()`). Lowering spends from one shared
  budget across tools, system and messages, and logs a warning when it drops markers
  rather than silently truncating. `ANTHROPIC_BREAKPOINT_CAP` in
  `packages/ai/src/protocols/anthropic-messages.ts` enforces the same cap on the wire.
- `packages/ai/src/tool-updates.ts` — tool declarations are one ordered list with
  `Message.tools({add})` markers rather than a mutable top-level array. Tools a
  protocol can add in place are appended where they are, "so the leading declaration,
  and the prompt cache behind it, stays fixed". This is the structural answer to
  "never reorder the tool list": the list is append-only by construction.

Shob already has the same *primitive* — `agent/crates/ai/src/providers/anthropic/cache_control.rs`
plus the breakpoint emitters in `build_params.rs` — but only one breakpoint on the last
tool and one on the system block. There is no policy layer, no budget accounting and no
per-model narrowing.

### Hashed line anchors instead of retyped text (`hashline`)

`crates/edit/src/modes/hashline/` is a complete edit mode built on positional anchors:

- `format.rs` defines the encoding: a file section header `[path#TAG]` where `TAG` is a
  4-hex snapshot tag (`HL_FILE_HASH_LENGTH = 4`), and numbered rows `N:content`
  (`format_numbered_line`), plus hunk keywords `PUT start.==end:` / `CUT` / `REM` / `MV`
  and gap locators `<1`, `>$`, `<42`, `>42`.
- `prefixes.rs` normalizes line prefixes that were copied back out of read/search output
  (`123:` / `123|` / `>> 123:` / `+123:`), so a model that pastes numbered rows from a
  prior `read` still parses.
- `patcher.rs` (with `enforce_seen_lines` from `mod.rs`) refuses to patch lines the
  session never showed the model, and `recovery.rs`/`messages.rs` produce actionable
  errors when a tag is stale.

The token win is not that anchors are shorter than a diff; it is that the model never
emits the pre-image at all. A replacement is `PUT 120.==134:` plus new lines. Shob's
`edit` tool (`agent/crates/tools/src/tools/edit.rs`) requires `oldText`, i.e. the model
re-emits every replaced line.

### Summarized `read`, with a seen-row ledger

`packages/coding-agent/src/tools/read.ts` (~3000 lines) is not a file dumper:

- Line 1831ff selects a display mode (`hashLines: boolean; lineNumbers: boolean`) so a
  read can return rows already carrying the anchors `hashline` needs.
- Line 2011 calls `recordFullySeenRows(getEditStore(this.session), absolutePath, tag, outputText, cutLines)`:
  the read registers exactly which rows became visible, keyed by the snapshot tag. The
  edit engine later uses that ledger to enforce "you may only patch what you saw".
- `../utils/block-context` (`buildLineEntriesWithBlockContext`, `spansCoverEveryLine`)
  keeps syntactic-block context while still allowing elision of unrelated spans.

### Shell output minimization

`crates/shell/src/minimizer.rs` plus `minimizer/{config,detect,engine,filters,plan,pipeline}.rs`
compress shell stdout/stderr before it reaches the model. `MinimizerOutput` carries
`input_bytes`/`output_bytes`/`filter` labels, and — the part worth stealing — when a
filter rewrites output it also returns `original_text` so the caller can spill the full
capture and splice a URL into the text the model sees. Truncation is a *rendering*
decision; the data is still reachable.

### Compaction, and the parts that protect rather than shrink

`docs/compaction.md` is the map: `packages/agent/src/compaction/compaction.ts`
(summarization/handoff), `pruning.ts`, `shake.ts` ("mechanical content elision"),
`branch-summarization.ts`, `snapcompact` (history archived as dense bitmap images), and
`compaction-v2-streaming.ts`. Two details matter more than the strategy list:

- `packages/agent/src/compaction/pruning.ts` defines `PruneConfig` with
  `protectTokens`, `minimumSavings`, `protectedTools`, and `CacheLookbackConfig`
  (`cacheLookbackPositions`; the catalog axis `prompt-cache-lookback` resolves to 20 for
  Anthropic). Pruning is bounded by the provider's cache lookback: if rewriting a result
  would push the next request's cache lookup out of reach, it is left alone. Savings are
  only taken when they clear `minimumSavings` — i.e. do not churn the prefix for 200
  tokens.
- `pruning.ts` also implements *superseding*: `supersedeKey`/`supersedeComplete` prune
  a stale tool result first (even inside the protect window) when the model has since
  re-read the same target completely. The newest read stays; the older one is replaced
  by a marker.
- `packages/agent/src/compaction/message-cache.ts` memoizes token estimation and
  LLM-conversion per message *identity*, with a "settle gate" (never cache a streaming
  assistant whose `usage`/`stopReason` are provisional) and an explicit
  `invalidateMessageCache` contract for every in-place mutator (prune, shake,
  strip-images). Their own header calls out the failure mode: a memo that outlives a
  mutation silently undercounts. This is the correctness discipline a local cache needs.

### Per-role model routing

`packages/coding-agent/src/config/model-resolver.ts` defines roles and aliases:
`@smol` / `@slow` / `@default`, with `memory`, `tiny` and `widget` roles *aliasing*
`smol` when unconfigured (`ROLE_ALIASES`), and `findSmolModel()` walking a priority
chain (`roleDefaultModels().smol`). `DEFAULT_PREWALK_TARGET = "@smol"`. The subagent and
title-generation paths therefore do not need their own model configuration — they ask
for a role.

## 1.2 opencode

### Prompt composition is deliberately tiny and has a fixed tail

`packages/core/src/session/system-prompt.ts` renders
`packages/core/src/session/runner/prompt/system.txt` — 15 lines — and injects a single
`${OPENCODE_TOOL_GUIDANCE}` slot built from the *set of active tools
(`tools: string[]`). Tool-specific paragraphs are appended only when that tool is
active, and always in the same order. Cheaper than oh-my-pi's model-facing scaffolding,
and the invariant is the same: the prefix is a pure function of (prompt text, sorted
tool set), so it does not drift between turns.

### Cache breakpoints: policy + budget, with per-protocol down-lowering

- `packages/ai/src/cache-policy.ts` is the analogue of oh-my-pi's, with the same default
  (`AUTO = { tools: true, system: true, messages: { tail: 1 } }`), a `BREAKPOINT_CAP = 4`,
  the same skip list for implicit-cache protocols, and the same per-family narrowing
  (Qwen; gateway upstreams get `NONE`; Gemini gets only the last breakpoint). Manual
  `CacheHint`s are preserved and count against the budget; auto only fills the rest
  (`Budget.remaining`).
- `packages/ai/src/protocols/utils/cache.ts` — `newBreakpoints(cap)`, `take()`,
  `warnDropped()`. Dropping is logged, never silent.
- `packages/ai/src/protocols/anthropic-messages.ts` — `lowerTool` attaches
  `cache_control` only where the policy spent a breakpoint; `ANTHROPIC_BREAKPOINT_CAP = 4`
  is enforced at the wire ("beyond the cap the API returns a 400 — so the lowering layer
  counts emitted markers and silently drops any that exceed it").

### History: append-only entries, projection at read time

`packages/core/src/session/` separates *storage* from *what the model sees*:
`history.ts`, `context.ts`, `projector.ts`, `model-request.ts`, `runner/to-llm-message.ts`.
History is not rewritten in place to save tokens; a projector decides what the request
contains. That keeps the durable transcript honest and makes token reduction a pure
function of settings — which is what makes before/after measurement possible at all.

### Compaction

`packages/core/src/session/compaction.ts`:

- Settings are explicit about the safety margin: `buffer` keeps room free below the
  model's limit ("a share such as 20%, or tokens. Unset keeps 15%, and at least 16k,
  free"), `keep` is "tokens of recent conversation kept verbatim beside the summary",
  and `background` precomputes the summary as the context nears the threshold
  (`precompute`) "so automatic compaction can swap it in".
- Two different size signals: `estimatePrompt` reports `{ measured, estimated }` —
  `measured` is what the provider reported at the latest response, `estimated` is text
  added since. Threshold checks use both rather than trusting a character count.
- Triggers are enumerated (`auto | overflow | manual`), and `auto` is allowed to *skip*
  (`Outcome.status = "skipped"`) when the context fits — no summarization churn.

### Measurement

`packages/core/src/session/usage.ts` plus the `Usage` type on messages carry
input/output/cache-read/cache-write; `packages/core/src/util/token.ts` is a
`chars / 4` estimator used where the provider has not reported yet. Estimates are
labelled as estimates, not mixed into billing numbers.

## 1.3 What Shob already has, and the actual gaps

Already present in the vendored `agent/` subtree (so this work must not duplicate it):

| Capability | Where |
| --- | --- |
| Anthropic `cache_control` wire type + TTL resolution | `agent/crates/ai/src/providers/anthropic/cache_control.rs` |
| Breakpoint on last tool + system block + last user block | `agent/crates/ai/src/providers/anthropic/build_params.rs` (`convert_tools`, `convert_messages`) |
| OpenAI `prompt_cache_key` / `prompt_cache_retention` | `agent/crates/ai/src/providers/openai_responses.rs` |
| Cache-miss *waste* accounting and pricing | `agent/crates/harness/src/cache_stats.rs` |
| History compaction, cut points, token estimation | `agent/crates/harness/src/compaction/` |
| Tool output truncation with continuation hints | `agent/crates/tools/src/truncate.rs`, `output_window.rs`, `shell_compact.rs` |
| `before_tool_call` / `after_tool_call` hooks in the loop | `agent/crates/agent/src/agent_loop.rs`, exposed via `AgentHarnessOptions` |

Gaps this work closes:

1. **No local tool-result cache.** Nothing anywhere returns "unchanged since last
   read". A re-read after an edit re-sends the whole file. `grep` for `unchanged since`
   over `agent/` and `src/` returns nothing.
2. **No anchor-based edit path reachable by the model.** `edit` requires `oldText`
   re-emission; there is no `PUT n.==m:`-style mode.
3. **Prompt prefix is only *incidentally* stable.** `compose_system_prompt`
   (`agent/crates/harness/src/system_prompt.rs`) orders base → append → context →
   skills, which is correct, but nothing enforces it, nothing checks it per turn, and
   only two of the four available breakpoints are spent.
4. **No per-role model routing.** `SessionTitleRequest::spawn`
   (`agent/crates/embedded/src/lib.rs`) builds `SessionOptions` from the *same selector*
   as the main conversation, so "name this conversation" runs on the expensive model.
5. **No per-turn token/cache-hit telemetry surface** beyond the raw `Usage` events that
   `src/agent_bridge.rs` forwards and `cache_stats.rs` prices.
6. **No reproducible benchmark.**

The remaining documents design and implement exactly these six.
