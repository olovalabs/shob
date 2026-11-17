//! Mirrors `packages/ai/src/models.ts::calculateCost` — the tiered usage-cost
//! calculator. Hoisted under the anthropic provider module because only
//! anthropic calls it in v1, but the math is provider-agnostic.
//!
//! The TS `calculateCost` mutates `usage.cost` in place; the Rust port returns
//! a fresh `UsageCost` (the `Usage` is owned by the caller). The 1h-cache-write
//! surcharge (2× input) is applied to the `cache_write_1h` subset, matching the
//! TS `cacheWrite * shortWrite + input * 2 * longWrite` line.

use crate::types::{ModelCost, ModelCostRates, Usage, UsageCost};

/// Recompute `usage.cost` against `model.cost`. Mirrors TS `calculateCost`.
///
/// Tier selection: walk `model.cost.tiers`, pick the tier with the largest
/// `input_tokens_above` that the request's billable input (`input + cacheRead +
/// cacheWrite`) exceeds. Falls back to the model's base rates.
pub fn calculate_cost(cost: &ModelCost, usage: &Usage) -> UsageCost {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;

    let mut rates: ModelCostRates = cost.rates;
    let mut matched_threshold: i64 = -1;
    for tier in &cost.tiers {
        if input_tokens > tier.input_tokens_above && tier.input_tokens_above > matched_threshold {
            rates = tier.rates;
            matched_threshold = tier.input_tokens_above;
        }
    }

    // Anthropic charges 2× base input for 1h cache writes.
    let long_write = usage.cache_write_1h.unwrap_or(0).max(0);
    let short_write = (usage.cache_write - long_write).max(0);

    let input_cost = (rates.input / 1_000_000.0) * usage.input as f64;
    let output_cost = (rates.output / 1_000_000.0) * usage.output as f64;
    let cache_read_cost = (rates.cache_read / 1_000_000.0) * usage.cache_read as f64;
    let cache_write_cost = (rates.cache_write * short_write as f64
        + rates.input * 2.0 * long_write as f64)
        / 1_000_000.0;
    let total = input_cost + output_cost + cache_read_cost + cache_write_cost;

    UsageCost {
        input: input_cost,
        output: output_cost,
        cache_read: cache_read_cost,
        cache_write: cache_write_cost,
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCostRates, ModelCostTier, Usage};
    use serde_json::json;

    fn rates(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelCostRates {
        ModelCostRates {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    fn usage(input: i64, output: i64, cache_read: i64, cache_write: i64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output + cache_read + cache_write,
            cost: UsageCost::default(),
        }
    }

    #[test]
    fn base_rates_no_tiers() {
        // claude-opus-4-8: input 5, cacheWrite (5m) 6.25 per Mtok.
        let cost = ModelCost {
            rates: rates(5.0, 25.0, 0.5, 6.25),
            tiers: vec![],
        };
        let u = usage(100, 5, 0, 1_000_000);
        let c = calculate_cost(&cost, &u);
        // input 100 * 5/M = 0.0005; cacheWrite 1M * 6.25/M = 6.25.
        assert!((c.input - 0.0005).abs() < 1e-9);
        assert!((c.cache_write - 6.25).abs() < 1e-9);
        assert!((c.total - (0.0005 + 6.25 + 5.0 * 25.0 / 1_000_000.0)).abs() < 1e-9);
    }

    #[test]
    fn long_cache_write_charged_2x_input() {
        // 1M total cacheWrite, 400k of it 1h. input=5 → 1h rate = 10/M.
        let cost = ModelCost {
            rates: rates(5.0, 25.0, 0.5, 6.25),
            tiers: vec![],
        };
        let mut u = usage(100, 5, 0, 1_000_000);
        u.cache_write_1h = Some(400_000);
        let c = calculate_cost(&cost, &u);
        // 600k * 6.25/M + 400k * 10/M = 3.75 + 4.0 = 7.75.
        assert!((c.cache_write - 7.75).abs() < 1e-9);
        assert_eq!(u.cache_write_1h, Some(400_000));
    }

    #[test]
    fn tier_promotion_picks_largest_matching_threshold() {
        let cost = ModelCost {
            rates: rates(5.0, 25.0, 0.5, 6.25),
            tiers: vec![
                ModelCostTier {
                    rates: rates(10.0, 40.0, 1.0, 12.5),
                    input_tokens_above: 1_000,
                },
                ModelCostTier {
                    rates: rates(3.0, 15.0, 0.25, 4.0),
                    input_tokens_above: 100_000,
                },
            ],
        };
        // Billable input = 5_000 → exceeds 1_000 but not 100_000 → first tier.
        let u = usage(5_000, 0, 0, 0);
        let c = calculate_cost(&cost, &u);
        assert!((c.input - (10.0 / 1_000_000.0) * 5_000.0).abs() < 1e-9);
        // Billable input = 200_000 → exceeds both → second (higher) tier.
        let u2 = usage(200_000, 0, 0, 0);
        let c2 = calculate_cost(&cost, &u2);
        assert!((c2.input - (3.0 / 1_000_000.0) * 200_000.0).abs() < 1e-9);
        // Sanity: JSON round-trips the tier struct (it derives Serialize).
        let _ = serde_json::to_value(&cost.tiers[0]).unwrap();
        let _ = json!(cost.tiers[0].input_tokens_above);
    }
}
