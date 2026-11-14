//! M5g integration — defensive-copy contract for the `AgentHarness` tool
//! registry + active-tool-name config. Mirrors the TS
//! `agent-harness.test.ts` "defensive copy" cases (setters Clone inputs,
//! getters return clones — mutating a returned reference never reaches the
//! harness's internal state) and the `setTools` default-`activeNames` rule.
//!
//! These exercise the PUBLIC harness surface:
//! - `AgentHarness::get_tools` / `set_tools` (harness-level, TS
//!   `AgentHarness.getTools`/`setTools` — NOT on the `AgentLane` interface).
//! - `AgentLane::get_active_tools` / `set_active_tools` (lane-level).
//! - `AgentHarness::close` then rejection with `HarnessError::Closed`.
//!
//! A hand-rolled `DummyTool` (configurable schema name) stands in for a real
//! tool so the test controls the name space (`alpha`/`beta`/`gamma`) and can
//! assert identity via `Arc::ptr_eq` across `get_tools` calls.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolResultPartial};
use rpi_ai::types::{Schema, Tool};
use rpi_harness::agent_harness::{AgentHarness, AgentLane};
use rpi_harness::result::HarnessError;
use rpi_harness::types::{AgentHarnessOptions, HarnessTool, ToolReplay};
use tokio_util::sync::CancellationToken;

/// A trivial `AgentTool` whose only configurable field is the schema name. Used
/// so the test can mint distinct tool identities (`alpha`/`beta`/`gamma`) and
/// recognize them by `Arc::ptr_eq` against the `Arc<dyn AgentTool>` the harness
/// stores. Returns `Arc<dyn AgentTool>` so it drops straight into
/// `HarnessTool::new` and compares with `Arc::ptr_eq`.
struct DummyTool {
    schema: Tool,
    label: &'static str,
}

impl DummyTool {
    fn new(name: &'static str) -> Arc<dyn AgentTool> {
        Arc::new(Self {
            schema: Tool {
                name: name.to_string(),
                description: format!("dummy {name} tool"),
                parameters: Schema::empty_object(),
                constrained_sampling: None,
            },
            label: name,
        })
    }
}

#[async_trait]
impl AgentTool for DummyTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        self.label
    }
    async fn execute(
        &self,
        _id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        Ok(AgentToolResult::default())
    }
}

/// Empty `AgentHarnessOptions` with a fresh in-memory session + the given
/// tools pre-installed and `active_tool_names` defaulted to every tool's
/// schema name. Mirrors the TS harness `create` default.
fn options_with(tools: Vec<HarnessTool>) -> AgentHarnessOptions {
    let active = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
    AgentHarnessOptions {
        active_tool_names: active,
        tools,
        ..AgentHarnessOptions::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tools_returns_clone_independent_of_internal() {
    let alpha = DummyTool::new("alpha");
    let tools = vec![
        HarnessTool::new(alpha.clone()),
        HarnessTool::new(DummyTool::new("beta")),
    ];
    let harness = AgentHarness::create(options_with(tools))
        .await
        .expect("create");

    let got = harness.get_tools().await.expect("get_tools");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].tool.schema().name, "alpha");
    assert_eq!(got[1].tool.schema().name, "beta");
    // Identity preserved across the clone boundary (same Arc allocation).
    assert!(
        Arc::ptr_eq(&got[0].tool, &alpha),
        "get_tools must return the same Arc<dyn AgentTool>, not a re-built tool"
    );

    // Mutate the returned Vec (clear it). The internal registry is behind a
    // Mutex and must be unaffected — re-query and confirm.
    let mut mutated = got;
    mutated.clear();

    let again = harness.get_tools().await.expect("get_tools again");
    assert_eq!(
        again.len(),
        2,
        "clearing the returned Vec must not reach internal state"
    );
    assert_eq!(again[0].tool.schema().name, "alpha");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_tools_none_resets_active_names_to_all_schema_names() {
    let harness = AgentHarness::create(options_with(vec![HarnessTool::new(DummyTool::new(
        "alpha",
    ))]))
    .await
    .expect("create");

    // Replace with a distinct set; pass `None` for active names → harness must
    // reset active_tool_names to every new tool's schema name (TS `setTools`
    // default), NOT keep the old "alpha".
    let gamma = DummyTool::new("gamma");
    let beta = DummyTool::new("beta");
    harness
        .set_tools(
            vec![
                HarnessTool::new(beta.clone()),
                HarnessTool::new(gamma.clone()),
            ],
            None,
        )
        .await
        .expect("set_tools");

    let tools = harness.get_tools().await.expect("get_tools");
    assert_eq!(tools.len(), 2);
    assert!(Arc::ptr_eq(&tools[0].tool, &beta));
    assert!(Arc::ptr_eq(&tools[1].tool, &gamma));

    let active = <AgentHarness as AgentLane>::get_active_tools(&harness)
        .await
        .expect("get_active_tools");
    assert_eq!(active, vec!["beta".to_string(), "gamma".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_tools_some_pins_active_names_exactly() {
    let harness = AgentHarness::create(options_with(vec![]))
        .await
        .expect("create");

    let alpha = DummyTool::new("alpha");
    let beta = DummyTool::new("beta");
    harness
        .set_tools(
            vec![HarnessTool::new(alpha), HarnessTool::new(beta)],
            Some(vec!["beta".to_string()]),
        )
        .await
        .expect("set_tools");

    let active = <AgentHarness as AgentLane>::get_active_tools(&harness)
        .await
        .expect("get_active_tools");
    assert_eq!(active, vec!["beta".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_and_set_active_tools_are_defensive_copies() {
    let alpha = DummyTool::new("alpha");
    let harness = AgentHarness::create(options_with(vec![HarnessTool::new(alpha)]))
        .await
        .expect("create");

    let got = <AgentHarness as AgentLane>::get_active_tools(&harness)
        .await
        .expect("get");
    assert_eq!(got, vec!["alpha".to_string()]);

    // Mutate the returned Vec; internal must be unchanged.
    let mut mutated = got;
    mutated.push("sneaky".to_string());
    let _ = mutated;

    let again = <AgentHarness as AgentLane>::get_active_tools(&harness)
        .await
        .expect("get again");
    assert_eq!(again, vec!["alpha".to_string()]);

    // set_active_tools moves the Vec in; re-get returns that exact set as a clone.
    <AgentHarness as AgentLane>::set_active_tools(
        &harness,
        vec!["alpha".to_string(), "extra".to_string()],
    )
    .await
    .expect("set_active_tools");
    let after = <AgentHarness as AgentLane>::get_active_tools(&harness)
        .await
        .expect("get");
    assert_eq!(after, vec!["alpha".to_string(), "extra".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_rejects_subsequent_tool_ops_with_closed() {
    let harness = AgentHarness::create(options_with(vec![HarnessTool::new(DummyTool::new(
        "alpha",
    ))]))
    .await
    .expect("create");

    harness.close().await.expect("close");
    // After close, every tool-registry op rejects with HarnessError::Closed.
    // (`HarnessTool` is not `Debug` — it wraps `Arc<dyn AgentTool>` — so we
    // match rather than `expect_err`, which would require the Ok variant to be
    // `Debug`.)
    match harness.get_tools().await {
        Err(e) => assert!(matches!(e, HarnessError::Closed { .. }), "got {e:?}"),
        Ok(_) => panic!("get_tools after close should reject with Closed"),
    }
    match harness
        .set_tools(vec![HarnessTool::new(DummyTool::new("beta"))], None)
        .await
    {
        Err(e) => assert!(matches!(e, HarnessError::Closed { .. }), "got {e:?}"),
        Ok(_) => panic!("set_tools after close should reject with Closed"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_tool_replay_flag_round_trips() {
    // The defensive-copy surface also covers the `ToolReplay` flag carried on
    // each `HarnessTool`. set/get must preserve it (clone, not strip).
    let alpha = DummyTool::new("alpha");
    let harness = AgentHarness::create(options_with(vec![
        HarnessTool::new(alpha.clone()).with_replay(ToolReplay::Never)
    ]))
    .await
    .expect("create");

    let got = harness.get_tools().await.expect("get_tools");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].replay, ToolReplay::Never);
    assert!(Arc::ptr_eq(&got[0].tool, &alpha));
}
