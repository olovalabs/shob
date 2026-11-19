# 2. Design: `token_efficiency`

## 2.1 Constraints that shape the design

Read from this repository before designing:

1. **`agent/` is a vendored subtree.** `AGENTS.md` says do not restructure it casually.
   So the layer lives in Shob's own crate (`src/token_efficiency/`) and touches `agent/`
   only where a hook is *already* specified but not exported.
2. **`before_tool_call` and `after_tool_call` already exist in the loop.**
   `agent/crates/agent/src/agent_loop.rs` runs `before_tool_call` (`Prepared::Immediate`
   on block, `validated_args` replacement applied without re-validation) and
   `finalize_executed_tool_call` merges `AfterToolCallResult` field by field. Both are
   reachable from `AgentHarnessOptions`. `before_tool_call` is already threaded through
   `SessionOptions`; `after_tool_call` is wired into the harness but **not** exposed on
   `SessionOptions`. That one missing wire is the entire `agent/` diff.
3. **No new runtime dependencies.** The root crate has no hashing crate; `sha2` exists
   in the graph only transitively. A cache whose correctness rests on a hash should not
   depend on resolving an ambiguous `sha2` 0.10-vs-0.11 lock entry from a sandbox that
   cannot run `cargo`. The module ships a small, tested SHA-256 instead.
4. **Correctness beats savings.** Every rule below is written so that the failure mode is
   "we sent too much", never "we sent something false".

## 2.2 Where token spend actually leaks

| Leak | Default cost | Fix |
| --- | --- | --- |
| Re-reading a file the context already contains | full file body, again | read dedupe (§2.5) |
| Re-running the same `grep`/`find` after an unrelated turn | full match list | search cache (§2.6) |
| `edit` re-emitting every replaced line as `oldText` | O(changed lines) input per edit | anchor edits (§2.7) |
| Noisy shell output (`git status`, test logs) | whole buffer | minimization (§2.8) |
| Title generation on the expensive model | 1 turn per conversation | routing (§2.9) |
| Prefix drift silently invalidating the provider cache | full prompt at write price | prefix guard (§2.10) |

## 2.3 Architecture

```
                      ┌──────────────────────────────────────────┐
   provider request   │  rpi harness (agent/) — unchanged        │
   ─────────────────► │  system → tools → context → turns        │
                      │  cache_control on last tool / system /   │
                      │  last user block                         │
                      └──────────────────────────────────────────┘
                                        ▲
                                        │ AgentToolResult (post-hook)
                      ┌─────────────────┴────────────────────────┐
                      │ hooks.rs                                 │
                      │   before_tool_call: anchors expand,      │
                      │     search-cache short-circuit           │
                      │   after_tool_call: read dedupe,          │
                      │     minimization, byte accounting        │
                      └───────┬──────────────┬───────────────────┘
                              │              │
              ┌───────────────┴──┐   ┌───────┴────────┐
              │ cache.rs         │   │ anchors.rs     │
              │ read/search      │   │ snapshot       │
              │ results, digests │   │ ledger         │
              └───────┬──────────┘   └───────┬────────┘
                      │                      │
              ┌───────┴──────────────────────┴────────┐
              │ hash.rs (SHA-256) · metrics.rs ·      │
              │ prefix.rs · routing.rs · config.rs    │
              └───────────────────────────────────────┘
```

Single-writer state: all mutable state lives behind one `parking_lot::Mutex` in
`ToolCache`, held only for hash-map work — never across file I/O, never across an
`await`. Hashing and stat calls happen *before* taking the lock. This matters: the
hooks run on the backend's Tokio workers and Shob's rules forbid blocking there.

`TokenEfficiency` is an `Arc<…>` shared by both hook closures and by the metrics sink, so
one instance per conversation. Dropping it drops the ledgers.

## 2.4 Cache keys

Every entry is keyed by a *request identity* and validated by a *content identity*.
Those are deliberately separate: the request identity answers "have I seen this
question", the content identity answers "is my answer still true".

### Read

```
request key  = (canonical_path, offset, limit, raw_read_requested?)
content stamp = (sha256(file_bytes), byte_len, line_count)
```

`canonical_path` is the symlink-resolved path, so `./a.rs`, `a.rs` and `src/../a.rs`
share one entry.

### Search (`grep` / `find` / glob)

```
request key  = (tool_name, pattern, search_root, glob, ignore_case, literal_mode, limit)
content stamp = (sha256(each result line, in order), match_count, scanned_file_count,
                 newest_scanned_mtime_ms)
```

`newest_scanned_mtime_ms` is what makes validation cheap: on a hit we re-walk the
recorded *scanned file set* and compare only mtimes, not contents.

### Anchor snapshot

```
ledger key   = canonical_path
ledger value = (snapshot_tag, sha256(file_bytes), line_count,
                delivered_turn, delivered_at_ms)
```

## 2.5 Read dedupe — the one rule that keeps it honest

On `after_tool_call` for `read`:

1. Re-read the file and hash it. **The hash comes from bytes on disk, never from the
   cache.** `mtime` + `size` are recorded but are *not* sufficient to declare a hit:
   two same-length writes inside one millisecond is a real, if rare, event, and the
   repo stores `mtime_ms` at millisecond resolution (`FileInfo::mtime_ms`). A
   `trust_mtime` fast path exists and defaults to **off**; enabling it trades a hash for
   a stat and is documented as such.
2. If the stamp matches the previous delivery **and** the previous delivery is still in
   context (see §2.11), replace the result content with a stub:

   ```
   [unchanged since turn 7: src/cache.rs — 412 lines, 14.2 KB, sha 3f9a1c2e.
    Content is identical to that read; re-issue read with {"fresh": true} to force a resend.]
   ```

   and set `details.cache = "hit"`.

The stub is the entire saving. It is safe exactly while the earlier body is still in the
model's context, which is why §2.11 exists and why the window is bounded.

3. Otherwise store the new stamp and return the real content unchanged.

## 2.6 Search cache

Same shape, validated by mtime of the scanned set. Two extra rules:

- **Bound the validation set.** If the previous scan touched more than
  `max_validate_files` (default 2000), the entry is not reused: validating it would cost
  more I/O than the search. Correctness is unaffected — it is a miss, not a wrong hit.
- **Never cache an error result** or a result that hit the output cap. A truncated match
  list is a different answer, and reporting "unchanged" over a truncated list would
  hide new matches.

Search dedupe `before_tool_call` short-circuits by *blocking* the call with a
non-error-shaped stub. Blocking produces an error result in the loop, so the stub text
is explicit that this is a cache short-circuit, not a failure:

```
[search cache hit: identical request and no matching file changed since turn 5.
 14 matches, first at src/a.rs:22. Re-run with {"fresh": true} to force a scan.]
```

## 2.7 Anchor edits

The ledger records what `read` actually delivered. The model may then address lines
instead of retyping them:

```
edit {"path": "src/cache.rs", "anchor": "src/cache.rs#3f9a",
      "ops": [{"put": "120.==134", "text": "…new lines…"}]}
```

`before_tool_call` translates this into the canonical `edits: [{oldText,newText}]` form
that `create_edit_tool` already validates, by:

1. resolving the ledger entry for `path` and checking `anchor` equals its current
   `snapshot_tag` — a mismatch is a **stale anchor**, reported with the tag to re-read,
   never applied;
2. re-reading the file and confirming `sha256 == ledger.digest`, so the line numbers
   still mean what they meant when the model saw them;
3. slicing lines `start..=end` and emitting `oldText` = those lines joined,
   `newText` = the supplied replacement.

Because the expansion happens locally against bytes just read, an anchor edit can never
apply to shifted content. The model saves the pre-image; correctness costs one local
read.

`PUT`, `CUT` (delete), and `<n` / `>n` / `<$` insertion cursors are supported. Ops are
applied bottom-up so earlier ops do not shift later line numbers.

## 2.8 Output minimization

Applied in `after_tool_call` to `bash`/`grep`/`find`/`ls` results that exceed
`minimize_over_bytes` (default 4 KiB): collapse runs of blank lines, drop duplicated
consecutive lines with a `… ×N` marker, elide lockfile/vendored paths, and cap the
result at `minimize_max_bytes` with an explicit elision count. Never touches `read`
(whose truncation semantics the model depends on) and never touches image content.

Every minimization records `bytes_in`/`bytes_out`, so §2.12 can report it instead of
claiming it.

## 2.9 Model routing

```rust
enum Role { Primary, Smol, Judge }
```

`routing.rs` holds a role → `provider/model` table from config. `route()` falls back to
the primary selector when the role is unconfigured, so routing is never a hard failure.
The one consumer wired today is `SessionTitleRequest::spawn`, which currently reuses the
conversation's selector (`agent/crates/embedded/src/lib.rs`). Shob has no
subagent-spawning path to route — `src/subagent_chat.rs` *reads* child transcripts — so
this document does not claim that it does.

## 2.10 Prefix guard

Not a cache; a tripwire. `prefix.rs` computes a digest over
`(system_prompt, sorted tool names)` — the part of the request the provider caches —
and records it per turn. If it changes mid-conversation without a tool-set change or a
resource reload, the metrics row is tagged `prefix_drift`, which is exactly the
condition that turns every `cache_write` into money that buys nothing. It also asserts
the ordering invariant the repo already satisfies
(`compose_system_prompt`: base → append → context → skills) with a unit test so a future
edit cannot quietly move the volatile section earlier.

What the guard cannot do is add the 4th provider breakpoint: that lives in
`agent/crates/ai/src/providers/anthropic/build_params.rs`, which already places
breakpoints on the last tool, the system block and the last user block. Extending it is
recommended in the report, not done here, because it is upstream code with its own test
suite.

## 2.11 Invalidation rules (the whole contract)

| Trigger | Action | Why |
| --- | --- | --- |
| File bytes changed (hash differs) | entry replaced, real content returned | the only rule that matters |
| `mtime` changed but hash equal | stamp kept, entry stays valid | touch/checkout noise must not cost tokens |
| File deleted | entry evicted | a hit would describe a file that is gone |
| Any `write`/`edit` to path P | P's read entries evicted, P's ledger digest cleared | post-edit reads must show new content |
| Compaction / history rewrite | **all** dedupe ledgers cleared | the earlier body is no longer in context; a stub would lie |
| Turn distance > `dedupe_window_turns` (default 8) | entries not reused for dedupe | bounds the "is it still in context" assumption |
| Session resume | cache starts empty | never trust a ledger across processes |
| Search validation set > `max_validate_files` | miss | cheaper to re-search than to validate |

The compaction rule is the one a naive implementation misses: dropping a body from
context while keeping a "see above" stub in a *later* message produces a prompt that
references content the model cannot see. `ToolCache::invalidate_all()` is called from the
host when a compaction event arrives, and it is tested (see `cache.rs`'s
`invalidate_all_clears_every_ledger` and `hooks.rs`'s `compaction_invalidates_every_stub`).

## 2.12 Measurement

`metrics.rs` keeps one row per provider turn:

```json
{"turn":7,"input":1204,"output":331,"cache_read":41002,"cache_write":0,
 "cache_hit_rate":0.971,"bytes_saved":8122,"tool_calls":3,"prefix_drift":false}
```

`bytes_saved` is the per-turn figure `ToolCache` attributes to dedupe and
minimization; `cache_hit_rate` is derived, not reported.

`cache_hit_rate = cache_read / (cache_read + input)` — the same quantity
`agent/crates/harness/src/cache_stats.rs` prices for waste, expressed as a rate. Rows are
appended as JSONL under the Shob data dir, capped in size, and mirrored into an in-memory
summary the UI can read. Fed from `AgentEvent::Usage`, which `src/agent_bridge.rs`
already produces.

## 2.13 Configuration

`AppSettings.token_efficiency`. Every feature is independently switchable. All
default on, except `trust_mtime`, which trades exactness for a stat and must be
opted into:

```jsonc
"token_efficiency": {
  "enabled": true,
  "read_dedupe": true,
  "search_cache": true,
  "anchor_edits": true,
  "minimize_output": true,
  "route_cheap_tasks": true,
  "metrics": true,
  "trust_mtime": false,
  "dedupe_window_turns": 8,
  "max_validate_files": 2000,
  "minimize_over_bytes": 4096,
  "minimize_max_bytes": 24576,
  "smol_model": null
}
```

`SHOB_TOKEN_EFFICIENCY=0` disables the layer for one run without touching settings,
following the existing `SHOB_*` convention.

## 2.14 Why this cannot serve stale data

Enumerating the ways a cache can lie, and the guard for each:

- **Content changed, entry not evicted** → hits require a byte-level hash of the file as
  it is *now*. `mtime` never promotes an entry to hit on its own.
- **Line numbers shifted after an edit** → anchor expansion re-hashes and re-reads the
  file; a stale tag is rejected, not applied.
- **Body pruned from context but stub retained** → dedupe is cleared on compaction and
  bounded by a turn window.
- **Truncated result mistaken for complete** → capped/errored results are never cached.
- **Hash collision** → SHA-256, and the stamp also carries byte length and line count, so
  a collision would have to match all three.
- **Resume across processes** → cache is in-memory only.
