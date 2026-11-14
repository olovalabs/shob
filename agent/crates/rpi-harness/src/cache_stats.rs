//! Prompt-cache waste accounting.
//!
//! Port of upstream's `packages/coding-agent/src/core/cache-stats.ts`.
//!
//! Anthropic-style prompt caching is only a win when the previous turn's
//! prompt is re-read from cache. When it isn't — because the cache expired, the
//! model changed, or a breakpoint was missed — the same tokens are re-billed at
//! the (more expensive) input/write rate. This module detects those "wasteful"
//! misses and prices them, so the UI can explain a surprising cost.

use rpi_agent::message::AgentMessage;
use rpi_ai::types::AssistantMessage;

use crate::session::types::Entry;

/// Prompt-cache TTL: idle gaps longer than this are worth mentioning as the
/// likely cause of a miss. Anthropic's default cache TTL is 5 minutes.
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache breakpoint granularity noise.
const NOISE_FLOOR_TOKENS: i64 = 1024;

/// A counted cache miss on a single assistant message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheMiss {
    /// Prompt tokens that were in the previous turn's prompt but not read from cache.
    pub missed_tokens: i64,
    /// Extra dollars paid vs. a full cache hit; 0 when pricing is unknown.
    pub missed_cost: f64,
    /// Milliseconds since the previous request (which last refreshed the cache).
    pub idle_ms: i64,
    /// True when the model changed relative to the previous request.
    pub model_changed: bool,
}

/// Cumulative cache waste across a session.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: i64,
    pub missed_cost: f64,
    /// Number of counted misses (turns above the noise floor).
    pub miss_count: u64,
}

/// Minimal pricing lookup. Cost is $ per **million** tokens (native
/// `ModelPriceSource` returns `cost.cacheRead`).
pub trait ModelPriceSource {
    fn cache_read_price_per_million(&self, provider: &str, model: &str) -> Option<f64>;
}

/// A price source that never knows prices (missed cost reported as 0).
pub struct NoPrices;

impl ModelPriceSource for NoPrices {
    fn cache_read_price_per_million(&self, _provider: &str, _model: &str) -> Option<f64> {
        None
    }
}

/// The last request seen by the scan; everything in its prompt should be cached.
#[derive(Debug, Clone)]
struct PreviousRequest {
    prompt_tokens: i64,
    model_key: String,
    timestamp: i64,
    /// Sticky: some earlier request in this scan segment reported cache
    /// activity. Distinguishes a total miss on a cache-read-only provider from
    /// a provider that never reports caching at all.
    reported_cache: bool,
}

fn assistant_of(entry: &Entry) -> Option<&AssistantMessage> {
    match entry.as_message()? {
        AgentMessage::Assistant(a) => Some(a.as_ref()),
        _ => None,
    }
}

/// Compute the cache miss for one assistant message relative to the previous
/// request. Returns `None` when nothing is counted: first turn, after a reset,
/// no cache activity ever reported, or a miss below the noise floor.
fn detect_miss(
    prev: Option<&PreviousRequest>,
    message: &AssistantMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let usage = &message.usage;
    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
    // A zero-cache turn only counts when cache activity was reported before.
    let prev = prev?;
    if prompt_tokens <= 0 || (usage.cache_read + usage.cache_write == 0 && !prev.reported_cache) {
        return None;
    }

    let missed_tokens = prev.prompt_tokens.min(prompt_tokens) - usage.cache_read;
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    // Extra cost = missed tokens billed at the actual paid rate (input/cacheWrite)
    // instead of the cache-read rate. Missed tokens can only land in the input or
    // cacheWrite buckets, so the paid rate comes from this message's own cost.
    let paid_tokens = usage.input + usage.cache_write;
    let paid_per_token = if paid_tokens > 0 {
        (usage.cost.input + usage.cost.cache_write) / paid_tokens as f64
    } else {
        0.0
    };
    let read_per_token = if usage.cache_read > 0 {
        usage.cost.cache_read / usage.cache_read as f64
    } else {
        models
            .cache_read_price_per_million(&message.provider, &message.model)
            .map(|p| p / 1_000_000.0)
            .unwrap_or(0.0)
    };

    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
        idle_ms: (message.timestamp - prev.timestamp).max(0),
        model_changed: format!("{}/{}", message.provider, message.model) != prev.model_key,
    })
}

fn as_previous_request(
    message: &AssistantMessage,
    reported_cache: bool,
) -> Option<PreviousRequest> {
    let usage = &message.usage;
    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
    if prompt_tokens <= 0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens,
        model_key: format!("{}/{}", message.provider, message.model),
        timestamp: message.timestamp,
        reported_cache: reported_cache || usage.cache_read + usage.cache_write > 0,
    })
}

struct Scan<'a> {
    prev: Option<PreviousRequest>,
    totals: CacheWasteTotals,
    misses: Vec<(usize, &'a AssistantMessage, CacheMiss)>,
}

fn scan<'a>(entries: &'a [Entry], models: &dyn ModelPriceSource) -> Scan<'a> {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();
    let mut misses = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        match entry {
            // The context legitimately changed; the next turn's prompt is new
            // content, not re-billed content. Model switches are NOT exempt:
            // they re-bill the full prompt and should be counted.
            Entry::Compaction(_) | Entry::BranchSummary(_) => {
                prev = None;
            }
            _ => {
                if let Some(message) = assistant_of(entry) {
                    if let Some(miss) = detect_miss(prev.as_ref(), message, models) {
                        totals.missed_tokens += miss.missed_tokens;
                        totals.missed_cost += miss.missed_cost;
                        totals.miss_count += 1;
                        misses.push((index, message, miss));
                    }
                    prev = as_previous_request(
                        message,
                        prev.as_ref().map(|p| p.reported_cache).unwrap_or(false),
                    )
                    .or(prev);
                }
            }
        }
    }

    Scan {
        prev,
        totals,
        misses,
    }
}

/// Cumulative cache waste across a session: prompt tokens that should have been
/// cache reads (they were in the previous turn's prompt) but were re-billed.
pub fn compute_cache_waste(entries: &[Entry], models: &dyn ModelPriceSource) -> CacheWasteTotals {
    scan(entries, models).totals
}

/// All counted cache misses across a session, keyed by the index of the
/// assistant message that paid for them. Used to re-derive transcript notices
/// when rebuilding the chat from entries.
pub fn collect_cache_misses<'a>(
    entries: &'a [Entry],
    models: &dyn ModelPriceSource,
) -> Vec<(usize, &'a AssistantMessage, CacheMiss)> {
    scan(entries, models).misses
}

/// Detect a cache miss on a just-completed assistant message. `entries` must
/// not yet contain `message` (message_end fires before persistence).
pub fn detect_cache_miss(
    entries: &[Entry],
    message: &AssistantMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let scan = scan(entries, models);
    detect_miss(scan.prev.as_ref(), message, models)
}

/// Incremental tracker for callers that observe assistant messages one at a
/// time (e.g. the streaming TUI) instead of re-scanning the whole transcript.
/// Feed each finalized assistant message to [`CacheMissTracker::observe`]; it
/// returns the same [`CacheMiss`] the batch scan would have produced.
#[derive(Debug, Default)]
pub struct CacheMissTracker {
    prev: Option<PreviousRequest>,
}

impl CacheMissTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The context legitimately changed (compaction / branch summary); the next
    /// turn's prompt is new content, not re-billed content.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// Observe a finalized assistant message, returning a miss when one is
    /// counted (see [`detect_miss`]).
    pub fn observe(
        &mut self,
        message: &AssistantMessage,
        models: &dyn ModelPriceSource,
    ) -> Option<CacheMiss> {
        let miss = detect_miss(self.prev.as_ref(), message, models);
        let reported = self
            .prev
            .as_ref()
            .map(|p| p.reported_cache)
            .unwrap_or(false);
        if let Some(next) = as_previous_request(message, reported).or_else(|| self.prev.clone()) {
            self.prev = Some(next);
        }
        miss
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::{Api, AssistantRole, ProviderId, StopReason, Usage, UsageCost};

    fn msg(
        provider: &str,
        model: &str,
        input: i64,
        cache_read: i64,
        cache_write: i64,
        ts: i64,
    ) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole,
            content: vec![],
            api: Api::AnthropicMessages,
            provider: ProviderId::from(provider),
            model: model.to_string(),
            response_model: None,
            response_id: None,
            usage: Usage {
                input,
                output: 0,
                cache_read,
                cache_write,
                cache_write_1h: None,
                reasoning: None,
                total_tokens: input + cache_read + cache_write,
                cost: UsageCost::default(),
            },
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: Some(true),
            timestamp: ts,
        }
    }

    #[test]
    fn first_turn_has_no_miss() {
        let m = msg("anthropic", "claude", 10_000, 0, 0, 1000);
        assert!(detect_miss(None, &m, &NoPrices).is_none());
    }

    #[test]
    fn total_miss_above_noise_floor_counts() {
        // prev prompt 10_000, this turn re-reads 0 of it.
        let prev = PreviousRequest {
            prompt_tokens: 10_000,
            model_key: "anthropic/claude".into(),
            timestamp: 1000,
            reported_cache: true,
        };
        let m = msg("anthropic", "claude", 10_000, 0, 0, 40_000);
        let miss = detect_miss(Some(&prev), &m, &NoPrices).expect("miss");
        assert_eq!(miss.missed_tokens, 10_000);
        assert_eq!(miss.idle_ms, 39_000);
        assert!(!miss.model_changed);
    }

    #[test]
    fn small_miss_is_noise_floor() {
        let prev = PreviousRequest {
            prompt_tokens: 500,
            model_key: "anthropic/claude".into(),
            timestamp: 0,
            reported_cache: true,
        };
        let m = msg("anthropic", "claude", 500, 0, 0, 1);
        assert!(detect_miss(Some(&prev), &m, &NoPrices).is_none());
    }

    #[test]
    fn model_change_flag_set() {
        let prev = PreviousRequest {
            prompt_tokens: 10_000,
            model_key: "anthropic/claude".into(),
            timestamp: 0,
            reported_cache: true,
        };
        let m = msg("anthropic", "claude-2", 10_000, 0, 0, 1);
        let miss = detect_miss(Some(&prev), &m, &NoPrices).expect("miss");
        assert!(miss.model_changed);
    }
}
