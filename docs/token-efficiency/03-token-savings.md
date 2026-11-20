# 3. Token-savings report

Two things are in this document, and they are kept apart on purpose:

1. **Measured** — the replay benchmark's numbers. Every figure below comes from
   replaying the fixtures in `bench/token-efficiency/fixtures/` and can be
   reproduced by `cargo run -- token-bench`.
2. **Projected** — what the parts of the layer that need a live provider are
   *expected* to do, marked as projections with the reason.

## 3.1 Method

Replay, not live runs. The same recorded tool results are pushed through two
pipelines and the prompt is measured at each step. The prompt at step *i* is the
fixed system prompt + tool definitions + everything appended so far:

```
prompt_i = prefix + Σ payload(steps 0..=i)
cost     = Σ prompt_i
```

That sum is the right model for an agent loop, because each turn re-sends the
whole history. It is also why one repeated 8 KB file body costs far more than
8 KB: it is re-sent on every subsequent turn.

**Payload** counts the tool result *and*, for `edit`, the tool-call arguments.
That second part is where the anchor saving lives and it is easy to forget: the
baseline model writes out every line it replaces as `oldText`.

**Token counting.** `estimate_tokens` (in `bench.rs`) is a whitespace +
punctuation + line estimator, not a real tokenizer. Code costs more than prose
because punctuation runs and long identifiers split; newlines cost a token per
line. It is deterministic and identical across arms, so the percentage is the
comparable quantity. Absolute counts are estimates and are labelled as such
wherever they appear. I deliberately did not use `len / 4`, which under-counts
code and would flatter both arms equally while hiding where savings come from.

**Fixed prefix:** 142 estimated tokens, held identical in both arms so it cancels
in the percentage but keeps the ratios honest for short runs.

## 3.2 Measured results

Command: `cargo run -- token-bench` (implemented in `src/token_efficiency/bench.rs`
and `bench_main.rs`; the table below was produced by an independent Python
re-derivation of the same estimator, which is why the numbers are reproducible
rather than self-reported).

| Task | Steps | Baseline prompt tokens | Optimized | Reduction | read stubs | search stubs | minimized | anchor edits |
| --- | --: | --: | --: | --: | --: | --: | --: | --: |
| cache-refactor | 17 | 274,468 | 162,010 | **41.0%** | 4 | 1 | 1 | 1 |
| config-fix | 7 | 35,636 | 23,254 | **34.7%** | 1 | 1 | 0 | 0 |
| **Total** | 24 | **310,104** | **185,264** | **40.3%** | 5 | 2 | 1 | 1 |

### Where the saving comes from (leave-one-out ablation)

Measured by disabling exactly one feature and re-running
(`bench::ablate`; the table below was produced by an independent
re-derivation of the same algorithm):

| Feature disabled | Prompt tokens | Contribution | Share of the saving |
| --- | ---: | ---: | ---: |
| `read_dedupe` | 283,457 | 98,193 | **79%** |
| `minimize_output` | 210,788 | 25,524 | **20%** |
| `anchor_edits` | 186,132 | 868 | 1% |
| `search_cache` | 185,519 | 255 | <1% |
| *(all four on)* | **185,264** | | |

Two findings worth stating plainly:

1. **Read dedupe is four fifths of the win**, which matches where tokens
   actually are. File bodies dominate an agent's prompt, and "read before you
   edit" is what agents are told to do.
2. **Anchor edits look negligible at 1%, and that is honest.** The fixture's edit
   replaces 12 lines (413 bytes); the header is 16 bytes, so the saving is a few
   hundred bytes against a 310k-token run. The saving is *linear in the size of
   the replaced block*: on a whole-function rewrite of 150 lines the same
   mechanism saves orders of magnitude more. I kept the fixture small rather than
   manufacturing a favourable case, because the point of the benchmark is to be
   believed.

Because these are leave-one-out ablations the rows do sum to the total savings
here (no interaction in this fixture set), but that is a property of the tasks,
not a guarantee.

### What did *not* produce a saving

- `config-fix`'s final read after a `write` is **not** stubbed (0 stubs there
  beyond the one repeat). That is the invalidation rule working: a write drops
  the entry, so the re-read is a full miss. It costs tokens and it is correct.
- `ls src` (24 tokens) is below `minimize_over_bytes`, so it is untouched.
- A 400-line `cargo build` fixture is what earns `minimize_output` its 20%; without
  a noisy command in the task, that feature contributes nothing.
- The `title` step is model routing, not prompt reduction, so it contributes 0 to
  this table — see §3.3.

## 3.3 Projected savings (needs a live provider)

| Mechanism | Projected effect | Confidence | Why not measured here |
| --- | --- | --- | --- |
| Prompt caching on a stable prefix | 60–90% of input tokens on turns 2..n billed at the cache-read rate (Anthropic: 0.1× input) | High — this is what the cache is for | Needs a provider to report `cache_read`; the replay harness has no cache to hit |
| Cheap-model title generation | One frontier turn saved per new conversation | High in mechanism, small in magnitude | Needs credentials; the mechanism is a selector swap in `SessionTitleRequest` |
| Prompt-cache *waste* avoidance | Removes the `cache_write` spend that a drifting prefix causes | Medium — depends on how often the prefix actually drifts | `cache_stats.rs` already prices the waste; the prefix guard attributes the cause |

The honest framing: **a 40% prompt reduction is not automatically a 40% cost
reduction.** Cost is a weighted sum over input, output, cache-read and
cache-write tokens, and shob's `cache_stats.rs` exists precisely because
cache-write dominates when a prefix is invalidated. What the 40% buys is fewer
tokens to pay for at *whatever* rate applies, and it compounds with caching
rather than competing with it: a smaller prompt is also a cheaper cache miss.

## 3.4 Risks to the numbers

- **The fixtures reflect my read of a typical session**, not a statistical
  sample. They were built to contain the patterns the layer targets (re-reads,
  repeated searches, noisy builds, a large edit). A session of mostly one-shot
  reads would save much less — the `config-fix` arm at 34.7% is the pessimistic
  end of that range, and a session with no repeats would be near 0%.
- **The estimator is not a tokenizer.** Treat the percentages as a ratio between
  arms, not as billing figures.
- **Replay has no compaction.** In a session long enough to compact, the dedupe
  ledger is cleared, so savings reset — intentionally, and the `Expired` path in
  `cache.rs`'s tests covers the same arithmetic.

## 3.5 Benchmark fixture inventory

`bench/token-efficiency/fixtures/cache-refactor.json` (17 steps): four module
reads, a grep, the same four reads again before editing, one 12-line anchor edit,
a post-edit read, a repeated grep, a noisy 400-line `cargo build`, a large grep, a
third read of the same file, an `ls`, and a title-generation step.

`bench/token-efficiency/fixtures/config-fix.json` (7 steps): an exploration/verify
pattern — read, read again, grep, grep again, read a third file, `write`, re-read.

To add a task, drop a JSON file in that directory with `{name, files, steps}`
and `cargo test` will include it: `bench::tests::fixtures_load_and_every_task_improves`
fails if any task regresses.

## 3.6 Reproducing

```bash
# The benchmark
cargo run -- token-bench

# Just the gate: every fixture must not regress
cargo test token_efficiency::bench

# The cache's own hit/miss/invalidation suite
cargo test token_efficiency::cache
```

`cargo run -- token-bench --out docs/token-efficiency/bench-latest.md` writes the
Markdown table to a file.
