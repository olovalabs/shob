// Real foreground lane, deliberately without an RPC-owner poll: the backend
// itself must cancel the root before returning to the sequential agent loop.
mod writer_failure_ordering {
    use super::*;
    use rpi_agent::{AgentTool, ToolResultPartial};
    use rpi_ai::providers::faux::{
        faux_assistant_message, FauxBlock, FauxProvider, FauxScript, FauxStep,
    };
    use rpi_harness::agent_harness::{AgentHarness, AgentLane};
    use rpi_harness::events::{RunEndEvent, RunEndOutcome};
    use rpi_harness::session::types::{LaneRecord, OperationOutcome};
    use rpi_harness::types::{AgentHarnessOptions, HarnessTool, HarnessToolExecution};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Output {
        entered: tokio::sync::mpsc::UnboundedSender<Value>,
        release: std::sync::mpsc::Receiver<bool>,
    }
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.entered.send(serde_json::from_slice(bytes)?).unwrap();
            if self
                .release
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or(true)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "controlled physical output failure",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Dropping the sender unblocks the physical writer even if an assertion fails.
    struct ObservedBackend {
        inner: Arc<ExternalTools>,
        returned: AtomicBool,
        cancelled_at_completion: AtomicBool,
        retired_at_completion: AtomicBool,
    }
    struct ObserveCompletion<'a> {
        backend: &'a ObservedBackend,
        cancel: CancellationToken,
    }
    impl Drop for ObserveCompletion<'_> {
        fn drop(&mut self) {
            self.backend
                .cancelled_at_completion
                .store(self.cancel.is_cancelled(), Ordering::SeqCst);
            // The external-tool wrapper may drop this backend future as soon
            // as owned abort completion cancels the root. Observe after the
            // awaited inner future is retired on BOTH return and cancellation.
            // All 64 slots must be available; checking on Drop must not hide a
            // genuinely leaked request in the losing operation future.
            let slots: Vec<_> = (0..64)
                .map(|_| self.backend.inner.broker.register())
                .collect();
            self.backend
                .retired_at_completion
                .store(slots.iter().all(Result::is_ok), Ordering::SeqCst);
        }
    }
    #[async_trait]
    impl ExternalToolBackend for ObservedBackend {
        async fn execute(
            &self,
            name: &str,
            args: Value,
            cancel: CancellationToken,
        ) -> Result<AgentToolResult, AgentError> {
            let observation = ObserveCompletion {
                backend: self,
                cancel: cancel.clone(),
            };
            let result = self.inner.execute(name, args, cancel).await;
            self.returned.store(true, Ordering::SeqCst);
            drop(observation);
            result
        }
    }
    // Direct control: keep polling the backend through root cancellation,
    // unlike create_external_tool's deliberately cancellation-droppable wrapper.
    struct DirectBackendTool {
        tool: Arc<dyn AgentTool>,
        backend: Arc<ObservedBackend>,
    }
    #[async_trait]
    impl AgentTool for DirectBackendTool {
        fn schema(&self) -> &rpi_ai::types::Tool {
            self.tool.schema()
        }
        fn label(&self) -> &str {
            "owned backend control"
        }
        async fn execute(
            &self,
            _: &str,
            params: Value,
            signal: CancellationToken,
            _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, AgentError> {
            self.backend.execute(&self.schema().name, params, signal).await
        }
    }
    struct DangerousNative {
        schema: rpi_ai::types::Tool,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AgentTool for DangerousNative {
        fn schema(&self) -> &rpi_ai::types::Tool {
            &self.schema
        }
        fn label(&self) -> &str {
            "observable native side effect"
        }
        async fn execute(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
            _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("dangerous native action executed"))
        }
    }
    async fn case(kind: &str, direct: bool) {
        let script = FauxScript::new();
        script.set_responses(vec![
            FauxStep::factory(|_| {
                faux_assistant_message(
                    vec![
                        FauxBlock::tool_call("external_test", json!({})),
                        FauxBlock::tool_call("dangerous_native", json!({})),
                    ],
                    rpi_ai::types::StopReason::ToolUse,
                )
            }),
            FauxStep::factory(|_| {
                faux_assistant_message("unexpected continuation", rpi_ai::types::StopReason::Stop)
            }),
        ]);
        let provider = FauxProvider::new(script);
        let harness = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            tool_execution: HarnessToolExecution::Sequential,
            ..Default::default()
        })
        .await
        .unwrap();
        let lane: Arc<dyn AgentLane> = harness.lane("main");
        let (entered, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (writer, task, failure) = super::super::super::writer::start_rpc_writer(
            Output {
                entered,
                release: gate,
            },
            Default::default(),
        );
        let inner = ExternalTools::new(writer.clone(), Arc::downgrade(&lane));
        let backend = Arc::new(ObservedBackend {
            inner: inner.clone(),
            returned: AtomicBool::new(false),
            cancelled_at_completion: AtomicBool::new(false),
            retired_at_completion: AtomicBool::new(false),
        });
        let native_calls = Arc::new(AtomicUsize::new(0));
        let external_tool = create_external_tool(
            ExternalToolDefinition {
                name: "external_test".into(),
                description: "test".into(),
                input_schema: json!({"type":"object"}),
            },
            backend.clone(),
        );
        let external_tool: Arc<dyn AgentTool> = if direct {
            Arc::new(DirectBackendTool { tool: external_tool, backend: backend.clone() })
        } else {
            external_tool
        };
        harness
            .set_tools(
                vec![
                    HarnessTool::new(external_tool),
                    HarnessTool::new(Arc::new(DangerousNative {
                        schema: rpi_ai::types::Tool {
                            name: "dangerous_native".into(),
                            description: "test".into(),
                            parameters: rpi_ai::types::Schema::new(json!({"type":"object"})),
                            constrained_sampling: None,
                        },
                        calls: native_calls.clone(),
                    })),
                ],
                None,
            )
            .await
            .unwrap();
        let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = ends.clone();
        let _watch = harness
            .events()
            .on::<RunEndEvent, _>(move |end| observed.lock().unwrap().push(end.clone()));
        if kind == "enqueue" {
            super::super::super::writer::write_line(&writer, &json!("block")).await;
            tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap();
            for n in 0..64 {
                super::super::super::writer::write_line(&writer, &json!(n)).await;
            }
        }
        let run = tokio::spawn(async move { lane.prompt_text("test", vec![]).await });
        if kind != "enqueue" {
            let request = tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request["type"], "external_tool_request");
            if kind == "physical" {
                release.send(true).unwrap();
            } else {
                for n in 0..64 {
                    super::super::super::writer::write_line(&writer, &json!(n)).await;
                }
                assert!(inner
                    .respond(&json!({"requestId":request["requestId"], "result":{"content":[]}}))
                    .unwrap());
            }
        }
        let finished = tokio::time::timeout(Duration::from_secs(3), run).await;
        // Measure real request retirement BEFORE shutdown's cancel_all can hide
        // a leak. The run has settled (or the timeout is reported below).
        let slots: Vec<_> = (0..64).map(|_| inner.broker.register()).collect();
        let available = slots.iter().filter(|slot| slot.is_ok()).count();
        drop(slots);
        let returned = backend.returned.load(Ordering::SeqCst);
        eprintln!("writer failure kind={kind} direct={direct} backend_returned={returned} broker_slots_after_settlement={available}/64");
        // Release blocked OS IO before making assertions; failure stops queue draining.
        drop(release);
        inner.shutdown();
        drop(writer);
        let writer_finished = tokio::time::timeout(Duration::from_secs(3), task).await;
        finished
            .expect("foreground must settle boundedly")
            .unwrap()
            .unwrap();
        assert!(writer_finished.unwrap().unwrap().is_err());
        assert!(
            failure.is_cancelled(),
            "must originate from output, not invalid input"
        );
        assert_eq!(available, 64, "request leaked after foreground settlement");
        if direct {
            assert!(returned, "owned direct control must return through the backend");
        }
        assert!(
            backend.retired_at_completion.load(Ordering::SeqCst),
            "pending request leaked at backend return/drop"
        );
        assert_eq!(
            native_calls.load(Ordering::SeqCst),
            0,
            "second sequential native tool ran after writer failure"
        );
        assert_eq!(
            provider.state().call_count.load(Ordering::SeqCst),
            1,
            "provider continued after writer failure"
        );
        assert!(
            backend.cancelled_at_completion.load(Ordering::SeqCst),
            "root must be cancelled before backend returns or is dropped (tool signal is a child)"
        );
        let end = ends.lock().unwrap().last().unwrap().clone();
        assert_eq!(end.outcome, RunEndOutcome::Aborted);
        let records = harness
            .session()
            .find_records(&Default::default())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(record, LaneRecord::OperationFinished(f) if f.run_id == end.run_id && f.outcome == OperationOutcome::Aborted)), "{records:?}");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_failure_enqueue_cancels_root_before_backend_return() {
        case("enqueue", false).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_failure_physical_cancels_root_before_backend_return() {
        case("physical", false).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_failure_closure_cancels_root_before_backend_return() {
        case("closure", false).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_failure_owned_backend_controls_cancel_root_before_return() {
        for kind in ["enqueue", "physical", "closure"] {
            case(kind, true).await;
        }
    }
}
