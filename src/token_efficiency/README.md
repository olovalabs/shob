# Token efficiency layer

A caching and token-reduction layer for Shob's embedded agent chat, built to
reduce prompt spend without hurting task success.

| Document | Contents |
| --- | --- |
| [1. Reference analysis](01-reference-analysis.md) | What oh-my-pi and opencode do, with file citations, and what Shob already had |
| [2. Design](02-design.md) | Architecture, cache keys, invalidation rules, configuration |
| [3. Token savings](03-token-savings.md) | Measured before/after benchmark, per-feature attribution, projections |
| [4. Tradeoffs](04-tradeoffs.md) | Risks, costs, the `agent/` diff, what is not done |

## The short version

| Feature | What it does | Where |
| --- | --- | --- |
| Read dedupe | A re-read of unchanged bytes becomes `[unchanged since turn N]` | `src/token_efficiency/cache.rs` |
| Search cache | An identical `grep`/`find`/`ls` is answered locally when nothing it scanned changed | `src/token_efficiency/cache.rs`, `hooks.rs` |
| Anchor edits | `edit` accepts `PUT n.==m:` against a hashed line snapshot instead of retyped `oldText` | `src/token_efficiency/anchors.rs` |
| Output minimization | Repeated/blank/noise lines collapse in `bash`/`grep`/`find`/`ls` output | `src/token_efficiency/minimize.rs` |
| Cheap-task routing | Conversation titles run on a configured `smol` model | `src/token_efficiency/routing.rs` |
| Prompt-prefix guard | Detects a volatile system prompt or reordered tools, which silently invalidate the provider cache | `src/token_efficiency/prefix.rs` |
| Measurement | Per-turn input/output/cache tokens and cache hit rate, as JSONL | `src/token_efficiency/metrics.rs` |

Measured on the two recorded tasks in `src/token_efficiency/fixtures/`:
**310,104 → 185,264 prompt tokens, a 40.3% reduction**, of which read dedupe is
79% and output minimization 20%.

## Try it

```bash
cargo run -- token-bench                 # the before/after table
cargo run -- token-bench --out out.md    # write it to a file
cargo test token_efficiency              # 145 tests: hits, misses, invalidation
SHOB_TOKEN_EFFICIENCY=0 cargo run        # run with the layer off
```

## Two invariants worth knowing before changing anything

1. **A hit requires a byte-level digest of the file as it is now.** `mtime` is
   recorded but never decides, except under the explicitly-unsafe `trust_mtime`
   opt-in.
2. **Dedupe ledgers are cleared on compaction.** A stub that says "see turn 7" is
   a lie once turn 7 has been summarized away.

Both are pinned by tests; see [Design §2.11](02-design.md#211-invalidation-rules-the-whole-contract).
