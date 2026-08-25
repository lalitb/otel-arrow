// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use otap_df_pdata::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::receivers::filelog_receiver::batching::{FinalizationOutcome, ProgressFrontier};
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    FRAMING_PROFILE_VERSION, FramingResume, Locator,
};
use crate::receivers::filelog_receiver::checkpoint::store::fault::FaultPoint;
use crate::receivers::filelog_receiver::checkpoint::wal::RegisterFile;
use crate::receivers::filelog_receiver::config::Config;
use crate::receivers::filelog_receiver::discovery::scanner::DiscoveryPlan;
use crate::receivers::filelog_receiver::discovery::source::spawn_discovery;

fn runtime_config(
    include: &str,
    namespace_dir: &std::path::Path,
    max_records: u32,
) -> RuntimeConfig {
    let mut config: Config = serde_json::from_value(json!({
        "include": [include],
        "checkpoint": { "id": "worker-test" }
    }))
    .unwrap();
    config.start_at = crate::receivers::filelog_receiver::StartAt::Beginning;
    config.discovery.poll_interval = Duration::from_millis(10);
    config.limits.max_tracked_files = 4;
    config.limits.max_pending_candidates = 4;
    config.limits.max_open_files = 4;
    config.limits.max_read_bytes_per_turn = 64;
    config.batch.max_records = max_records;
    config.batch.max_flush_period = Duration::from_secs(30);
    let mut runtime = RuntimeConfig::from_config(config, "").unwrap();
    runtime.checkpoint_namespace_dir = namespace_dir.to_path_buf();
    runtime
}

async fn receive_batch(events: &mut tokio::sync::mpsc::Receiver<WorkerEvent>) -> WorkerBatch {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("worker event timeout")
            .expect("worker event channel closed");
        match event {
            WorkerEvent::Batch(batch) => return batch,
            WorkerEvent::Failed(error) => panic!("worker failed: {error}"),
            WorkerEvent::Stopped => panic!("worker stopped before emitting a batch"),
            WorkerEvent::CommitResult { .. } | WorkerEvent::Drained => {}
        }
    }
}

async fn receive_commit(
    events: &mut tokio::sync::mpsc::Receiver<WorkerEvent>,
) -> (u64, u32, bool, Result<(), WorkerError>) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("worker event timeout")
            .expect("worker event channel closed");
        match event {
            WorkerEvent::CommitResult {
                batch_id,
                attempt,
                explicit_loss,
                result,
            } => return (batch_id, attempt, explicit_loss, result),
            WorkerEvent::Failed(error) => panic!("worker failed: {error}"),
            WorkerEvent::Stopped => panic!("worker stopped before commit result"),
            WorkerEvent::Batch(_) | WorkerEvent::Drained => {}
        }
    }
}

async fn stop_worker(
    worker: WorkerHandle,
    events: &mut tokio::sync::mpsc::Receiver<WorkerEvent>,
) -> Result<(), WorkerError> {
    let _ = worker.command_tx.send(WorkerCommand::Shutdown);
    events.close();
    drop(worker.command_tx);
    tokio::task::spawn_blocking(move || worker.join.join())
        .await
        .unwrap()
        .unwrap()
}

/// Scenario: the exact RuntimeConfig used by worker integration tests starts
/// the dedicated discovery source against a temporary file.
/// Guarantees: startup emits one observed candidate immediately, ruling out
/// polling delay or glob-shape ambiguity in worker lifecycle tests.
#[test]
fn worker_runtime_discovery_plan_emits_initial_candidate() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("app.log");
    std::fs::write(&source, b"one\n").unwrap();
    let runtime = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    let discovery = spawn_discovery(DiscoveryPlan::from_runtime(&runtime).unwrap()).unwrap();
    let message = discovery.recv_timeout(Duration::from_secs(2)).unwrap();
    let DiscoveryMessage::Batch(batch) = message else {
        panic!("expected initial discovery batch");
    };
    assert_eq!(batch.events.len(), 1);
    assert!(matches!(batch.events[0], CandidateEvent::Observed(_)));
    discovery.request_shutdown();
    discovery.into_join_handle().join().unwrap();
}

/// Scenario: one temporary file is discovered, assigned a durable identity,
/// read, framed into two records, resent once, Ack-committed, and reopened.
/// Guarantees: resend shallow-clones the same Arrow arrays without rereading,
/// only attempt two commits, the real checkpoint reaches exact EOF, and a
/// second worker can reopen after discovery/reader leases are released.
#[tokio::test]
async fn discovery_to_ack_reopen_uses_same_retained_arrow_batch() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("app.log");
    std::fs::write(&source, b"one\ntwo\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 2);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let first = receive_batch(&mut events).await;
    assert_eq!(
        (first.batch_id, first.attempt, first.record_count),
        (1, 1, 2)
    );
    let first_column = first
        .records
        .get(ArrowPayloadType::Logs)
        .unwrap()
        .column(0)
        .clone();
    worker
        .command_tx
        .send(WorkerCommand::Resend {
            batch_id: first.batch_id,
            next_attempt: 2,
        })
        .unwrap();
    let second = receive_batch(&mut events).await;
    assert_eq!(
        (second.batch_id, second.attempt, second.record_count),
        (1, 2, 2)
    );
    let second_column = second
        .records
        .get(ArrowPayloadType::Logs)
        .unwrap()
        .column(0);
    assert!(Arc::ptr_eq(&first_column, second_column));

    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: 1,
            attempt: 2,
            explicit_loss: false,
        })
        .unwrap();
    let (batch_id, attempt, explicit_loss, result) = receive_commit(&mut events).await;
    assert_eq!((batch_id, attempt, explicit_loss), (1, 2, false));
    result.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let records: Vec<_> = store.table().iter().map(|(_, record)| record).collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].committed_offset, 8);
    drop(store);

    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let reopened = spawn_worker(runtime, event_tx).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err()
    );
    stop_worker(reopened, &mut events).await.unwrap();
}

/// Scenario: two readable files share a receiver whose first record exactly
/// seals a one-record batch, and no completion is sent.
/// Guarantees: the sole retained batch blocks every further filelog batch and
/// no durable offset advances for either file before shutdown.
#[tokio::test]
async fn retained_batch_blocks_all_files_until_completion() {
    let directory = tempdir().unwrap();
    std::fs::write(directory.path().join("a.log"), b"a\nnext\n").unwrap();
    std::fs::write(directory.path().join("b.log"), b"b\nnext\n").unwrap();
    let pattern = directory.path().join("*.log");
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(pattern.to_str().unwrap(), &namespace, 1);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let batch = receive_batch(&mut events).await;
    assert_eq!(batch.record_count, 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err()
    );
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().len(), 2);
    assert!(
        store
            .table()
            .iter()
            .all(|(_, record)| record.committed_offset == 0)
    );
}

/// Scenario: the progress WAL fails before writing the Ack transaction, then
/// async retries the same commit while the store is fail-closed.
/// Guarantees: every failure returns a CommitResult, the logical batch stays
/// retained and can still be shallow-resent, and reopen observes no progress
/// from either failed checkpoint attempt.
#[tokio::test]
async fn checkpoint_fault_retains_batch_across_retries() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("fault.log");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker_with_store_fault(
        runtime.clone(),
        event_tx,
        FaultPoint::BeforeWalTransactionWrite,
        1,
    )
    .unwrap();

    let first = receive_batch(&mut events).await;
    let first_column = first
        .records
        .get(ArrowPayloadType::Logs)
        .unwrap()
        .column(0)
        .clone();
    for _ in 0..2 {
        worker
            .command_tx
            .send(WorkerCommand::Commit {
                batch_id: 1,
                attempt: 1,
                explicit_loss: false,
            })
            .unwrap();
        let (batch_id, attempt, _, result) = receive_commit(&mut events).await;
        assert_eq!((batch_id, attempt), (1, 1));
        assert!(result.is_err());
    }

    worker
        .command_tx
        .send(WorkerCommand::Resend {
            batch_id: 1,
            next_attempt: 2,
        })
        .unwrap();
    let resend = receive_batch(&mut events).await;
    assert!(Arc::ptr_eq(
        &first_column,
        resend
            .records
            .get(ArrowPayloadType::Logs)
            .unwrap()
            .column(0)
    ));
    assert!(stop_worker(worker, &mut events).await.is_err());

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 0);
}

/// Scenario: the first post-registration compaction attempt fails at a safe
/// pre-write boundary and the fault plan then permits a retry.
/// Guarantees: the worker pauses source work, retries checkpoint maintenance
/// within the configured failure budget, and emits the source batch instead
/// of terminating after the first compaction error.
#[tokio::test]
async fn checkpoint_maintenance_failure_retries_before_source_progress() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("maintenance.log");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.checkpoint.compact_after_transactions = 1;
    runtime.checkpoint.max_consecutive_failures = 3;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker =
        spawn_worker_with_store_fault(runtime, event_tx, FaultPoint::BeforeSnapshotWrite, 1)
            .unwrap();

    let batch = receive_batch(&mut events).await;
    assert_eq!(batch.record_count, 1);
    stop_worker(worker, &mut events).await.unwrap();
}

/// Scenario: Drain arrives while one real batch is retained, and its matching
/// Ack commit completes before the engine's external deadline.
/// Guarantees: the worker waits for that completion, persists exact EOF,
/// performs final checkpoint drain, and reports Drained only after every
/// descriptor, runtime lease, and namespace lock is released.
#[tokio::test]
async fn drain_waits_for_matching_commit_before_reporting_drained() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("drain.log");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let batch = receive_batch(&mut events).await;
    worker.command_tx.send(WorkerCommand::Drain).unwrap();
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: batch.batch_id,
            attempt: batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    let (_, _, _, result) = receive_commit(&mut events).await;
    result.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, WorkerEvent::Drained));

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().iter().next().unwrap().1.committed_offset, 5);
    drop(store);
    stop_worker(worker, &mut events).await.unwrap();
}

/// Scenario: a same-locator file is overwritten between discovery passes
/// after its original bytes were Ack-committed.
/// Guarantees: Updated evidence is rejected before identity persistence,
/// Stage 11 fails closed, and reopen retains exactly the original durable
/// identity rather than a second poisoned record for the locator.
#[tokio::test]
async fn incompatible_updated_evidence_fails_before_checkpoint_mutation() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("updated.log");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let batch = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: batch.batch_id,
            attempt: batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    std::fs::write(&source, b"xxxx\n").unwrap();

    let failure = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            WorkerEvent::Failed(message) => break message,
            WorkerEvent::Stopped => panic!("worker stopped without its failure"),
            WorkerEvent::Batch(_) | WorkerEvent::CommitResult { .. } | WorkerEvent::Drained => {}
        }
    };
    assert!(failure.contains("requires Stage 12 identity/truncation handling"));
    events.close();
    drop(worker.command_tx);
    let thread_result = tokio::task::spawn_blocking(move || worker.join.join())
        .await
        .unwrap()
        .unwrap();
    assert!(thread_result.is_err());

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().len(), 1);
    assert_eq!(store.table().iter().next().unwrap().1.committed_offset, 5);
}

/// Scenario: the worker-to-async event slot is full while Drain is queued,
/// then the consumer releases exactly one slot.
/// Guarantees: event ownership is retained, Drain is observed during the
/// bounded retry loop, and the event is delivered after capacity returns.
#[test]
fn full_event_handoff_observes_drain_without_losing_event() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
    event_tx.try_send(WorkerEvent::Drained).unwrap();
    let (command_tx, command_rx) = sync_channel(1);
    command_tx.send(WorkerCommand::Drain).unwrap();
    let sender = std::thread::spawn(move || {
        send_event_interruptibly(&event_tx, &command_rx, WorkerEvent::Stopped)
    });

    loop {
        match command_tx.try_send(WorkerCommand::Drain) {
            Ok(()) => break,
            Err(std::sync::mpsc::TrySendError::Full(_)) => std::thread::yield_now(),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                panic!("event sender stopped before consuming Drain")
            }
        }
    }
    assert!(matches!(
        event_rx.blocking_recv(),
        Some(WorkerEvent::Drained)
    ));
    assert_eq!(sender.join().unwrap().unwrap(), HandoffControl::Drain);
    assert!(matches!(
        event_rx.blocking_recv(),
        Some(WorkerEvent::Stopped)
    ));
}

/// Scenario: the worker-to-async event slot is full while Shutdown is queued.
/// Guarantees: the handoff aborts immediately with event ownership dropped
/// locally, allowing thread shutdown without waiting for downstream capacity.
#[test]
fn full_event_handoff_observes_shutdown() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
    event_tx.try_send(WorkerEvent::Drained).unwrap();
    let (command_tx, command_rx) = sync_channel(1);
    command_tx.send(WorkerCommand::Shutdown).unwrap();
    assert_eq!(
        send_event_interruptibly(&event_tx, &command_rx, WorkerEvent::Stopped).unwrap(),
        HandoffControl::Shutdown
    );
}

/// Scenario: async teardown closes the event receiver while the worker still
/// owns an event from a command queued before Shutdown.
/// Guarantees: closed event handoff is treated as cancellation, allowing
/// worker cleanup and join to determine the real terminal result.
#[test]
fn closed_event_handoff_is_shutdown_cancellation() {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
    drop(event_rx);
    let (_command_tx, command_rx) = sync_channel(1);
    assert_eq!(
        send_event_interruptibly(&event_tx, &command_rx, WorkerEvent::Stopped).unwrap(),
        HandoffControl::Shutdown
    );
}

/// Scenario: Stage 12 supplies a same-frontier recordless finalization delta
/// for an active durable identity.
/// Guarantees: the worker helper persists the direct delta as one real
/// checkpoint transaction, finalizes lifecycle state, and creates no empty
/// OTAP batch.
#[test]
fn direct_recordless_finalization_commits_without_otap() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("finalize.log");
    let runtime = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        2,
    );
    let mut store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let file_id = FileId::from_bytes([91; 16]);
    let _outcomes = store
        .register_files(vec![RegisterFile {
            file_id,
            file_epoch: 1,
            committed_offset: 17,
            fingerprint: b"0123456789abcdef".to_vec(),
            ignored_header_bytes: 0,
            locator: Locator::Unspecified,
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: runtime.framing_profile_digest,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 10,
            advisory_path: b"finalize.log".to_vec(),
        }])
        .unwrap();
    let mut open = OpenBatch::new(&runtime).unwrap();
    let delta = match open
        .finalize_file(
            file_id,
            ProgressFrontier {
                file_epoch: 1,
                offset: 17,
                framing_resume: FramingResume::Clean,
            },
            11,
        )
        .unwrap()
    {
        FinalizationOutcome::Direct(delta) => delta,
        FinalizationOutcome::Merged => panic!("empty batch cannot merge finalization"),
    };

    persist_direct_progress(&mut store, &delta).unwrap();
    let record = store.table().get(&file_id).unwrap();
    assert_eq!(record.committed_offset, 17);
    assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
    assert_eq!(store.stats().wal_transactions, 2);
}

/// Scenario: two files each hold a drain-flushable unterminated record while
/// batch.max_records permits only one record per batch.
/// Guarantees: drain snapshots both provisional frontiers, emits two
/// separately Ack-gated batches in deterministic bounded replay, and reports
/// Drained only after both file offsets are durable.
#[test]
fn drain_replays_flushable_records_across_multiple_batches() {
    let directory = tempdir().unwrap();
    std::fs::write(directory.path().join("a.log"), b"partial-a").unwrap();
    std::fs::write(directory.path().join("b.log"), b"partial-b").unwrap();
    let pattern = directory.path().join("*.log");
    let runtime_config = runtime_config(
        pattern.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    let mut runtime = WorkerRuntime::new(runtime_config).unwrap();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let (_command_tx, command_rx) = sync_channel(4);
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        assert_eq!(
            runtime.process_discovery_message(&command_rx).unwrap(),
            LoopControl::Continue
        );
        let now = Instant::now();
        match runtime.readers_mut().unwrap().poll(now).unwrap() {
            ReaderPoll::Data(turn) => {
                assert_eq!(
                    runtime
                        .process_turn(turn, now, &event_tx, &command_rx)
                        .unwrap(),
                    LoopControl::Continue
                );
            }
            ReaderPoll::EndOfFile {
                file_id,
                file_epoch,
                source_offset,
                ..
            } => {
                assert_eq!(
                    runtime
                        .process_eof(
                            file_id,
                            file_epoch,
                            source_offset,
                            now,
                            &event_tx,
                            &command_rx,
                        )
                        .unwrap(),
                    LoopControl::Continue
                );
            }
            ReaderPoll::Idle { .. } | ReaderPoll::DescriptorCapacityBlocked { .. } => {
                std::thread::yield_now();
            }
            other => panic!("unexpected reader state before drain: {other:?}"),
        }
        let ready = runtime.framers.len() == 2
            && runtime
                .readers_ref()
                .unwrap()
                .frontiers()
                .all(|frontier| frontier.read_offset > frontier.committed_offset);
        if ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not buffer both tails"
        );
    }

    runtime.drain_requested = true;
    assert_eq!(
        runtime.drive_drain(&event_tx, &command_rx).unwrap(),
        LoopControl::Continue
    );
    let first = match event_rx.try_recv().unwrap() {
        WorkerEvent::Batch(batch) => batch,
        other => panic!("expected first drain batch, got {other:?}"),
    };
    assert_eq!(first.record_count, 1);
    runtime
        .commit_retained(first.batch_id, first.attempt)
        .unwrap();

    assert_eq!(
        runtime.drive_drain(&event_tx, &command_rx).unwrap(),
        LoopControl::Continue
    );
    let second = match event_rx.try_recv().unwrap() {
        WorkerEvent::Batch(batch) => batch,
        other => panic!("expected second drain batch, got {other:?}"),
    };
    assert_eq!(second.record_count, 1);
    assert_ne!(first.batch_id, second.batch_id);
    runtime
        .commit_retained(second.batch_id, second.attempt)
        .unwrap();

    assert_eq!(
        runtime.drive_drain(&event_tx, &command_rx).unwrap(),
        LoopControl::Shutdown
    );
    assert!(runtime.drain_complete);
    assert!(matches!(
        event_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(
        runtime
            .store
            .table()
            .iter()
            .all(|(_, record)| record.committed_offset == 9)
    );
    runtime.shutdown_resources().unwrap();
}

/// Scenario: one read turn contains two queued newline records, and the
/// second becomes ready exactly at the first record's batch deadline.
/// Guarantees: `SealBefore` drops the refused record and its reservation,
/// rewinds the reader to the accepted checkpoint, then rereads and commits
/// the second record only after the first batch is Acked.
#[test]
fn seal_before_discards_and_rereads_queued_turn_output() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("seal-before.log");
    std::fs::write(&source, b"one\ntwo\n").unwrap();
    let mut runtime_config = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        10,
    );
    runtime_config.batch.max_flush_period = Duration::from_millis(10);
    let mut runtime = WorkerRuntime::new(runtime_config).unwrap();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let (_command_tx, command_rx) = sync_channel(4);
    let deadline = Instant::now() + Duration::from_secs(5);

    let first_turn = loop {
        assert_eq!(
            runtime.process_discovery_message(&command_rx).unwrap(),
            LoopControl::Continue
        );
        match runtime.readers_mut().unwrap().poll(Instant::now()).unwrap() {
            ReaderPoll::Data(turn) => break turn,
            ReaderPoll::Idle { .. } | ReaderPoll::DescriptorCapacityBlocked { .. } => {
                std::thread::yield_now()
            }
            other => panic!("unexpected reader state before turn: {other:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "worker did not produce a read turn"
        );
    };
    assert_eq!(first_turn.bytes(), b"one\ntwo\n");

    let first_ready = Instant::now();
    let deadline_ready = first_ready + Duration::from_millis(10);
    let mut clock_calls = 0usize;
    let mut ready_clock = || {
        clock_calls += 1;
        if clock_calls == 1 {
            first_ready
        } else {
            deadline_ready
        }
    };
    assert_eq!(
        runtime
            .process_turn_with_clock(
                first_turn,
                first_ready,
                &event_tx,
                &command_rx,
                &mut ready_clock,
            )
            .unwrap(),
        LoopControl::Continue
    );
    let first = match event_rx.try_recv().unwrap() {
        WorkerEvent::Batch(batch) => batch,
        other => panic!("expected first sealed batch, got {other:?}"),
    };
    assert_eq!(first.record_count, 1);
    let frontier = runtime.readers_ref().unwrap().frontiers().next().unwrap();
    assert_eq!((frontier.committed_offset, frontier.read_offset), (0, 4));
    runtime
        .commit_retained(first.batch_id, first.attempt)
        .unwrap();

    let replay = match runtime.readers_mut().unwrap().poll(Instant::now()).unwrap() {
        ReaderPoll::Data(turn) => turn,
        other => panic!("expected refused record replay, got {other:?}"),
    };
    assert_eq!(replay.source_offset(), 4);
    assert_eq!(replay.bytes(), b"two\n");
    let replay_ready = Instant::now();
    assert_eq!(
        runtime
            .process_turn_with_clock(replay, replay_ready, &event_tx, &command_rx, &mut || {
                replay_ready
            },)
            .unwrap(),
        LoopControl::Continue
    );
    assert_eq!(
        runtime.seal_open_batch(&event_tx, &command_rx).unwrap(),
        LoopControl::Continue
    );
    let second = match event_rx.try_recv().unwrap() {
        WorkerEvent::Batch(batch) => batch,
        other => panic!("expected replay batch, got {other:?}"),
    };
    assert_eq!(second.record_count, 1);
    runtime
        .commit_retained(second.batch_id, second.attempt)
        .unwrap();
    assert_eq!(
        runtime
            .store
            .table()
            .iter()
            .next()
            .unwrap()
            .1
            .committed_offset,
        8
    );
    runtime.shutdown_resources().unwrap();
}
