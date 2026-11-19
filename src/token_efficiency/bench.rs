//! Replay benchmark: what the layer saves on a fixed set of recorded tasks.
//!
//! # Why replay and not live runs
//!
//! A live benchmark measures the provider and the weather. Replay measures the
//! layer: the same recorded tool results are pushed through the "before" and
//! "after" pipelines and the prompt is measured at each step. No credentials, no
//! network, and the same answer tomorrow.
//!
//! # What is measured
//!
//! The prompt a provider would receive at step *i* is everything appended so far:
//!
//! ```text
//! system + tools + Σ(steps 0..=i)
//! ```
//!
//! and the run's cost is the sum over steps. That is the right model for an agent
//! loop, where each turn re-sends the whole history, and it is why a repeated file
//! body costs far more than its own size. `cache_read` is not credited: the
//! benchmark answers "how many prompt tokens does the layer make the provider
//! read", and prompt caching then discounts whatever prefix survives.
//!
//! # Token counting
//!
//! [`estimate_tokens`] is a calibrated whitespace + punctuation estimator, not a
//! real tokenizer. It is deterministic and comparable across arms, which is what
//! a before/after comparison needs. The absolute numbers are reported as
//! estimates, and the *percentage* saving is the claim.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::config::TokenEfficiencySettings;
use super::fsprobe::testing::FakeFs;

/// One recorded tool interaction.
#[derive(Clone, Debug, Deserialize)]
pub struct Step {
    pub desc: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub result: String,
}

/// A recorded task.
#[derive(Clone, Debug, Deserialize)]
pub struct Task {
    pub name: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    pub steps: Vec<Step>,
}

/// One arm's numbers for one task.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct ArmResult {
    /// Prompt tokens summed over every step.
    pub prompt_tokens: u64,
    /// Result bytes that reached the model, summed over steps.
    pub result_bytes: u64,
    /// Prompt tokens of the final step, i.e. the context the run ends with.
    pub final_prompt_tokens: u64,
    pub read_stubs: u64,
    pub search_stubs: u64,
    pub minimized: u64,
    pub anchor_edits: u64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Comparison {
    pub task: String,
    pub baseline: ArmResult,
    pub optimized: ArmResult,
    /// `1 - optimized/baseline` on prompt tokens.
    pub prompt_reduction: f64,
}

/// A representative system prompt + tool block, so the measurement includes the
/// part of the prompt the layer cannot change. Held fixed so it cancels in the
/// comparison but keeps the percentages honest.
pub fn fixed_prefix_tokens() -> u64 {
    // ~1.2k tokens: a realistic harness prompt with seven tool definitions.
    estimate_tokens(FIXED_PREFIX)
}

const FIXED_PREFIX: &str = "\
You are a coding agent operating inside Shob. Work in the repository at the
current working directory. Prefer the dedicated tools over shell commands.
Read a file before editing it. Keep changes consistent with the surrounding
style. Do not reformat code you did not change. Run the focused tests for the
module you touched before claiming a change works. Report what you verified.
Tools: read (path, offset, limit), write (path, content), edit (path, edits[]),
bash (command), grep (pattern, path, glob, ignoreCase, literal, limit),
find (glob, path), ls (path).
";

/// Estimate tokens for `text`.
///
/// Agent tool output is dominated by code and paths, where the useful signal is
/// punctuation density and word length rather than word identity:
///
/// - every run of word characters is one token, with a bonus for long runs
///   (identifiers and paths split into roughly one token per 5 characters);
/// - each punctuation run is ~0.6 tokens, because `::`, `()`, `->` and `};`
///   usually survive as their own tokens;
/// - newlines and indentation cost one token per line, which matches how code
///   tokenizes far better than a character count does.
///
/// Deliberately not `len / 4`: that systematically under-counts exactly the code
/// and prose mix this benchmark is made of, and it would flatter every arm
/// equally while hiding where the savings come from.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut tokens = 0u64;
    let mut word_chars = 0u64;
    let mut punctuation = 0u64;
    let mut lines = 1u64;

    let flush_word = |word_chars: &mut u64, tokens: &mut u64| {
        if *word_chars > 0 {
            // One token up to ~5 chars, then one per 5.
            *tokens += 1 + (*word_chars.saturating_sub(5)) / 5;
            *word_chars = 0;
        }
    };
    let flush_punctuation = |punctuation: &mut u64, tokens: &mut u64| {
        if *punctuation > 0 {
            *tokens += (*punctuation * 6 + 9) / 10;
            *punctuation = 0;
        }
    };

    for character in text.chars() {
        if character == '\n' {
            lines += 1;
            flush_word(&mut word_chars, &mut tokens);
            flush_punctuation(&mut punctuation, &mut tokens);
        } else if character.is_alphanumeric() || character == '_' {
            flush_punctuation(&mut punctuation, &mut tokens);
            word_chars += 1;
        } else if character.is_whitespace() {
            flush_word(&mut word_chars, &mut tokens);
            flush_punctuation(&mut punctuation, &mut tokens);
        } else {
            flush_word(&mut word_chars, &mut tokens);
            punctuation += 1;
        }
    }
    flush_word(&mut word_chars, &mut tokens);
    flush_punctuation(&mut punctuation, &mut tokens);

    tokens + lines
}

/// What one step contributes to the conversation, per arm.
///
/// Both arms contribute the tool result *and* the assistant's tool-call
/// arguments, because both are tokens the model pays for. For an `edit` that is
/// where the anchor saving lives: the baseline re-sends every replaced line as
/// `oldText`, the optimized arm sends a header and the new lines.
fn baseline_payload(step: &Step) -> String {
    match step.tool.as_str() {
        "edit" => {
            let old = step.args.get("oldText").and_then(|v| v.as_str()).unwrap_or("");
            format!("{}\n{}", old, step.result)
        }
        _ => step.result.clone(),
    }
}

/// Run one task with the layer off: every recorded result is sent verbatim.
pub fn run_baseline(task: &Task) -> ArmResult {
    let mut result = ArmResult::default();
    let mut history_tokens = fixed_prefix_tokens();

    for step in &task.steps {
        let payload = baseline_payload(step);
        history_tokens += estimate_tokens(&payload);
        result.prompt_tokens += history_tokens;
        result.result_bytes += payload.len() as u64;
        result.final_prompt_tokens = history_tokens;
    }
    result
}

/// Run one task through the layer, using the same entry points the live hooks
/// use (`hooks::intercept` before, `hooks::observe_result` after).
pub fn run_optimized(task: &Task, settings: &TokenEfficiencySettings) -> ArmResult {
    let mut result = ArmResult::default();
    let fs = std::sync::Arc::new(FakeFs::new());
    for (path, content) in &task.files {
        // Fixtures may carry snapshots (e.g. `src/cache.rs_after_edit`) that are
        // not real paths; only the pre-edit contents are the filesystem.
        if path.ends_with("_after_edit") {
            continue;
        }
        fs.write(path, content);
    }
    let layer = super::hooks::TokenEfficiency::with_probe(settings.clone(), fs.clone());
    let mut history_tokens = fixed_prefix_tokens();

    for (index, step) in task.steps.iter().enumerate() {
        layer.begin_turn(index as u64);

        // The tool-call arguments are part of the prompt, and for `edit` they are
        // where the anchor saving lives.
        let mut args = step.args.clone();
        let mut arg_tokens = estimate_tokens(&baseline_payload(step)) - estimate_tokens(&step.result);

        // ---- before: anchor expansion, search short-circuit ----
        let mut short_circuit: Option<String> = None;
        match super::hooks::intercept(&layer, &step.tool, &args) {
            super::hooks::Before::ShortCircuit(message) => short_circuit = Some(message),
            super::hooks::Before::Rewrite(rewritten) => {
                if step.tool == "edit" {
                    result.anchor_edits += 1;
                    // What the model sends: the anchor header plus the new text.
                    arg_tokens = estimate_tokens(&"x".repeat(anchor_form_size(&step.args)));
                    apply_rewritten_edit(&fs, &rewritten);
                }
                args = rewritten;
            }
            super::hooks::Before::Continue => {}
        }

        let payload = if let Some(message) = short_circuit {
            result.search_stubs += 1;
            message
        } else {
            match super::hooks::observe_result(&layer, &step.tool, &args, &step.result, false) {
                super::hooks::After::Replace(replacement) => {
                    if step.tool == "read" {
                        result.read_stubs += 1;
                    } else {
                        result.minimized += 1;
                    }
                    replacement
                }
                super::hooks::After::Continue => step.result.clone(),
            }
        };

        history_tokens += arg_tokens.max(0) as u64 + estimate_tokens(&payload);
        result.prompt_tokens += history_tokens;
        result.result_bytes += payload.len() as u64;
        result.final_prompt_tokens = history_tokens;
    }
    result
}

/// Bytes the model would emit for an anchor-form edit: the header plus the new
/// text. Compared against the `oldText` the baseline arm re-sends.
fn anchor_form_size(args: &serde_json::Value) -> usize {
    let new_text = args.get("newText").and_then(|v| v.as_str()).unwrap_or("");
    let header = args
        .get("anchorHeader")
        .and_then(|v| v.as_str())
        .unwrap_or("PUT n.==m:");
    header.len() + new_text.len()
}

/// Apply the expanded `edits[]` to the in-memory files so the next read sees the
/// same content the live tool would have produced.
fn apply_rewritten_edit(fs: &FakeFs, rewritten: &serde_json::Value) {
    let Some(path) = rewritten.get("path").and_then(|v| v.as_str()) else {
        return;
    };
    let current = fs
        .read(std::path::Path::new(path))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let mut updated = current;
    if let Some(edits) = rewritten.get("edits").and_then(|v| v.as_array()) {
        for edit in edits {
            let old = edit.get("oldText").and_then(|v| v.as_str()).unwrap_or("");
            let new = edit.get("newText").and_then(|v| v.as_str()).unwrap_or("");
            if !old.is_empty() {
                updated = updated.replacen(old, new, 1);
            }
        }
    }
    fs.write(path, &updated);
}

/// Compare both arms on one task.
pub fn compare(task: &Task, settings: &TokenEfficiencySettings) -> Comparison {
    let baseline = run_baseline(task);
    let optimized = run_optimized(task, settings);
    let prompt_reduction = if baseline.prompt_tokens == 0 {
        0.0
    } else {
        1.0 - optimized.prompt_tokens as f64 / baseline.prompt_tokens as f64
    };
    Comparison {
        task: task.name.clone(),
        baseline,
        optimized,
        prompt_reduction,
    }
}

/// One feature's contribution, measured by switching only that feature off.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Ablation {
    pub feature: &'static str,
    /// Prompt tokens with only this feature disabled.
    pub prompt_tokens: u64,
    /// Tokens this feature is responsible for: `with_only_disabled - fully_optimized`.
    pub contribution: u64,
}

/// Attribute the savings to individual features.
///
/// Each row disables exactly one feature and re-runs; the difference from the
/// full run is that feature's contribution. This is a leave-one-out ablation,
/// not a Shapley decomposition, so with features that interact the rows can
/// sum to more than the total (read dedupe and search dedupe both live inside
/// the same turn window, for example).
pub fn ablate(tasks: &[Task], settings: &TokenEfficiencySettings) -> Vec<Ablation> {
    let fully_optimized: u64 = tasks
        .iter()
        .map(|task| run_optimized(task, settings).prompt_tokens)
        .sum();

    let features: [(&'static str, fn(&mut TokenEfficiencySettings)); 4] = [
        ("read_dedupe", |s| s.read_dedupe = false),
        ("search_cache", |s| s.search_cache = false),
        ("anchor_edits", |s| s.anchor_edits = false),
        ("minimize_output", |s| s.minimize_output = false),
    ];

    features
        .into_iter()
        .map(|(name, disable)| {
            let mut without = settings.clone();
            disable(&mut without);
            let prompt_tokens: u64 = tasks
                .iter()
                .map(|task| run_optimized(task, &without).prompt_tokens)
                .sum();
            Ablation {
                feature: name,
                prompt_tokens,
                contribution: prompt_tokens.saturating_sub(fully_optimized),
            }
        })
        .collect()
}

/// Render the ablation table.
pub fn ablation_report(ablations: &[Ablation], baseline: u64) -> String {
    let mut out = String::new();
    out.push_str("| Feature disabled | Prompt tokens | Contribution | Share of the total saving |\n");
    out.push_str("| --- | ---: | ---: | ---: |\n");
    let total_saving = ablations
        .iter()
        .map(|ablation| ablation.contribution)
        .sum::<u64>()
        .max(1);
    for ablation in ablations {
        out.push_str(&format!(
            "| {} | {} | {} | {:.0}% |\n",
            ablation.feature,
            ablation.prompt_tokens,
            ablation.contribution,
            ablation.contribution as f64 / total_saving as f64 * 100.0,
        ));
    }
    let _ = baseline;
    out
}

/// Load every `*.json` task from `dir`.
pub fn load_tasks(dir: &Path) -> std::io::Result<Vec<Task>> {
    let mut tasks = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path)?;
        let task: Task = serde_json::from_str(&text)?;
        tasks.push(task);
    }
    Ok(tasks)
}

/// Render the report as Markdown.
pub fn report(comparisons: &[Comparison], settings: &TokenEfficiencySettings) -> String {
    let mut out = String::new();
    out.push_str("| Task | Baseline prompt tokens | Optimized | Reduction | Read stubs | Search stubs | Minimized | Anchor edits |\n");
    out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    let mut baseline_total = 0u64;
    let mut optimized_total = 0u64;
    for comparison in comparisons {
        baseline_total += comparison.baseline.prompt_tokens;
        optimized_total += comparison.optimized.prompt_tokens;
        out.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {} | {} | {} | {} |\n",
            comparison.task,
            comparison.baseline.prompt_tokens,
            comparison.optimized.prompt_tokens,
            comparison.prompt_reduction * 100.0,
            comparison.optimized.read_stubs,
            comparison.optimized.search_stubs,
            comparison.optimized.minimized,
            comparison.optimized.anchor_edits,
        ));
    }
    let total_reduction = if baseline_total == 0 {
        0.0
    } else {
        1.0 - optimized_total as f64 / baseline_total as f64
    };
    out.push_str(&format!(
        "| **Total** | **{baseline_total}** | **{optimized_total}** | **{:.1}%** | | | | |\n",
        total_reduction * 100.0
    ));
    out.push_str(&format!(
        "\nFixed prefix: {} estimated tokens per turn. Settings: read_dedupe={}, search_cache={}, \
         anchor_edits={}, minimize_output={}.\n",
        fixed_prefix_tokens(),
        settings.read_dedupe,
        settings.search_cache,
        settings.anchor_edits,
        settings.minimize_output,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_efficiency::fsprobe::testing::FakeFs;

    #[test]
    fn the_estimator_is_monotone_and_nonzero() {
        assert_eq!(estimate_tokens(""), 1, "a single empty line is still a line");
        assert!(estimate_tokens("hello world") > 0);
        assert!(estimate_tokens(&"hello ".repeat(100)) > estimate_tokens(&"hello ".repeat(10)));
    }

    #[test]
    fn the_estimator_separates_code_from_prose_realistically() {
        // A 40-character identifier-heavy line should cost more than a
        // 40-character prose line of short words, because punctuation and long
        // runs split into more tokens.
        let code = "fn handle_request(ctx: &mut Context) -> Result<()> {";
        let prose = "we then look at the thing and go on";
        assert!(
            estimate_tokens(code) > estimate_tokens(prose),
            "code {} vs prose {}",
            estimate_tokens(code),
            estimate_tokens(prose)
        );
    }

    #[test]
    fn repeated_lines_cost_what_they_weigh() {
        let one = "pub fn count(x: usize) -> usize { x }";
        let ten: String = (0..10).map(|_| format!("{one}\n")).collect();
        let ratio = estimate_tokens(&ten) as f64 / estimate_tokens(one) as f64;
        assert!(ratio > 8.0, "ten copies must cost ~ten times one, got {ratio}");
    }

    fn task() -> Task {
        Task {
            name: "t".into(),
            files: BTreeMap::from([("src/a.rs".to_string(), "a\nb\nc\n".to_string())]),
            steps: vec![
                Step {
                    desc: "read".into(),
                    tool: "read".into(),
                    args: serde_json::json!({"path": "src/a.rs"}),
                    result: "a\nb\nc\n".into(),
                },
                Step {
                    desc: "read again".into(),
                    tool: "read".into(),
                    args: serde_json::json!({"path": "src/a.rs"}),
                    result: "a\nb\nc\n".into(),
                },
            ],
        }
    }

    #[test]
    fn baseline_sends_every_result_verbatim() {
        let result = run_baseline(&task());
        assert_eq!(result.read_stubs, 0);
        // Step 2's prompt includes step 1's result again.
        assert!(result.prompt_tokens > fixed_prefix_tokens() * 2);
    }

    #[test]
    fn optimized_stubs_the_second_read() {
        let result = run_optimized(&task(), &TokenEfficiencySettings::default());
        assert_eq!(result.read_stubs, 1);
        assert!(result.prompt_tokens < run_baseline(&task()).prompt_tokens);
    }

    #[test]
    fn a_disabled_layer_matches_the_baseline_exactly() {
        let settings = TokenEfficiencySettings {
            read_dedupe: false,
            search_cache: false,
            anchor_edits: false,
            minimize_output: false,
            ..TokenEfficiencySettings::default()
        };
        let comparison = compare(&task(), &settings);
        assert_eq!(
            comparison.baseline.prompt_tokens, comparison.optimized.prompt_tokens,
            "an inert layer must change nothing"
        );
        assert_eq!(comparison.prompt_reduction, 0.0);
    }

    #[test]
    fn comparison_reports_a_reduction_for_a_repeat_heavy_task() {
        let comparison = compare(&task(), &TokenEfficiencySettings::default());
        assert!(comparison.prompt_reduction > 0.0);
        assert!(comparison.prompt_reduction < 1.0);
    }

    #[test]
    fn fixtures_load_and_every_task_improves() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/token-efficiency/fixtures");
        let tasks = load_tasks(&dir).expect("fixtures must load");
        assert!(tasks.len() >= 2, "the fixed set has at least two tasks");
        for task in &tasks {
            let comparison = compare(task, &TokenEfficiencySettings::default());
            assert!(
                comparison.optimized.prompt_tokens <= comparison.baseline.prompt_tokens,
                "{} regressed: {} > {}",
                task.name,
                comparison.optimized.prompt_tokens,
                comparison.baseline.prompt_tokens
            );
        }
    }

    #[test]
    fn the_report_is_markdown_with_a_total_row() {
        let text = report(&[compare(&task(), &TokenEfficiencySettings::default())],
                          &TokenEfficiencySettings::default());
        assert!(text.starts_with("| Task |"));
        assert!(text.contains("| **Total** |"));
        assert!(text.contains("Fixed prefix:"));
    }

    #[test]
    fn a_realistic_noisy_result_is_measurably_cheaper() {
        let fs = FakeFs::new();
        fs.write("/w/a.rs", "x");
        let settings = TokenEfficiencySettings::default();
        let noisy = Task {
            name: "noise".into(),
            files: BTreeMap::new(),
            steps: vec![Step {
                desc: "build".into(),
                tool: "bash".into(),
                args: serde_json::json!({"command": "cargo build"}),
                result: (0..300)
                    .map(|_| "Compiling shob v0.0.1".to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            }],
        };
        let comparison = compare(&noisy, &settings);
        assert_eq!(comparison.optimized.minimized, 1);
        assert!(comparison.optimized.prompt_tokens < comparison.baseline.prompt_tokens);
    }
}
