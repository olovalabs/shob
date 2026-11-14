//! Queue ACKs and in-memory publication share an owned message gate. Dropping
//! an API caller cannot abandon the second half after storage accepted a write.
use super::*;

pub(crate) struct QueueReservation {
    pub(super) items: Vec<QueuedMessage>,
    queue: SharedMessageQueue,
}

impl QueueReservation {
    /// Keep pending items in their original positions until their entry ACK.
    /// A dropped handoff only releases reservations; it cannot lose the input.
    pub(super) fn reserve(queue: SharedMessageQueue, lane: &str, all: bool) -> Self {
        let items = {
            let mut q = queue.lock().unwrap();
            let limit = if all || matches!(q.mode, QueueMode::All) {
                usize::MAX
            } else {
                1
            };
            let items: Vec<_> = q
                .pending
                .iter()
                .filter(|item| item.lane == lane && !q.reserved.contains(&item.entry_id))
                .take(limit)
                .cloned()
                .collect();
            for item in &items {
                q.reserved.insert(item.entry_id.clone());
            }
            items
        };
        Self { items, queue }
    }
}

impl Drop for QueueReservation {
    fn drop(&mut self) {
        let mut queue = self.queue.lock().unwrap();
        for item in &self.items {
            queue.reserved.remove(&item.entry_id);
        }
    }
}

impl AgentHarness {
    pub(super) async fn reserve_next_run(&self) -> QueueReservation {
        let _gate = self.message_writes.lock().await;
        let queue = self.inner.lock().unwrap().next_run_queue.clone();
        QueueReservation::reserve(queue, &self.lane, true)
    }

    fn confirm_queue_entry(&self, id: &str) {
        let queues = {
            let inner = self.inner.lock().unwrap();
            [
                inner.steering_queue.clone(),
                inner.follow_up_queue.clone(),
                inner.next_run_queue.clone(),
            ]
        };
        for queue in queues {
            let mut q = queue.lock().unwrap();
            q.remove(id);
            q.reserved.remove(id);
        }
    }

    pub(super) async fn commit_initial_prompt(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
    ) -> HarnessResult<Entry> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            let (_, committed) = harness
                .session
                .append_record_and_entry(record, entry, &harness.lane)
                .await
                .map_err(session_to_harness_err)?;
            harness.confirm_queue_entry(committed.id());
            Ok(committed)
        })
        .await
        .map_err(|error| HarnessError::io(format!("initial prompt write task failed: {error}")))?
    }

    pub(super) async fn commit_reserved_prompt(
        &self,
        entry: ProvisionedEntry,
    ) -> HarnessResult<Entry> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            let committed = harness.append_provisioned_locked(entry).await?;
            harness.confirm_queue_entry(committed.id());
            Ok(committed)
        })
        .await
        .map_err(|error| HarnessError::io(format!("entry write task failed: {error}")))?
    }

    pub(super) async fn cancel_queue_item(&self, id: &str) -> HarnessResult<CancelQueuedResult> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _gate = gate;
            let queues = {
                let inner = harness.inner.lock().unwrap();
                if inner.closed {
                    return Err(HarnessError::closed());
                }
                [
                    inner.steering_queue.clone(),
                    inner.follow_up_queue.clone(),
                    inner.next_run_queue.clone(),
                ]
            };
            for queue in queues {
                let present = {
                    let q = queue.lock().unwrap();
                    q.pending
                        .iter()
                        .any(|item| item.lane == harness.lane && item.entry_id == id)
                        && !q.reserved.contains(&id)
                };
                if !present {
                    continue;
                }
                let record = harness.queue_cancellation(&id).await?;
                harness
                    .session
                    .append_queue_cancellations(vec![record])
                    .await
                    .map_err(session_to_harness_err)?;
                queue.lock().unwrap().remove(&id);
                return Ok(CancelQueuedResult {
                    outcome: CancelQueuedOutcome::Cancelled,
                });
            }
            Ok(CancelQueuedResult {
                outcome: CancelQueuedOutcome::AlreadyConsumed,
            })
        })
        .await
        .map_err(|error| HarnessError::io(format!("queue cancellation task failed: {error}")))?
    }

    async fn queue_cancellation(&self, id: &str) -> HarnessResult<LaneRecord> {
        let enqueued = self
            .find_enqueue(id)
            .await?
            .ok_or_else(|| HarnessError::io("queued item has no pending enqueue record"))?;
        Ok(LaneRecord::QueueCancelled(QueueCancelledRecord {
            base: self.record_base(),
            run_id: enqueued.run_id,
            entry_id: id.to_owned(),
        }))
    }

    pub(super) async fn clear_queue_items(&self) -> HarnessResult<QueuedMessages> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            harness.clear_queue_locked().await
        })
        .await
        .map_err(|error| HarnessError::io(format!("queue clear task failed: {error}")))?
    }

    /// Caller owns message_writes across cancellation and in-memory removal.
    /// Shared with abort so cancelling the run cannot race a queue drain.
    pub(super) async fn clear_queue_locked(&self) -> HarnessResult<QueuedMessages> {
        let queues = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            [inner.steering_queue.clone(), inner.follow_up_queue.clone()]
        };
        let items: Vec<Vec<QueuedMessage>> = queues
            .iter()
            .map(|queue| {
                let q = queue.lock().unwrap();
                q.pending
                    .iter()
                    .filter(|item| item.lane == self.lane && !q.reserved.contains(&item.entry_id))
                    .cloned()
                    .collect()
            })
            .collect();
        let mut records = Vec::new();
        for item in items.iter().flatten() {
            records.push(self.queue_cancellation(&item.entry_id).await?);
        }
        if !records.is_empty() {
            self.session
                .append_queue_cancellations(records)
                .await
                .map_err(session_to_harness_err)?;
        }
        let mut restored = QueuedMessages::default();
        for (index, (queue, items)) in queues.iter().zip(&items).enumerate() {
            for item in items {
                queue.lock().unwrap().remove(&item.entry_id);
                let out = if index == 0 {
                    &mut restored.steering
                } else {
                    &mut restored.follow_up
                };
                out.push(queued_message_view(&item.message));
            }
        }
        Ok(restored)
    }
}

#[cfg(test)]
mod cancellation_tests;

#[cfg(test)]
mod reservation_tests {
    use super::*;

    fn queue(mode: QueueMode) -> SharedMessageQueue {
        let mut queue = MessageQueue::new(mode);
        for (id, lane) in [("first", "main"), ("other", "side"), ("last", "main")] {
            queue.pending.push_back(QueuedMessage {
                entry_id: id.into(),
                lane: lane.into(),
                message: UserMessage::new(id, 0).into(),
            });
        }
        Arc::new(Mutex::new(queue))
    }

    fn views(texts: &[&str]) -> Vec<QueuedMessageView> {
        texts
            .iter()
            .map(|text| QueuedMessageView {
                text: (*text).into(),
                images: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn reservations_preserve_mode_lane_order_and_uncommitted_drop() {
        for (mode, expected) in [(QueueMode::OneAtATime, 1), (QueueMode::All, 2)] {
            let queue = queue(mode);
            let reservation = QueueReservation::reserve(queue.clone(), "main", false);
            assert_eq!(reservation.items.len(), expected);
            assert_eq!(reservation.items[0].entry_id, "first");
            assert_eq!(queue.lock().unwrap().pending.len(), 3);
            assert_eq!(
                queue.lock().unwrap().peek_for_lane("side"),
                views(&["other"])
            );
            assert_eq!(
                queue.lock().unwrap().peek_for_lane("main").len(),
                2 - expected
            );
            drop(reservation);
            assert_eq!(
                queue.lock().unwrap().peek_for_lane("main"),
                views(&["first", "last"])
            );
            assert!(queue.lock().unwrap().reserved.is_empty());
        }
    }

    #[test]
    fn next_run_reserves_all_even_in_one_at_a_time_mode() {
        let queue = queue(QueueMode::OneAtATime);
        let reservation = QueueReservation::reserve(queue.clone(), "main", true);
        assert_eq!(reservation.items.len(), 2);
        assert!(queue.lock().unwrap().peek_for_lane("main").is_empty());
        // An ACK removes just its ID; releasing the remaining reservations
        // cannot resurrect that committed prefix or reorder the suffix.
        queue.lock().unwrap().remove("first");
        drop(reservation);
        assert_eq!(
            queue.lock().unwrap().peek_for_lane("main"),
            views(&["last"])
        );
        assert_eq!(
            queue.lock().unwrap().peek_for_lane("side"),
            views(&["other"])
        );
    }
}
