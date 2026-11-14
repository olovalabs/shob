//! Bounded host-owned direct-child metadata, independent of poll/dispose.
use crate::AgentToolResult;
use rpi_ai::types::Usage;
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct Accounting {
    has_children: bool,
    usage: Option<Usage>,
    names: BTreeSet<String>,
    terminate: bool,
    invalid_usage: bool,
    invalid_names: bool,
}
impl Accounting {
    pub(super) fn retain(&mut self, result: &AgentToolResult) {
        self.has_children = true;
        self.terminate |= result.terminate;
        if let Some(usage) = &result.usage {
            let mut sum = self.usage.clone().unwrap_or_else(Usage::zero);
            if checked_add(&mut sum, usage).is_ok() && !self.invalid_usage {
                self.usage = Some(sum);
            } else {
                self.invalid_usage = true;
                self.usage = None;
            }
        }
        for name in &result.added_tool_names {
            if name.len() > 256 || (!self.names.contains(name) && self.names.len() >= 256) {
                self.invalid_names = true;
            } else {
                self.names.insert(name.clone());
            }
        }
    }
    pub(super) fn merge(&self, result: &mut AgentToolResult) -> Result<(), String> {
        // Capture also exists for ordinary tools. Do not sort, validate, or
        // rewrite their metadata unless they actually composed a child call.
        if !self.has_children {
            return Ok(());
        }
        let mut combined = Accounting::default();
        combined.retain(result);
        // Reuse the same checked/bounded path for the containing tool's own metadata.
        combined.retain(&AgentToolResult {
            usage: self.usage.clone(),
            added_tool_names: self.names.iter().cloned().collect(),
            terminate: self.terminate,
            ..Default::default()
        });
        let invalid_usage = self.invalid_usage || combined.invalid_usage;
        let invalid_names = self.invalid_names || combined.invalid_names;
        result.usage = if invalid_usage { None } else { combined.usage };
        // A rejected name set has no completion-order-dependent partial prefix.
        result.added_tool_names = if invalid_names {
            Vec::new()
        } else {
            combined.names.into_iter().collect()
        };
        result.terminate = combined.terminate;
        if invalid_usage || invalid_names {
            Err("ToolProgram accounting overflow, non-finite cost, or metadata limit exceeded; invalid metadata omitted".into())
        } else {
            Ok(())
        }
    }
}
fn checked_add(sum: &mut Usage, value: &Usage) -> Result<(), ()> {
    macro_rules! tokens {
        ($($field:ident),*) => {$(sum.$field = sum.$field.checked_add(value.$field).ok_or(())?;)*};
    }
    tokens!(input, output, cache_read, cache_write, total_tokens);
    for (a, b) in [
        (&mut sum.cache_write_1h, value.cache_write_1h),
        (&mut sum.reasoning, value.reasoning),
    ] {
        if let Some(b) = b {
            *a = Some(a.unwrap_or(0).checked_add(b).ok_or(())?);
        }
    }
    macro_rules! cost {
        ($($field:ident),*) => {$(
            let next = sum.cost.$field + value.cost.$field;
            if !next.is_finite() { return Err(()); }
            sum.cost.$field = next;
        )*};
    }
    cost!(input, output, cache_read, cache_write, total);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn result(usage: Usage) -> AgentToolResult {
        AgentToolResult {
            usage: Some(usage),
            terminate: true,
            ..Default::default()
        }
    }
    #[test]
    fn ordinary_tool_without_children_keeps_metadata_unchanged() {
        let accounting = Accounting::default();
        let mut usage = Usage::zero();
        usage.cost.total = f64::INFINITY;
        let mut outer = AgentToolResult {
            usage: Some(usage),
            added_tool_names: vec!["z".into(), "a".into(), "z".into(), "x".repeat(300)],
            terminate: true,
            ..Default::default()
        };
        let names = outer.added_tool_names.clone();
        assert!(accounting.merge(&mut outer).is_ok());
        assert_eq!(outer.added_tool_names, names);
        assert!(outer.usage.unwrap().cost.total.is_infinite());
        assert!(outer.terminate);
    }
    #[test]
    fn checked_usage_rejects_every_integer_overflow_and_nonfinite_cost_without_panicking() {
        for field in [
            "input",
            "output",
            "cache_read",
            "cache_write",
            "total_tokens",
            "reasoning",
            "cache_write_1h",
        ] {
            let mut first = Usage::zero();
            let mut second = Usage::zero();
            macro_rules! set {
                ($name:ident) => {
                    if field == stringify!($name) {
                        first.$name = i64::MAX;
                        second.$name = 1;
                    }
                };
            }
            set!(input);
            set!(output);
            set!(cache_read);
            set!(cache_write);
            set!(total_tokens);
            if field == "reasoning" {
                first.reasoning = Some(i64::MAX);
                second.reasoning = Some(1);
            }
            if field == "cache_write_1h" {
                first.cache_write_1h = Some(i64::MAX);
                second.cache_write_1h = Some(1);
            }
            let mut accounting = Accounting::default();
            accounting.retain(&result(first));
            accounting.retain(&result(second));
            let mut outer = AgentToolResult::default();
            assert!(accounting.merge(&mut outer).is_err(), "{field}");
            assert!(outer.usage.is_none());
            assert!(outer.terminate);
        }
        for cost in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            let mut usage = Usage::zero();
            usage.cost.total = cost;
            let mut accounting = Accounting::default();
            accounting.retain(&result(usage.clone()));
            accounting.retain(&result(usage));
            let mut outer = AgentToolResult::default();
            assert!(accounting.merge(&mut outer).is_err());
            assert!(outer.usage.is_none());
            assert!(outer.terminate);
        }
    }
    #[test]
    fn own_plus_child_overflow_and_bounded_name_metadata_fail_closed() {
        let mut usage = Usage::zero();
        usage.input = i64::MAX;
        let mut accounting = Accounting::default();
        accounting.retain(&result(usage));
        let mut usage = Usage::zero();
        usage.input = 1;
        let mut outer = result(usage);
        assert!(accounting.merge(&mut outer).is_err());
        assert!(outer.usage.is_none());
        let mut accounting = Accounting::default();
        accounting.retain(&AgentToolResult {
            added_tool_names: (0..300).map(|i| format!("name-{i}")).collect(),
            ..Default::default()
        });
        let mut outer = AgentToolResult::default();
        assert!(accounting.merge(&mut outer).is_err());
        assert!(outer.added_tool_names.len() <= 256);
    }
}
