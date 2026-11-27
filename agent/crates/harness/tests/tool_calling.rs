//! Run the commands below from the agent workspace.
//! Offline replay: cargo test -p harness --test tool_calling
//! Live: set OPENAI_API_KEY, ANTHROPIC_API_KEY, SHOB_EVAL_OPENAI_MODEL and
//! SHOB_EVAL_ANTHROPIC_MODEL, then run:
//! cargo test -p harness --test tool_calling live_comparison -- --ignored --nocapture

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::{AgentEvent, AgentMessage, CollectorEmitter};
use ai::providers::anthropic::AnthropicProvider;
use ai::providers::faux::{FauxProvider, FauxScript, FauxStep};
use ai::providers::openai_responses::{openai_responses_models, OpenAiResponsesProvider};
use ai::{
    Api, AssistantMessageEventStream, Content, Context, Model, Provider, SimpleStreamOptions,
};
use harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use harness::types::{AgentHarnessOptions, AgentHarnessStreamOptions, HarnessTool, RetryPolicy};
use serde::Serialize;
use serde_json::{json, Value};
use tools::{
    create_bash_tool, create_edit_tool, create_find_tool, create_grep_tool, create_ls_tool,
    create_read_tool, create_write_tool, ExecutionToolContext, FileContent, FileSystem,
    InMemoryExecutionEnv, ShellScript,
};

const SOURCE: &str = "fn first() {\n    target();\n}\nfn second() {\n    target();\n}\n";
const UPDATED: &str = "fn first() {\n    target();\n}\nfn second() {\n    replacement();\n}\n";
const PATH: &str = "src/lib.rs";
const MAX_REQUESTS: usize = 8;
const SAMPLES: usize = 3;
const BUILD: &str = "cargo build --locked";

struct Case {
    name: &'static str,
    prompt: &'static str,
    seed: Option<(&'static str, Value)>,
    expected: Vec<(&'static str, Value)>,
    edits_file: bool,
    control: bool,
}

fn cases() -> Vec<Case> {
    let simple = |name, prompt, tool, args| Case {
        name,
        prompt,
        seed: None,
        expected: vec![(tool, args)],
        edits_file: false,
        control: false,
    };
    let recovery = |name, seed, expected, edits_file| Case {
        name,
        prompt: if edits_file {
            "In src/lib.rs, change target() to replacement() in second() only. Finish the edit."
        } else {
            "Show the contents of src/lib.rs."
        },
        seed: Some(seed),
        expected,
        edits_file,
        control: false,
    };
    let read = || ("read", json!({"path": PATH}));
    let edit = |old: &str, new: &str| {
        (
            "edit",
            json!({"path": PATH, "edits": [{"oldText": old, "newText": new}]}),
        )
    };
    let recover_edit = || {
        vec![
            read(),
            edit(
                "fn second() {\n    target();",
                "fn second() {\n    replacement();",
            ),
        ]
    };
    vec![
        simple(
            "filenames",
            "Find all Rust source filenames under src.",
            "find",
            json!({"path": "src", "pattern": "**/*.rs"}),
        ),
        simple(
            "contents",
            "Find the lines in src containing the literal text target().",
            "grep",
            json!({"path": "src", "pattern": "target()", "literal": true}),
        ),
        Case {
            control: true,
            ..simple(
                "directory",
                "List the immediate entries in src.",
                "ls",
                json!({"path": "src"}),
            )
        },
        Case {
            name: "read_pages",
            prompt: "Show lines 1 through 4 of src/lib.rs, reading two lines per call.",
            seed: None,
            expected: vec![
                ("read", json!({"path": PATH, "offset": 1, "limit": 2})),
                ("read", json!({"path": PATH, "offset": 3, "limit": 2})),
            ],
            edits_file: false,
            control: false,
        },
        recovery(
            "stale_edit",
            edit("fn second() {\n    outdated();", "replacement();"),
            recover_edit(),
            true,
        ),
        recovery(
            "ambiguous_edit",
            edit("    target();", "    replacement();"),
            recover_edit(),
            true,
        ),
        recovery(
            "invalid_arguments",
            ("read", json!({"path": ["fixture-input-sentinel"]})),
            vec![read()],
            false,
        ),
        recovery(
            "unknown_tool",
            ("raed", json!({"path": PATH})),
            vec![read()],
            false,
        ),
        Case {
            control: true,
            ..simple(
                "build",
                "Run cargo build --locked and report the result.",
                "bash",
                json!({"command": BUILD}),
            )
        },
    ]
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Wording {
    Baseline,
    Candidate,
}

impl Wording {
    fn apply(self, context: &mut Context) {
        if matches!(self, Self::Baseline) {
            return;
        }
        for tool in &mut context.tools {
            let guidance = match tool.name.as_str() {
                "find" => "Find filenames by glob. Use grep to find matching text inside files.",
                "grep" => "Find matching text inside files. Use find to match filenames.",
                "ls" => "List immediate directory entries. Use find to match filenames recursively.",
                "read" => "Read a known file or line range. Use find or grep to locate an unknown path.",
                "bash" => "Run commands and pipelines such as builds, tests, and Git. Use find, grep, ls, or read for direct repository inspection.",
                "edit" => "If oldText is stale or ambiguous, read the target lines again and include enough surrounding text for one unique match.",
                _ => continue,
            };
            tool.description = format!("{guidance} {}", tool.description);
            let (field, detail) = match tool.name.as_str() {
                "read" => ("/properties/offset",
                    "One-based start line. Omit for line 1. Continue at the line after the last line returned."),
                "grep" => ("/properties/offset",
                    "Result offset. To continue, use the returned next_offset and keep the other arguments unchanged."),
                "edit" => ("/definitions/ReplaceEditJson/properties/oldText",
                    "Exact current text from the target region. After a stale or ambiguous match, reread the region and include unique surrounding lines."),
                _ => continue,
            };
            let parameter = tool
                .parameters
                .0
                .pointer_mut(field)
                .expect("built-in tool parameter");
            parameter["description"] = json!(detail);
        }
    }
}

struct EvalProvider {
    inner: Arc<dyn Provider>,
    seed: Mutex<Option<Arc<FauxProvider>>>,
    wording: Wording,
    requests: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for EvalProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn models(&self) -> &[Model] {
        self.inner.models()
    }

    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let seed = self.seed.lock().unwrap().take();
        if let Some(seed) = seed {
            return seed.stream_simple(model, context, options).await;
        }
        let request = self.requests.fetch_add(1, Ordering::Relaxed);
        if request >= MAX_REQUESTS {
            let script = FauxScript::new();
            let mut message =
                ai::providers::faux::faux_assistant_message("", ai::types::StopReason::Error);
            message.error_message = Some("Tool eval request limit reached".into());
            script.set_responses(vec![FauxStep::message(message)]);
            return FauxProvider::new(script)
                .stream_simple(model, context, options)
                .await;
        }
        let mut context = context.clone();
        self.wording.apply(&mut context);
        let mut options = options.clone();
        options.max_tokens = Some(4096);
        self.inner.stream_simple(model, &context, &options).await
    }
}

#[derive(Debug, Serialize)]
struct Metrics {
    first_choice_correct: bool,
    rejected_arguments: usize,
    recovered: Option<bool>,
    tool_calls: usize,
    input_tokens: i64,
    model_requests: usize,
    hit_request_limit: bool,
    provider_error: Option<String>,
    passed: bool,
}

struct ToolOutcome {
    id: String,
    name: String,
    arguments: Value,
    passed_validation: bool,
    result: Option<(bool, String)>,
}

async fn evaluate(
    case: &Case,
    inner: Arc<dyn Provider>,
    wording: Wording,
) -> (Metrics, Vec<ToolOutcome>) {
    let env = Arc::new(InMemoryExecutionEnv::new());
    env.write_file(PATH, FileContent::Text(SOURCE.into()), None)
        .await
        .unwrap();
    env.write_file(
        "src/other.rs",
        FileContent::Text("fn other() {}\n".into()),
        None,
    )
    .await
    .unwrap();
    env.write_file(
        "examples/decoy.rs",
        FileContent::Text("fn outside_src() { target(); }\n".into()),
        None,
    )
    .await
    .unwrap();
    env.register_shell(BUILD, ShellScript::success("Finished dev profile"))
        .await;
    let context = ExecutionToolContext::new(env.clone(), Some(env.clone()));
    let tools = [
        create_read_tool(&context, None),
        create_find_tool(&context, None),
        create_grep_tool(&context, None),
        create_ls_tool(&context, None),
        create_edit_tool(&context),
        create_write_tool(&context),
        create_bash_tool(&context, None),
    ]
    .into_iter()
    .map(HarnessTool::new)
    .collect::<Vec<_>>();

    let model = inner.models()[0].clone();
    let provider = Arc::new(EvalProvider {
        inner,
        seed: Mutex::new(case.seed.as_ref().map(|(name, args)| {
            FauxProvider::new(FauxScript::new().with_tool_call(*name, args.clone()))
        })),
        wording,
        requests: AtomicUsize::new(0),
    });
    let (collector, events) = CollectorEmitter::new();
    let validated = Arc::new(Mutex::new(Vec::new()));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model,
        active_tool_names: tools
            .iter()
            .map(|tool| tool.tool.schema().name.clone())
            .collect(),
        tools,
        models: vec![provider.clone()],
        retry: RetryPolicy {
            enabled: false,
            ..Default::default()
        },
        stream_options: AgentHarnessStreamOptions {
            max_retries: Some(0),
            cache_retention: Some(ai::CacheRetention::None),
            timeout: Some(Duration::from_secs(120)),
            ..Default::default()
        },
        agent_emitter: Some(Arc::new(collector)),
        before_tool_call: Some(Arc::new({
            let validated = validated.clone();
            move |context, _| {
                validated.lock().unwrap().push(context.tool_call.id.clone());
                Box::pin(async { None })
            }
        })),
        ..Default::default()
    })
    .await
    .unwrap();
    let outcome = harness
        .prompt_text(case.prompt, vec![])
        .await
        .unwrap()
        .outcome;
    let file = env.read_text_file(PATH, None).await.unwrap();
    let validated = validated.lock().unwrap();
    let events = events.lock().unwrap();
    let mut calls: Vec<ToolOutcome> = Vec::new();
    let mut input_tokens = 0;
    for event in events.iter() {
        match event {
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(message),
                ..
            } => {
                input_tokens +=
                    message.usage.input + message.usage.cache_read + message.usage.cache_write;
                calls.extend(message.content.iter().filter_map(|block| {
                    let Content::ToolCall(call) = block else {
                        return None;
                    };
                    Some(ToolOutcome {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        passed_validation: validated.contains(&call.id),
                        result: None,
                    })
                }));
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(result),
                ..
            } => {
                let call = calls
                    .iter_mut()
                    .rev()
                    .find(|call| {
                        call.id == result.tool_call_id
                            && call.name == result.tool_name
                            && call.result.is_none()
                    })
                    .expect("result has an originating call");
                call.result = Some((!result.is_error, Content::text_only(&result.content, "\n")));
            }
            _ => {}
        }
    }
    let file_correct = file == if case.edits_file { UPDATED } else { SOURCE };
    let actual = &calls[usize::from(case.seed.is_some()).min(calls.len())..];
    let first_choice_correct = actual
        .first()
        .is_some_and(|call| call.name == case.expected[0].0);
    let mut remaining = actual.iter();
    let completed_tools = case
        .expected
        .iter()
        .all(|(name, args)| remaining.any(|call| call_matches(call, name, args)));
    let requests = provider.requests.load(Ordering::Relaxed);
    let hit_request_limit = requests > MAX_REQUESTS;
    let provider_error = match &outcome {
        HarnessRunOutcome::Failed { error, .. } if !hit_request_limit => Some(error.code.clone()),
        _ => None,
    };
    let seed_failed = calls
        .first()
        .and_then(|call| call.result.as_ref())
        .is_some_and(|(ok, _)| !ok);
    let passed = first_choice_correct
        && completed_tools
        && file_correct
        && (case.seed.is_none() || seed_failed)
        && matches!(outcome, HarnessRunOutcome::Completed { .. });
    let metrics = Metrics {
        first_choice_correct,
        rejected_arguments: calls
            .iter()
            .filter(|call| {
                !call.passed_validation
                    && call
                        .result
                        .as_ref()
                        .is_some_and(|(_, output)| output.contains("Invalid arguments for"))
            })
            .count(),
        recovered: case.seed.as_ref().map(|_| passed),
        tool_calls: calls.len(),
        input_tokens,
        model_requests: requests.min(MAX_REQUESTS),
        hit_request_limit,
        provider_error,
        passed,
    };
    (metrics, calls)
}

fn call_matches(call: &ToolOutcome, name: &str, expected: &Value) -> bool {
    let Some((true, output)) = &call.result else {
        return false;
    };
    if call.name != name {
        return false;
    }
    match name {
        "find" => {
            output.contains("lib.rs") && output.contains("other.rs") && !output.contains("decoy.rs")
        }
        "grep" => output.matches("target()").count() == 2 && output.contains("lib.rs"),
        "edit" => call.arguments["path"]
            .as_str()
            .is_some_and(|path| path.trim_start_matches("./") == PATH),
        _ => expected.as_object().unwrap().iter().all(|(key, value)| {
            let actual = &call.arguments[key];
            if key == "path" {
                return actual
                    .as_str()
                    .map(|path| path.trim_start_matches("./").trim_end_matches('/'))
                    == value.as_str();
            }
            actual == value || (key == "offset" && value == 1 && actual.is_null())
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_replay() {
    for case in cases() {
        for wording in [Wording::Baseline, Wording::Candidate] {
            let mut script = case
                .expected
                .iter()
                .fold(FauxScript::new(), |script, (tool, args)| {
                    script.with_tool_call(*tool, args.clone())
                });
            script = script.with_text("Done.").with_usage_estimate();
            let (metrics, calls) = evaluate(&case, FauxProvider::new(script), wording).await;
            assert!(
                metrics.passed,
                "{} {wording:?}: {metrics:?}, results: {:?}",
                case.name,
                calls.iter().map(|call| &call.result).collect::<Vec<_>>()
            );
            assert!(metrics.first_choice_correct);
            assert!(metrics.input_tokens > 0);
            assert_eq!(metrics.model_requests, case.expected.len() + 1);
            assert_eq!(
                metrics.tool_calls,
                case.expected.len() + usize::from(case.seed.is_some())
            );
            if case.seed.is_some() {
                assert_eq!(metrics.recovered, Some(true));
                let seed = &calls[0];
                let (_, output) = seed.result.as_ref().unwrap();
                match case.name {
                    "invalid_arguments" => {
                        assert!(!seed.passed_validation);
                        assert_eq!(metrics.rejected_arguments, 1);
                        assert!(output.contains("path") && output.contains("Correct these fields"));
                        assert!(!output.contains("fixture-input-sentinel"));
                    }
                    "unknown_tool" => {
                        assert!(!seed.passed_validation);
                        assert!(output.contains("Did you mean \"read\"?"), "{output}");
                    }
                    _ => assert!(seed.passed_validation),
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_limit_stops_without_contacting_the_provider_again() {
    let case = cases()
        .into_iter()
        .find(|case| case.name == "read_pages")
        .unwrap();
    let script = (1..=MAX_REQUESTS + 1).fold(FauxScript::new(), |script, offset| {
        script.with_tool_call("read", json!({"path": PATH, "offset": offset, "limit": 1}))
    });
    let provider = FauxProvider::new(script);
    let (metrics, _) = evaluate(&case, provider.clone(), Wording::Baseline).await;
    assert!(metrics.hit_request_limit);
    assert!(metrics.provider_error.is_none());
    assert!(!metrics.passed);
    assert_eq!(
        provider.state().call_count.load(Ordering::Relaxed),
        MAX_REQUESTS
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "sends synthetic fixtures to paid providers"]
async fn live_comparison() {
    let required = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| panic!("Set {name} to run the opt-in tool eval"))
    };
    let openai_id = required("SHOB_EVAL_OPENAI_MODEL");
    let anthropic_id = required("SHOB_EVAL_ANTHROPIC_MODEL");
    let openai_key = required("OPENAI_API_KEY");
    let anthropic_key = required("ANTHROPIC_API_KEY");

    let mut openai_model = openai_responses_models().remove(0);
    openai_model.id = openai_id;
    let anthropic_model = ai::providers::anthropic::models::get_model(&anthropic_id)
        .unwrap_or_else(|| {
            Model::new(
                &anthropic_id,
                &anthropic_id,
                Api::AnthropicMessages,
                "anthropic",
                "https://api.anthropic.com",
            )
        });
    let http = reqwest::Client::new();
    let providers: [Arc<dyn Provider>; 2] = [
        Arc::new(OpenAiResponsesProvider::with_models(
            "openai",
            Some(openai_key),
            http.clone(),
            vec![openai_model],
        )),
        Arc::new(AnthropicProvider::with_models(
            Some(anthropic_key),
            http,
            vec![anthropic_model],
        )),
    ];

    let mut improved = false;
    let mut regressed = false;
    let mut incomplete = false;
    for provider in providers {
        let model = &provider.models()[0];
        for case in cases() {
            let mut passes = [0; 2];
            let mut calls = [Vec::new(), Vec::new()];
            for sample in 1..=SAMPLES {
                for (arm, wording) in [Wording::Baseline, Wording::Candidate]
                    .into_iter()
                    .enumerate()
                {
                    let (metrics, _) = evaluate(&case, provider.clone(), wording).await;
                    println!(
                        "{}",
                        json!({
                            "provider": provider.id(), "model": model.id, "case": case.name,
                            "wording": wording, "sample": sample, "metrics": metrics
                        })
                    );
                    if metrics.provider_error.is_some() {
                        incomplete = true;
                        continue;
                    }
                    passes[arm] += usize::from(metrics.passed);
                    calls[arm].push(metrics.tool_calls);
                }
            }
            if calls.iter().any(|arm| arm.len() != SAMPLES) {
                continue;
            }
            calls.iter_mut().for_each(|counts| counts.sort_unstable());
            improved |= !case.control && passes[1] > passes[0];
            regressed |= passes[1] < passes[0]
                || (case.control && calls[1][SAMPLES / 2] > calls[0][SAMPLES / 2]);
        }
    }
    println!(
        "{}",
        json!({
            "candidate_clears_gate": !incomplete && improved && !regressed,
            "improved": improved, "regressed": regressed, "incomplete": incomplete
        })
    );
}
