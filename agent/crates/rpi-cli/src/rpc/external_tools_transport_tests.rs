// Dispatcher-level witnesses: unlike framing/writer unit tests these drive rpc_with_io.
mod external_transport {
    use super::*;
    use std::sync::{Condvar, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    const LIMIT: Duration = Duration::from_secs(3);

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_transport_rejects_invalid_utf8_and_oversized_input() {
        for input in [
            vec![0xff, b'\n'],
            vec![b' '; crate::rpc::protocol::MAX_FRAME_BYTES + 1],
        ] {
            let (harness, _) = rpc_test_harness("unused", 0.0).await;
            // Slice input avoids a test-side duplex write blocking after rejection.
            let output = RpcOutput::default();
            let code = tokio::time::timeout(
                LIMIT,
                rpc_with_io(
                    &harness,
                    None,
                    vec![],
                    rpi_extensions::UiDialogMailbox::new(),
                    None,
                    crate::args::RpcMessageUpdates::default(),
                    tokio::io::BufReader::with_capacity(127, input.as_slice()),
                    output,
                ),
            )
            .await
            .expect("invalid input must terminate the actual dispatcher");
            assert_ne!(code, 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_transport_ping_accepts_blank_crlf_and_final_eof_frame() {
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let output = RpcOutput::default();
        let code = tokio::time::timeout(
            LIMIT,
            rpc_with_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(
                    &b"\n \t\r\n{\"id\":1,\"type\":\"ping\"}\r\n{\"id\":2,\"type\":\"ping\"}"[..],
                ),
                output.clone(),
            ),
        )
        .await
        .expect("healthy EOF must complete");
        assert_eq!(code, 0);
        let messages = output.messages();
        let replies: Vec<_> = messages
            .iter()
            .filter(|m| m["type"] == "response")
            .collect();
        assert_eq!(replies.len(), 2, "{messages:?}");
        for (reply, id) in replies.iter().zip([1, 2]) {
            assert_eq!(reply["id"], id);
            assert_eq!(reply["success"], true);
            assert_eq!(reply["data"]["pong"], true);
        }
    }

    #[derive(Default)]
    struct Gate {
        // Armed only after observing the external request on the real wire.
        armed: StdMutex<bool>,
        released: Condvar,
        entered: tokio::sync::Notify,
        dropped: tokio::sync::Notify,
    }

    // Both explicit release and unwinding release an in-progress OS-style write.
    // The finite fallback also protects the runtime if the test future is cancelled.
    struct ReleaseOnDrop(Arc<Gate>);
    impl ReleaseOnDrop {
        fn release(&self) {
            *self.0.armed.lock().unwrap() = false;
            self.0.released.notify_all();
        }
    }
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.release();
        }
    }

    struct GatedOutput {
        output: RpcOutput,
        gate: Arc<Gate>,
    }
    impl Write for GatedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let armed = self.gate.armed.lock().unwrap();
            if *armed {
                self.gate.entered.notify_one();
                let (armed, timeout) = self
                    .gate
                    .released
                    .wait_timeout_while(armed, Duration::from_secs(5), |armed| *armed)
                    .unwrap();
                if timeout.timed_out() && *armed {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "finite test stdout gate expired",
                    ));
                }
            }
            self.output.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Drop for GatedOutput {
        fn drop(&mut self) {
            self.gate.dropped.notify_one();
        }
    }

    async fn pending_with_pumps() -> (
        AgentHarness,
        tokio::io::DuplexStream,
        RpcOutput,
        tokio::task::JoinHandle<i32>,
        rpi_extensions::UiDialogMailbox,
        Arc<rpi_agent::events::BroadcastEmitter>,
        ReleaseOnDrop,
    ) {
        let harness = external_pending_harness().await;
        let (emitter, agent_events_rx) = rpi_agent::events::BroadcastEmitter::new(256);
        let emitter = Arc::new(emitter);
        harness
            .set_agent_emitter(Some(emitter.clone()))
            .await
            .unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (client, input) = tokio::io::duplex(4096);
        let output = RpcOutput::default();
        let gate = Arc::new(Gate::default());
        let writer = GatedOutput {
            output: output.clone(),
            gate: gate.clone(),
        };
        let retained = harness.clone();
        let mailbox = ui.clone();
        let task = tokio::spawn(async move {
            rpc_with_io(
                &retained,
                Some(agent_events_rx),
                vec![],
                mailbox,
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input),
                writer,
            )
            .await
        });
        (
            harness,
            client,
            output,
            task,
            ui,
            emitter,
            ReleaseOnDrop(gate),
        )
    }

    async fn block_next_write(client: &mut tokio::io::DuplexStream, release: &ReleaseOnDrop) {
        *release.0.armed.lock().unwrap() = true;
        client
            .write_all(b"{\"id\":9,\"type\":\"ping\"}\n")
            .await
            .unwrap();
        tokio::time::timeout(LIMIT, release.0.entered.notified())
            .await
            .expect("writer must actually enter the controlled blocking write");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_transport_blocked_stdout_failure_persists_aborted_operation() {
        use rpi_harness::events::{RunEndEvent, RunEndOutcome};
        use rpi_harness::session::types::{LaneRecord, OperationOutcome};

        let (harness, mut client, output, mut task, ui, emitter, release) =
            pending_with_pumps().await;
        let ends = Arc::new(StdMutex::new(Vec::new()));
        let observed = ends.clone();
        let _watch = harness.events().on::<RunEndEvent, _>(move |end| {
            observed.lock().unwrap().push(end.clone());
        });
        external_start_pending(&mut client, &output).await;
        output
            .wait_for(|m| m["type"] == "tool_execution_start")
            .await;
        block_next_write(&mut client, &release).await;
        // Input failure cancels the writer while its OS-style write cannot be preempted.
        client.write_all(&[0xff, b'\n']).await.unwrap();
        let idle = tokio::time::timeout(LIMIT, harness.lane("main").wait_for_idle()).await;
        let records =
            tokio::time::timeout(LIMIT, harness.session().find_records(&Default::default())).await;
        let still_blocked = !task.is_finished();
        release.release(); // Never assert while an OS-style write remains gated.
        let finished = tokio::time::timeout(LIMIT, &mut task).await;
        let dropped = tokio::time::timeout(LIMIT, release.0.dropped.notified()).await;
        idle.expect("failure must settle the cooperative lane while stdout is blocked")
            .unwrap();
        let records = records
            .expect("session records must remain readable")
            .unwrap();
        let ends = ends.lock().unwrap();
        let end = ends
            .iter()
            .find(|end| end.outcome == RunEndOutcome::Aborted)
            .expect("transport failure must emit an actual aborted RunEnd");
        assert!(
            records.iter().any(|record| matches!(record,
                LaneRecord::OperationFinished(finished)
                if finished.run_id == end.run_id && finished.outcome == OperationOutcome::Aborted
            )),
            "{records:?}"
        );
        assert!(
            still_blocked,
            "in-progress OS write should hold RPC completion until released"
        );
        assert_ne!(finished.expect("RPC must finish after release").unwrap(), 0);
        dropped.expect("physical output writer must retire after release");
        assert!(!ui.is_attached());
        // Physical output retirement proves no future output, not that every
        // signalled producer task has independently been observed joining.
        drop(emitter);
    }

    pub(super) async fn owner_drop_witness() {
        let (harness, mut client, output, task, ui, emitter, release) = pending_with_pumps().await;
        external_start_pending(&mut client, &output).await;
        ui.handle(serde_json::json!({"requestId":"owner-drop", "toolCallId":"approval", "ui":{}}))
            .unwrap();
        output.wait_for(|m| m["type"] == "ui_dialog_request").await;
        block_next_write(&mut client, &release).await;
        task.abort();
        let cancelled = tokio::time::timeout(LIMIT, task).await;
        let idle = tokio::time::timeout(LIMIT, harness.lane("main").wait_for_idle()).await;
        release.release();
        let dropped = tokio::time::timeout(LIMIT, release.0.dropped.notified()).await;
        assert!(cancelled
            .expect("owner abort must complete")
            .unwrap_err()
            .is_cancelled());
        idle.expect("cooperative run must quiesce after owner abort")
            .unwrap();
        dropped.expect("physical output writer must retire after owner cancellation");
        assert!(!ui.is_attached());
        assert_eq!(ui.pending_len(), 0);
        assert!(!ui.is_pending("owner-drop"));
        assert_eq!(ui.poll("owner-drop").unwrap()["status"], "cancelled");
        // A retained harness/emitter cannot write through the retired output.
        // This is output quiescence, not a per-producer task-join witness.
        let before = output.messages();
        let next = tokio::time::timeout(LIMIT, harness.lane("main").prompt_text("next", vec![]))
            .await
            .expect("retained harness must remain usable")
            .unwrap();
        drop(next);
        emitter.try_emit(AgentEvent::AgentStart);
        assert_eq!(
            output.messages(),
            before,
            "retired output must remain unchanged on subsequent commits/events"
        );
        let tools = harness.get_tools().await.unwrap();
        let result = tokio::time::timeout(
            LIMIT,
            tools[0].tool.execute(
                "after-drop",
                serde_json::json!({"query":"x"}),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(|_| {}),
            ),
        )
        .await
        .expect("retained backend must not hang");
        assert!(matches!(result, Err(rpi_agent::error::AgentError::Abort)));
    }
}
