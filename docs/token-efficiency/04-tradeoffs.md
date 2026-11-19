# 4. Tradeoffs, risks, and what I would do next

## 4.1 What this layer changes about the agent's behavior

| Change | User-visible consequence | Why it is worth it |
| --- | --- | --- |
| A repeated read returns a stub | The model may need one extra turn if it wanted the text verbatim | `{"fresh": true}` forces a resend; the stub names the turn it refers to |
| An identical search is answered from cache | The model sees a non-error "cache hit" block instead of results | `{"fresh": true}` forces a scan; the stub reports the match count |
| `edit` accepts `anchor`/`ops` | A second argument shape to document | The existing `edits[]` form still works and is what the anchor form expands into |
| Noisy output is compressed | Some lines are gone from the model's view | Only for `bash`/`grep`/`find`/`ls`; `read` is never touched; elisions are counted |
| Titles may use a cheaper model | Different phrasing on titles | Never changes the conversation's own model |

## 4.2 Correctness risks and how each is contained

**The cache serving stale content.** The only thing that promotes an entry to a
hit is a byte-level digest of the file as it is *now*. `mtime` is recorded but
does not decide. The `trust_mtime` opt-in is the single exception, it is off by
default, and its cost is stated in its own doc comment and pinned by a test
(`trust_mtime_trades_exactness_for_a_stat_when_enabled`, which asserts that a
same-length same-mtime rewrite *is* missed when the flag is on).

**An anchor applied to shifted lines.** Two independent checks: the tag must
match what this session delivered, and the digest must still equal the recorded
one at expansion time. The second check happens against bytes read immediately
before expansion, so there is no window between validation and use.

**Stubbing a body that compaction removed from context.** `invalidate_all()` is
called on compaction/resume, and hits are additionally bounded by
`dedupe_window_turns`. A hit never refreshes the window, so a body cannot be kept
alive by re-reading it (`a_hit_does_not_extend_the_window`).

**Hiding changes in a search.** A capped or error result is never recorded
(`a_capped_search_result_is_not_cached`), and a new file in any scanned directory
moves its mtime, which invalidates the entry
(`a_new_file_in_a_scanned_directory_invalidates_a_glob_search`).

**The approval gate being bypassed.** The layer runs *before* the gate and hands
it the expanded arguments. Only the cache short-circuit skips the gate, and that
path executes no tool.

## 4.3 Costs

- **CPU.** One hash per read. A 200 KB file hashes in well under a millisecond;
  the cache amortises it against a provider round trip that costs dollars.
- **Memory.** Bounded by `dedupe_window_turns`: entries outside the window are
  dropped on `begin_turn`. Search entries hold a path+mtime list per query; the
  `max_validate_files` cap keeps a pathological glob from pinning a large set.
- **Latency.** The hooks are synchronous over small files and run on the
  backend's worker after the tool completes, never on the GPUI thread.
- **Code.** ~2,400 lines including tests. No new dependencies: SHA-256 is local
  (`hash.rs`) precisely so this does not perturb the lock file.

## 4.4 Tradeoffs I chose deliberately

1. **Hash, not mtime, for the hit decision.** A stat is ~50× cheaper than an
   fs read. But "same length, same millisecond" is a real rewrite pattern (an
   editor's save-as-rename, a scripted `sed -i` on a fixed-width field), and a
   stale hit corrupts the model's whole view of a file. `trust_mtime` exists for
   people who have measured that their workload is safe.
2. **Dedupe is bounded in turns, not tokens.** A token-budget rule would be
   more precise ("is this body still in context?") but requires the context
   builder to expose what survived compaction. Turn distance is a proxy that can
   only err toward *not* deduping, which is the safe direction.
3. **Anchor ops are expanded into the existing `edit` shape, not applied
   directly.** That means the tool's own uniqueness check, fuzzy fallback and
   per-path mutation queue all still apply. Slower than patching bytes directly;
   it also cannot corrupt a file the tool would have refused.
4. **Minimization is content-agnostic.** oh-my-pi has per-command filters for
   dozens of programs. I kept four generic rules because a per-command table is a
   maintenance surface that rots, and the generic rules capture most of the noise
   (repeated compile lines, blank runs, vendored paths).
5. **Search validation uses the result's own file list.** It avoids re-running
   the search to find what was scanned, at the cost of trusting the result text
   to name its files. If it names none, nothing is cached — a miss, not a wrong
   hit.

## 4.5 Upstream hooks: the one `agent/` diff

`AGENTS.md` says not to restructure the vendored subtree. The diff is three
pieces, all additive:

1. `agent/crates/embedded/src/lib.rs` — `SessionOptions::after_tool_call` and
   `with_after_tool_call`, plus re-exports of `AfterToolCall` and
   `BeforeToolCallContext`. The loop already supported `after_tool_call`
   (`agent_loop.rs::finalize_executed_tool_call`) and the harness already
   threaded it into `AgentHarnessOptions`; embedded hosts simply had no wire.
2. `agent/crates/cli/src/session.rs` — `build_with_tool_hooks`, which forwards
   the new hook; `build_with_tool_hook` now delegates to it with `None` so every
   existing caller is untouched.
3. `agent/crates/embedded/src/lib.rs` — `SessionTitleRequest::spawn_routed`.

All three are additive, and the existing `spawn` / `build_with_tool_hook` entry
points keep their signatures and behavior.

## 4.6 Not done, and why

- **A fourth Anthropic cache breakpoint.** `agent/crates/ai/.../build_params.rs`
  spends three (last tool, system block, last user block) and Anthropic allows
  four. Spending the fourth on the boundary between durable context
  (`<project_context>`) and the conversation is the single highest-value change
  available, and it belongs in a separate PR against the provider crate with its
  own recorded-request tests. The prefix guard here will *report* the benefit once
  it lands.
- **Routed subagents.** Shob has no subagent spawn path to route —
  `src/subagent_chat.rs` reads child transcripts written by rpi. `Router` has the
  `Judge` role ready for one, but claiming it works today would be false.
- **Compaction changes.** `agent/crates/harness/src/compaction/` is upstream's
  design and already protects recent tokens (`keep_recent_tokens`). I did not
  touch it; this layer's contribution is clearing the dedupe ledger when it runs.
- **Live-provider benchmark.** Out of scope for this change (no credentials).
  The per-turn `TurnRow` metrics are the hook it would plug into.

## 4.7 Recommended next steps, in order

1. Land the fourth cache breakpoint in the Anthropic builder (separate PR).
2. Surface `MetricsSink` in the UI (a status line near the model picker:
   "cache hit 87% · 34k tokens saved").
3. Run one live session with `metrics: true` and compare `cache_hit_rate` against
   `SHOB_TOKEN_EFFICIENCY=0` on the same task. That is the number §3.3 currently
   projects.
4. If the anchor form shows real adoption, add `CUT`-based deletes to the prompt
   hint the model sees; today the syntax is documented in the module and offered
   only when a model produces it.
