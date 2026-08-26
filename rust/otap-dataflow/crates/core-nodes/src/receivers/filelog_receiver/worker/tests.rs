// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;
use std::{fs::OpenOptions, io::Write};

use otap_df_pdata::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::receivers::filelog_receiver::batching::{FinalizationOutcome, ProgressFrontier};
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    FRAMING_PROFILE_VERSION, FramingResume, Locator, QUARANTINE_REASON_DECODE,
    QUARANTINE_REASON_TRUNCATE,
};
use crate::receivers::filelog_receiver::checkpoint::store::fault::FaultPoint;
use crate::receivers::filelog_receiver::checkpoint::wal::{RegisterFile, UpdateProgress};
use crate::receivers::filelog_receiver::config::{Config, OnDecodeError};
use crate::receivers::filelog_receiver::discovery::DiscoveredCandidate;
use crate::receivers::filelog_receiver::discovery::scanner::DiscoveryPlan;
use crate::receivers::filelog_receiver::discovery::source::spawn_discovery;
use crate::receivers::filelog_receiver::identity::matcher::{IdentityMatch, ResolvedIdentity};
use crate::receivers::filelog_receiver::identity::platform::open_candidate;

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

/// Scenario: framed outputs and lifecycle transitions cover decode policies,
/// record truncation, splitting, bounded flushes, copy-truncate policies,
/// descriptor quarantine, rotation finalization, and checkpoint maintenance.
/// Guarantees: worker-owned authoritative frame observations increment only
/// their fixed telemetry counters with exact malformed and discarded counts.
#[test]
fn framed_record_telemetry_uses_exact_bounded_categories() {
    let telemetry = WorkerTelemetryBridge::default();
    record_framed_telemetry(
        &telemetry,
        FramedTelemetry {
            decode_outcome: DecodeOutcome::Replacements { count: 2 },
            flush_reason: Some(FlushReason::MaxLines),
            truncated: true,
            discarded_source_bytes: 7,
            split: false,
        },
    );
    record_truncation_detection(&telemetry);
    record_truncation_outcome(&telemetry, OnTruncate::Fail);
    record_truncation_detection(&telemetry);
    record_truncation_outcome(&telemetry, OnTruncate::ReadNew);
    record_descriptor_quarantine_telemetry(&telemetry);
    record_rotation_finalization_telemetry(&telemetry);
    record_checkpoint_maintenance_telemetry(&telemetry, 1, 2, Duration::from_nanos(11), 2, true);
    record_framed_telemetry(
        &telemetry,
        FramedTelemetry {
            decode_outcome: DecodeOutcome::PreserveRaw { count: 3 },
            flush_reason: Some(FlushReason::Timeout),
            truncated: false,
            discarded_source_bytes: 0,
            split: true,
        },
    );

    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::DecodeReplaceRecords),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::DecodeReplaceUnits),
        2
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::DecodePreserveRawRecords),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::DecodePreserveRawUnits),
        3
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::RecordsTruncated),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::SourceBytesDiscarded),
        7
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::SplitFragments),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::FlushMaxLines),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::FlushTimeout),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::CopytruncateDetected),
        2
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::CopytruncateFail),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::CopytruncateReadNew),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::QuarantineTruncate),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::RotationDescriptorUnavailable),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::QuarantineDescriptorUnavailable),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::RotationFinalizations),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::CheckpointCompactions),
        1
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::CheckpointCompactionDurationNs),
        11
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::CheckpointCleanupGenerations),
        2
    );
    assert_eq!(
        telemetry.take_counter_for_test(WorkerCounter::CheckpointCleanupFailures),
        1
    );
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

async fn wait_for_worker_counter(
    telemetry: &WorkerTelemetryBridge,
    counter: WorkerCounter,
    expected: u64,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if telemetry.counter_for_test(counter) == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("worker telemetry counter timeout");
}

async fn wait_for_worker_gauge(
    telemetry: &WorkerTelemetryBridge,
    gauge: WorkerGauge,
    expected: u64,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if telemetry.gauge_for_test(gauge) == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("worker telemetry gauge timeout");
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

/// Scenario: a worker opens a source descriptor and then receives forced
/// shutdown while discovery still has current observations.
/// Guarantees: terminal bridge state zeros reader and discovery gauges before
/// the worker exits, without scanning retained reader or checkpoint tables.
#[tokio::test]
async fn worker_shutdown_zeros_all_terminal_population_gauges() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("shutdown-gauges.log");
    std::fs::write(&source, b"partial").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 10);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime, event_tx).unwrap();
    let telemetry = Arc::clone(&worker.telemetry);

    wait_for_worker_gauge(&telemetry, WorkerGauge::FilesOpen, 1).await;
    stop_worker(worker, &mut events).await.unwrap();
    assert_eq!(telemetry.gauge_for_test(WorkerGauge::FilesOpen), 0);
    assert_eq!(
        telemetry.gauge_for_test(WorkerGauge::FilesDescriptorBlocked),
        0
    );
    assert_eq!(
        telemetry.gauge_for_test(WorkerGauge::FilesRemovedWaiting),
        0
    );
    assert_eq!(telemetry.gauge_for_test(WorkerGauge::FilesPending), 0);
    assert_eq!(
        telemetry.gauge_for_test(WorkerGauge::CandidateOldestAgeNs),
        0
    );
    assert_eq!(
        telemetry.gauge_for_test(WorkerGauge::CandidateOverflowPersistenceNs),
        0
    );
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

/// Scenario: one malformed file and one valid file are discovered together
/// under the decode `fail` policy, then the worker is restarted.
/// Guarantees: only the malformed identity is durably quarantined with reason
/// `0x0001`, unrelated records continue, and restart never rereads quarantine.
#[tokio::test]
async fn decode_failure_is_durably_contained_to_one_file_across_restart() {
    let directory = tempdir().unwrap();
    let bad = directory.path().join("bad.log");
    let good = directory.path().join("good.log");
    std::fs::write(&bad, [0xff, b'\n']).unwrap();
    std::fs::write(&good, b"good\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let include = directory.path().join("*.log");
    let mut runtime = runtime_config(include.to_str().unwrap(), &namespace, 1);
    runtime.on_decode_error = OnDecodeError::Fail;
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
    wait_for_worker_counter(&worker.telemetry, WorkerCounter::QuarantineDecode, 1).await;
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let quarantined = store
        .table()
        .iter()
        .find(|(_, record)| record.lifecycle_state == LifecycleState::Quarantined)
        .map(|(_, record)| record)
        .expect("malformed identity is quarantined");
    assert_eq!(
        quarantined
            .quarantine_evidence
            .as_ref()
            .expect("quarantine evidence")
            .reason_code,
        QUARANTINE_REASON_DECODE
    );
    assert!(store.table().iter().any(|(_, record)| {
        record.lifecycle_state == LifecycleState::Active && record.committed_offset == 5
    }));
    drop(store);

    OpenOptions::new()
        .append(true)
        .open(&good)
        .unwrap()
        .write_all(b"again\n")
        .unwrap();
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let reopened = spawn_worker(runtime.clone(), event_tx).unwrap();
    let continued = receive_batch(&mut events).await;
    reopened
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: continued.batch_id,
            attempt: continued.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    assert_eq!(
        reopened
            .telemetry
            .counter_for_test(WorkerCounter::QuarantineDecode),
        0
    );
    stop_worker(reopened, &mut events).await.unwrap();
}

/// Scenario: a file emits one valid record and then reaches malformed input
/// while that record remains in the open logical batch.
/// Guarantees: the batch seals before quarantine, durable state remains
/// active until its matching Ack, and deferred containment does not reread or
/// count the malformed unit twice.
#[tokio::test]
async fn decode_quarantine_waits_for_preexisting_batch_ack() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("mixed.log");
    std::fs::write(&source, [b'g', b'o', b'o', b'd', b'\n', 0xff, b'\n']).unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 10);
    runtime.on_decode_error = OnDecodeError::Fail;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let batch = receive_batch(&mut events).await;
    assert_eq!(batch.record_count, 1);
    assert_eq!(
        worker
            .telemetry
            .counter_for_test(WorkerCounter::QuarantineDecode),
        0
    );
    assert_eq!(
        worker
            .telemetry
            .gauge_for_test(WorkerGauge::FilesQuarantined),
        0
    );
    assert_eq!(
        worker
            .telemetry
            .counter_for_test(WorkerCounter::DecodeFailures),
        1
    );

    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: batch.batch_id,
            attempt: batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    wait_for_worker_counter(&worker.telemetry, WorkerCounter::QuarantineDecode, 1).await;
    assert_eq!(
        worker
            .telemetry
            .counter_for_test(WorkerCounter::DecodeFailures),
        1
    );
    assert_eq!(
        worker
            .telemetry
            .counter_for_test(WorkerCounter::SourceBytesRead),
        7
    );
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 5);
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(
        record
            .quarantine_evidence
            .as_ref()
            .expect("quarantine evidence")
            .reason_code,
        QUARANTINE_REASON_DECODE
    );
}

/// Scenario: async closes the worker-event receiver after accepting the batch
/// whose valid prefix precedes a malformed source unit, then sends its Ack.
/// Guarantees: event-handoff cancellation cannot skip the post-commit decode
/// quarantine or cause the malformed unit to be detected twice after restart.
#[tokio::test]
async fn closed_commit_handoff_still_persists_pending_decode_quarantine() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("closed-handoff.log");
    std::fs::write(&source, [b'g', b'o', b'o', b'd', b'\n', 0xff, b'\n']).unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 10);
    runtime.on_decode_error = OnDecodeError::Fail;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let telemetry = Arc::clone(&worker.telemetry);

    let batch = receive_batch(&mut events).await;
    events.close();
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: batch.batch_id,
            attempt: batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    drop(events);
    let joined = tokio::task::spawn_blocking(move || worker.join.join())
        .await
        .unwrap()
        .unwrap();
    joined.unwrap();

    assert_eq!(telemetry.counter_for_test(WorkerCounter::DecodeFailures), 1);
    assert_eq!(
        telemetry.counter_for_test(WorkerCounter::QuarantineDecode),
        1
    );
    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 5);
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
}

/// Scenario: decode quarantine is durably synced, then injected reader cleanup
/// fails before the quarantined reader can be released.
/// Guarantees: successful quarantine telemetry and the durable population are
/// visible exactly once even though the worker subsequently fails closed.
#[test]
fn durable_decode_quarantine_is_reported_before_reader_cleanup() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("cleanup-fault.log");
    std::fs::write(&source, [0xff, b'\n']).unwrap();
    let mut runtime_config = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    runtime_config.on_decode_error = OnDecodeError::Fail;
    let mut runtime = WorkerRuntime::new(runtime_config).unwrap();
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let (_command_tx, command_rx) = sync_channel(4);
    let deadline = Instant::now() + Duration::from_secs(5);

    while runtime.readers_ref().unwrap().stats().tracked_readers == 0 {
        assert!(Instant::now() < deadline, "discovery did not admit source");
        assert_eq!(
            runtime
                .process_discovery_message(&event_tx, &command_rx)
                .unwrap(),
            LoopControl::Continue
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    runtime
        .readers_mut()
        .unwrap()
        .fail_next_revoked_release_for_test();
    let now = Instant::now();
    let turn = match runtime.readers_mut().unwrap().poll(now).unwrap() {
        ReaderPoll::Data(turn) => turn,
        other => panic!("expected malformed source data, got {other:?}"),
    };
    let error = runtime
        .process_turn(turn, now, &event_tx, &command_rx)
        .expect_err("injected reader cleanup must fail");

    assert!(
        error
            .to_string()
            .contains("injected revoked-reader release")
    );
    assert_eq!(
        runtime
            .telemetry
            .counter_for_test(WorkerCounter::DecodeFailures),
        1
    );
    assert_eq!(
        runtime
            .telemetry
            .counter_for_test(WorkerCounter::QuarantineDecode),
        1
    );
    assert_eq!(
        runtime
            .telemetry
            .gauge_for_test(WorkerGauge::FilesQuarantined),
        1
    );
    let record = runtime.store.table().iter().next().unwrap().1;
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(
        record.quarantine_evidence.as_ref().unwrap().reason_code,
        QUARANTINE_REASON_DECODE
    );
    runtime.shutdown_resources().unwrap();
}

/// Scenario: decode quarantine persistence faults before its WAL append or
/// at its required WAL sync after registration already succeeded.
/// Guarantees: detection remains visible, successful-quarantine telemetry is
/// zero, rollback preserves the old active state, and the worker fails closed.
#[tokio::test]
async fn decode_quarantine_faults_never_report_success() {
    for point in [
        FaultPoint::BeforeWalTransactionWrite,
        FaultPoint::BeforeWalSync,
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("malformed.log");
        std::fs::write(&source, [0xff, b'\n']).unwrap();
        let namespace = directory.path().join("checkpoint");
        let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
        runtime.on_decode_error = OnDecodeError::Fail;
        let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let worker = spawn_worker_with_store_fault(runtime.clone(), event_tx, point, 1).unwrap();
        let telemetry = Arc::clone(&worker.telemetry);

        let failure = loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("worker failure timeout")
                .expect("worker event channel closed")
            {
                WorkerEvent::Failed(message) => break message,
                WorkerEvent::Stopped => panic!("worker stopped without failure evidence"),
                WorkerEvent::Batch(_) | WorkerEvent::CommitResult { .. } | WorkerEvent::Drained => {
                }
            }
        };
        assert!(failure.contains("checkpoint"), "{failure}");
        assert_eq!(telemetry.counter_for_test(WorkerCounter::DecodeFailures), 1);
        assert_eq!(
            telemetry.counter_for_test(WorkerCounter::QuarantineDecode),
            0
        );
        assert_eq!(
            telemetry.counter_for_test(WorkerCounter::CheckpointFailures),
            u64::from(runtime.checkpoint.max_consecutive_failures) + 1
        );
        assert_eq!(telemetry.gauge_for_test(WorkerGauge::FilesQuarantined), 0);
        events.close();
        drop(worker.command_tx);
        assert!(
            tokio::task::spawn_blocking(move || worker.join.join())
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );

        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        let record = store.table().iter().next().unwrap().1;
        assert_eq!(record.lifecycle_state, LifecycleState::Active);
        assert_eq!(record.committed_offset, 0);
    }
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

/// Scenario: a due interval sync fails while one retired generation is
/// pending cleanup.
/// Guarantees: checkpoint failure is counted, cleanup is not attempted or
/// classified as failed, and the retired generation remains pending.
#[test]
fn due_sync_failure_does_not_count_unattempted_cleanup() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("unused.log");
    let mut runtime = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    runtime.checkpoint.sync_interval = Duration::from_millis(100);
    runtime.checkpoint.max_consecutive_failures = 2;
    let telemetry = Arc::new(WorkerTelemetryBridge::default());
    let mut worker = WorkerRuntime::new_with_store_fault_and_telemetry(
        runtime.clone(),
        FaultPoint::BeforeWalSync,
        1,
        Arc::clone(&telemetry),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let file_id = FileId::from_bytes([88; 16]);
    let _registered = worker
        .store
        .register_files(vec![RegisterFile {
            file_id,
            file_epoch: 1,
            committed_offset: 0,
            fingerprint: b"0123456789abcdef".to_vec(),
            ignored_header_bytes: 0,
            locator: Locator::Unspecified,
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: runtime.framing_profile_digest,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 1,
            advisory_path: b"unused.log".to_vec(),
        }])
        .unwrap();
    worker.store.compact().unwrap();
    let _progress = worker
        .store
        .commit_progress(vec![UpdateProgress {
            file_id,
            expected_committed_offset: 0,
            expected_file_epoch: 1,
            new_committed_offset: 0,
            new_framing_resume: FramingResume::Clean,
            new_last_seen_time_unix_nano: 2,
            finalize: false,
        }])
        .unwrap();
    worker.store.force_sync_due_for_test();

    worker.maintain_store().unwrap();
    assert_eq!(
        telemetry.counter_for_test(WorkerCounter::CheckpointFailures),
        1
    );
    assert_eq!(
        telemetry.counter_for_test(WorkerCounter::CheckpointCleanupFailures),
        0
    );
    assert_eq!(worker.store.retired_generations(), [0]);

    if let Some(discovery) = worker.discovery.take() {
        discovery.request_shutdown();
        discovery.into_join_handle().join().unwrap();
    }
    if let Some(readers) = worker.readers.take() {
        readers.shutdown().unwrap();
    }
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

/// Scenario: a same-locator file is overwritten after its original bytes
/// were Ack-committed under the default truncate policy.
/// Guarantees: the exact durable identity is synced to quarantine before its
/// descriptor and lease are released, without terminating the receiver.
#[tokio::test]
async fn incompatible_updated_evidence_is_durably_quarantined() {
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

    match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
        Err(_) => {}
        Ok(Some(event)) => panic!("unexpected event after quarantine: {event:?}"),
        Ok(None) => panic!("worker event channel closed after quarantine"),
    }
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().len(), 1);
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 5);
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(
        record.quarantine_evidence.as_ref().unwrap().reason_code,
        QUARANTINE_REASON_TRUNCATE
    );
    drop(store);

    let mut reloaded = runtime.clone();
    reloaded.rotation.on_truncate = OnTruncate::ReadNew;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(reloaded.clone(), event_tx).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), events.recv())
            .await
            .is_err()
    );
    stop_worker(worker, &mut events).await.unwrap();
    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&reloaded)).unwrap();
    assert_eq!(
        store.table().iter().next().unwrap().1.lifecycle_state,
        LifecycleState::Quarantined
    );
}

/// Scenario: a same-locator overwrite is detected while its previous record
/// batch is retained and unacknowledged.
/// Guarantees: the worker neither resets nor quarantines under the retained
/// delta, then durably quarantines only after the matching Ack advances it.
#[tokio::test]
async fn retained_batch_defers_truncation_transition_until_ack() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("retained-truncate.log");
    std::fs::write(&source, b"old\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let batch = receive_batch(&mut events).await;
    let mut replacement = OpenOptions::new().write(true).open(&source).unwrap();
    replacement.write_all(b"new\n").unwrap();
    replacement.sync_all().unwrap();
    drop(replacement);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), events.recv())
            .await
            .is_err()
    );

    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: batch.batch_id,
            attempt: batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_err()
    );
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 4);
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
}

/// Scenario: observable same-locator truncation uses the explicit
/// `read_new` policy after one old-epoch record was committed.
/// Guarantees: reset and fingerprint replacement are synced atomically before
/// new bytes are read, epoch two advances, and an epoch-one delta is rejected.
#[tokio::test]
async fn read_new_resets_epoch_before_reading_replacement_stream() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("read-new.log");
    std::fs::write(&source, b"old\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.rotation.on_truncate = OnTruncate::ReadNew;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let old = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: old.batch_id,
            attempt: old.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    let mut replacement = OpenOptions::new().write(true).open(&source).unwrap();
    replacement.write_all(b"new\n").unwrap();
    replacement.sync_all().unwrap();
    drop(replacement);
    let new = receive_batch(&mut events).await;
    assert_eq!(new.record_count, 1);
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: new.batch_id,
            attempt: new.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let mut store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let (file_id, record) = store.table().iter().next().unwrap();
    let file_id = *file_id;
    assert_eq!(record.file_epoch, 2);
    assert_eq!(record.committed_offset, 4);
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
    assert_eq!(record.fingerprint, b"new\n");

    let stale = store.commit_progress(vec![UpdateProgress {
        file_id,
        expected_committed_offset: 4,
        expected_file_epoch: 1,
        new_committed_offset: 8,
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: record.last_seen_time_unix_nano,
        finalize: false,
    }]);
    assert!(stale.is_err());
    let unchanged = store.table().get(&file_id).unwrap();
    assert_eq!((unchanged.file_epoch, unchanged.committed_offset), (2, 4));
}

/// Scenario: truncate-fail quarantine is durably synced, then injected reader
/// cleanup fails before the quarantined reader can be released.
/// Guarantees: the truncate outcome, quarantine action, and durable population
/// are published exactly once before the worker fails closed.
#[test]
fn durable_truncate_quarantine_is_reported_before_reader_cleanup() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("truncate-cleanup-fault.log");
    std::fs::write(&source, b"old\n").unwrap();
    let mut runtime_config = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    runtime_config.rotation.on_truncate = OnTruncate::Fail;
    let mut runtime = WorkerRuntime::new(runtime_config).unwrap();
    runtime.readers.take().unwrap().shutdown().unwrap();

    let resolved_path = std::fs::canonicalize(&source).unwrap();
    let opened = open_candidate(&resolved_path, false, 16, 0).unwrap();
    let evidence = opened.evidence;
    let locator = evidence.locator;
    let file_id = FileId::from_bytes([93; 16]);
    let _outcomes = runtime
        .store
        .register_files(vec![RegisterFile {
            file_id,
            file_epoch: 1,
            committed_offset: 4,
            fingerprint: evidence.fingerprint.clone(),
            ignored_header_bytes: 0,
            locator,
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: runtime.config.framing_profile_digest,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 1,
            advisory_path: evidence.advisory_path.clone(),
        }])
        .unwrap();
    let mut readers = ReaderTable::new(ReaderSettings::from_runtime(&runtime.config)).unwrap();
    readers
        .insert(
            DiscoveredCandidate {
                matched_path: source,
                resolved_path,
                evidence: evidence.clone(),
                modified: None,
            },
            ResolvedIdentity {
                file_id,
                file_epoch: 1,
                committed_offset: 4,
                framing_resume: FramingResume::Clean,
                lifecycle_state: LifecycleState::Active,
                matched_by: IdentityMatch::NewDiscovery,
            },
        )
        .unwrap();
    runtime.readers = Some(readers);
    let truncation = DetectedTruncation {
        file_id,
        expected_file_epoch: 1,
        expected_committed_offset: 4,
        observed_size: 0,
        observed_fingerprint: evidence.fingerprint,
        locator,
        present: true,
    };
    runtime.preflight_truncation(&truncation).unwrap();
    runtime
        .readers_mut()
        .unwrap()
        .fail_next_revoked_release_for_test();

    let error = runtime
        .apply_truncation(truncation)
        .expect_err("injected reader cleanup must fail");

    assert!(
        error
            .to_string()
            .contains("injected revoked-reader release")
    );
    assert_eq!(
        runtime
            .telemetry
            .counter_for_test(WorkerCounter::CopytruncateDetected),
        1
    );
    assert_eq!(
        runtime
            .telemetry
            .counter_for_test(WorkerCounter::CopytruncateFail),
        1
    );
    assert_eq!(
        runtime
            .telemetry
            .counter_for_test(WorkerCounter::QuarantineTruncate),
        1
    );
    assert_eq!(
        runtime
            .telemetry
            .gauge_for_test(WorkerGauge::FilesQuarantined),
        1
    );
    let record = runtime.store.table().get(&file_id).unwrap();
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(
        record.quarantine_evidence.as_ref().unwrap().reason_code,
        QUARANTINE_REASON_TRUNCATE
    );
    runtime.shutdown_resources().unwrap();
}

/// Scenario: the WAL fails at append or required sync for either a truncate
/// quarantine or a `read_new` reset transaction.
/// Guarantees: detection is counted while successful outcomes remain zero,
/// both policies fail closed, and reopen retains the old active frontier.
#[tokio::test]
async fn truncate_transitions_fail_closed_at_wal_fault_boundary() {
    for policy in [OnTruncate::Fail, OnTruncate::ReadNew] {
        for point in [
            FaultPoint::BeforeWalTransactionWrite,
            FaultPoint::BeforeWalSync,
        ] {
            let directory = tempdir().unwrap();
            let source = directory.path().join("fault-truncate.log");
            std::fs::write(&source, b"old\n").unwrap();
            let namespace = directory.path().join("checkpoint");
            let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
            runtime.rotation.on_truncate = policy;
            let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
            let worker =
                spawn_worker_with_store_fault(runtime.clone(), event_tx, point, 2).unwrap();
            let telemetry = Arc::clone(&worker.telemetry);

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
            std::fs::write(&source, b"new\n").unwrap();

            let failure = loop {
                let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                match event {
                    WorkerEvent::Failed(message) => break message,
                    WorkerEvent::Stopped => panic!("worker stopped without fault evidence"),
                    WorkerEvent::Batch(_)
                    | WorkerEvent::CommitResult { .. }
                    | WorkerEvent::Drained => {}
                }
            };
            assert!(failure.contains("checkpoint"), "{failure}");
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CopytruncateDetected),
                1
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CopytruncateFail),
                0
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CopytruncateReadNew),
                0
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::QuarantineTruncate),
                0
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CheckpointFailures),
                u64::from(runtime.checkpoint.max_consecutive_failures) + 1
            );
            events.close();
            drop(worker.command_tx);
            assert!(
                tokio::task::spawn_blocking(move || worker.join.join())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );

            let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
            let record = store.table().iter().next().unwrap().1;
            assert_eq!((record.file_epoch, record.committed_offset), (1, 4));
            assert_eq!(record.lifecycle_state, LifecycleState::Active);
        }
    }
}

/// Scenario: `read_new` detects copy-truncate after another file evicts the
/// affected reader's descriptor under `max_open_files: 1`.
/// Guarantees: the reset is synced, the same locator reopens from offset
/// zero at the next epoch, and collection continues without a worker failure.
#[tokio::test]
async fn read_new_reopens_a_present_nonresident_reader() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("first.log");
    let second = directory.path().join("second.log");
    std::fs::write(&first, b"old\n").unwrap();
    let include = directory.path().join("*.log");
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(include.to_str().unwrap(), &namespace, 1);
    runtime.limits.max_open_files = 1;
    runtime.rotation.on_truncate = OnTruncate::ReadNew;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let initial = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: initial.batch_id,
            attempt: initial.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    std::fs::write(&second, b"other\n").unwrap();
    let other = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: other.batch_id,
            attempt: other.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    std::fs::write(&first, b"new\n").unwrap();
    let replacement = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: replacement.batch_id,
            attempt: replacement.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let reset = store
        .table()
        .iter()
        .find(|(_, record)| record.file_epoch == 2)
        .map(|(_, record)| record)
        .expect("one reader must advance to the replacement epoch");
    assert_eq!(reset.committed_offset, 4);
    assert_eq!(reset.fingerprint, b"new\n");
}

/// Scenario: a present file's descriptor is evicted, then move/create
/// rotation removes that path while another file remains readable.
/// Guarantees: the affected identity is durably quarantined with explicit
/// missing-handle evidence and unrelated file collection continues.
#[tokio::test]
async fn removed_nonresident_reader_is_contained_per_file() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("first.log");
    let rotated = directory.path().join("first.log.1");
    let second = directory.path().join("second.log");
    std::fs::write(&first, b"old\n").unwrap();
    let include = directory.path().join("*.log");
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(include.to_str().unwrap(), &namespace, 1);
    runtime.limits.max_open_files = 1;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let initial = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: initial.batch_id,
            attempt: initial.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    std::fs::write(&second, b"one\n").unwrap();
    let other = receive_batch(&mut events).await;
    std::fs::rename(&first, &rotated).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: other.batch_id,
            attempt: other.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
        Err(_) => {}
        Ok(Some(event)) => panic!("unexpected event during per-file containment: {event:?}"),
        Ok(None) => panic!("worker stopped during per-file containment"),
    }
    OpenOptions::new()
        .append(true)
        .open(&second)
        .unwrap()
        .write_all(b"two\n")
        .unwrap();
    let continued = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: continued.batch_id,
            attempt: continued.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let quarantined = store
        .table()
        .iter()
        .find(|(_, record)| record.lifecycle_state == LifecycleState::Quarantined)
        .map(|(_, record)| record)
        .expect("descriptor-free rotation must be quarantined");
    assert_eq!(quarantined.committed_offset, 4);
    assert_eq!(
        quarantined
            .quarantine_evidence
            .as_ref()
            .unwrap()
            .reason_code,
        QUARANTINE_REASON_ROTATION_DESCRIPTOR_UNAVAILABLE
    );
    assert!(store.table().iter().any(|(_, record)| {
        record.lifecycle_state == LifecycleState::Active && record.committed_offset == 8
    }));
}

#[cfg(unix)]
/// Scenario: move/create rotation replaces the matched path while the old
/// file remains writable through an already-open Unix descriptor.
/// Guarantees: the replacement gets an independent identity, a late old-file
/// append resets the wait and is Acked, then only the old identity finalizes.
#[tokio::test]
async fn move_create_reads_replacement_and_late_write_before_finalization() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("app.log");
    let rotated = directory.path().join("app.log.1");
    std::fs::write(&source, b"old\n").unwrap();
    let mut late_writer = OpenOptions::new().append(true).open(&source).unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.rotation.rotate_wait = Duration::from_millis(300);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let initial = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: initial.batch_id,
            attempt: initial.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    std::fs::rename(&source, &rotated).unwrap();
    std::fs::write(&source, b"new\n").unwrap();
    let replacement = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: replacement.batch_id,
            attempt: replacement.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    late_writer.write_all(b"late\n").unwrap();
    late_writer.flush().unwrap();
    let late = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: late.batch_id,
            attempt: late.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    drop(late_writer);

    tokio::time::sleep(Duration::from_millis(450)).await;
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().len(), 2);
    let mut states: Vec<_> = store
        .table()
        .iter()
        .map(|(_, record)| (record.lifecycle_state, record.committed_offset))
        .collect();
    states.sort_unstable_by_key(|(_, offset)| *offset);
    assert_eq!(
        states,
        vec![
            (LifecycleState::Active, 4),
            (LifecycleState::RotatedFinalized, 9),
        ]
    );
}

#[cfg(unix)]
/// Scenario: a removed file ends with an unterminated tail while partial
/// flushing is disabled and its retained descriptor reaches stable EOF.
/// Guarantees: rotation emits no empty or tail record, advances no offset,
/// and directly finalizes only the already-durable zero frontier.
#[tokio::test]
async fn rotation_drops_disabled_unterminated_tail_without_advancing_progress() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("tail.log");
    let rotated = directory.path().join("tail.log.1");
    std::fs::write(&source, b"ready\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.framing.force_flush_period = Duration::ZERO;
    runtime.rotation.rotate_wait = Duration::from_millis(30);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let ready = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: ready.batch_id,
            attempt: ready.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    let mut writer = OpenOptions::new().append(true).open(&source).unwrap();
    writer.write_all(b"tail").unwrap();
    writer.flush().unwrap();
    std::fs::rename(&source, &rotated).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_err()
    );
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 6);
    assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
}

#[cfg(unix)]
/// Scenario: rotation flushes a final unterminated record under the enabled
/// partial-flush policy, once without Ack and once with a matching Ack.
/// Guarantees: the finalize bit remains behind the record's delta: shutdown
/// before Ack leaves the prior offset active, while Ack commits the tail and
/// finalization atomically.
#[tokio::test]
async fn rotation_flushed_tail_finalizes_only_with_matching_ack() {
    for acknowledge in [false, true] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("tail.log");
        let rotated = directory.path().join("tail.log.1");
        std::fs::write(&source, b"ready\n").unwrap();
        let namespace = directory.path().join("checkpoint");
        let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
        runtime.framing.force_flush_period = Duration::from_secs(10);
        runtime.rotation.rotate_wait = Duration::from_millis(30);
        let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

        let ready = receive_batch(&mut events).await;
        worker
            .command_tx
            .send(WorkerCommand::Commit {
                batch_id: ready.batch_id,
                attempt: ready.attempt,
                explicit_loss: false,
            })
            .unwrap();
        receive_commit(&mut events).await.3.unwrap();
        let mut writer = OpenOptions::new().append(true).open(&source).unwrap();
        writer.write_all(b"tail").unwrap();
        writer.flush().unwrap();
        std::fs::rename(&source, &rotated).unwrap();
        let batch = receive_batch(&mut events).await;
        assert_eq!(batch.record_count, 1);
        if acknowledge {
            worker
                .command_tx
                .send(WorkerCommand::Commit {
                    batch_id: batch.batch_id,
                    attempt: batch.attempt,
                    explicit_loss: false,
                })
                .unwrap();
            receive_commit(&mut events).await.3.unwrap();
        }

        stop_worker(worker, &mut events).await.unwrap();

        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        let record = store.table().iter().next().unwrap().1;
        if acknowledge {
            assert_eq!(record.committed_offset, 10);
            assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
        } else {
            assert_eq!(record.committed_offset, 6);
            assert_eq!(record.lifecycle_state, LifecycleState::Active);
        }
    }
}

#[cfg(unix)]
/// Scenario: Shutdown is queued immediately behind the matching Ack for an
/// exact-bound rotation-tail batch.
/// Guarantees: the tail delta already carries finalization, so worker exit
/// cannot expose an Acked tail whose durable lifecycle is still active.
#[tokio::test]
async fn exact_bound_rotation_tail_finalizes_in_its_ack_transaction() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("atomic-tail.log");
    let rotated = directory.path().join("atomic-tail.log.1");
    std::fs::write(&source, b"ready\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.framing.force_flush_period = Duration::from_secs(10);
    runtime.rotation.rotate_wait = Duration::from_millis(30);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let ready = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: ready.batch_id,
            attempt: ready.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    let mut writer = OpenOptions::new().append(true).open(&source).unwrap();
    writer.write_all(b"tail").unwrap();
    writer.flush().unwrap();
    std::fs::rename(&source, &rotated).unwrap();
    let tail = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: tail.batch_id,
            attempt: tail.attempt,
            explicit_loss: false,
        })
        .unwrap();
    worker.command_tx.send(WorkerCommand::Shutdown).unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 10);
    assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
}

#[cfg(unix)]
/// Scenario: the EOF discovery cadence is much longer than `rotate_wait`
/// after a late write to a removed file.
/// Guarantees: finalization runs at the rotation deadline rather than the
/// next ordinary EOF probe, leaving the old identity finalized before stop.
#[tokio::test]
async fn rotate_wait_is_not_rounded_up_to_discovery_poll_interval() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("deadline.log");
    let rotated = directory.path().join("deadline.log.1");
    std::fs::write(&source, b"old\n").unwrap();
    let mut late_writer = OpenOptions::new().append(true).open(&source).unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.discovery.poll_interval = Duration::from_millis(500);
    runtime.rotation.rotate_wait = Duration::from_millis(40);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let initial = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: initial.batch_id,
            attempt: initial.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    std::fs::rename(&source, &rotated).unwrap();
    std::fs::write(&source, b"new\n").unwrap();
    let replacement = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: replacement.batch_id,
            attempt: replacement.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    late_writer.write_all(b"late\n").unwrap();
    late_writer.flush().unwrap();
    let late = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: late.batch_id,
            attempt: late.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    drop(late_writer);
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert!(store.table().iter().any(|(_, record)| {
        record.lifecycle_state == LifecycleState::RotatedFinalized && record.committed_offset == 9
    }));
}

#[cfg(unix)]
/// Scenario: Drain arrives after removal starts a long rotation wait but
/// before the EOF stability interval can expire.
/// Guarantees: drain stays command-responsive, does not invent finalization,
/// and releases the descriptor and lease without waiting for `rotate_wait`.
#[tokio::test]
async fn drain_interrupts_rotation_wait_without_false_finalization() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("drain-rotate.log");
    let rotated = directory.path().join("drain-rotate.log.1");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.rotation.rotate_wait = Duration::from_secs(5);
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
    std::fs::rename(&source, &rotated).unwrap();
    worker.command_tx.send(WorkerCommand::Drain).unwrap();

    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, WorkerEvent::Drained));
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 5);
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
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

/// Scenario: an admitted nonresident reader's path is replaced before
/// discovery can deliver the old locator's `Removed` event.
/// Guarantees: reopen reports descriptor unavailability, the worker durably
/// quarantines the old identity, and retains it for later discovery cleanup.
#[test]
fn pre_discovery_path_replacement_uses_per_file_containment() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("replace-before-discovery.log");
    let rotated = directory.path().join("replace-before-discovery.log.1");
    std::fs::write(&source, b"old\n").unwrap();
    let runtime_config = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    let mut runtime = WorkerRuntime::new(runtime_config).unwrap();
    runtime.readers.take().unwrap().shutdown().unwrap();

    let resolved_path = std::fs::canonicalize(&source).unwrap();
    let opened = open_candidate(&resolved_path, false, 16, 0).unwrap();
    let evidence = opened.evidence;
    let locator = evidence.locator;
    let file_id = FileId::from_bytes([92; 16]);
    let _outcomes = runtime
        .store
        .register_files(vec![RegisterFile {
            file_id,
            file_epoch: 1,
            committed_offset: 0,
            fingerprint: evidence.fingerprint.clone(),
            ignored_header_bytes: 0,
            locator,
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: runtime.config.framing_profile_digest,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 1,
            advisory_path: evidence.advisory_path.clone(),
        }])
        .unwrap();
    let mut readers = ReaderTable::new(ReaderSettings::from_runtime(&runtime.config)).unwrap();
    readers
        .insert(
            DiscoveredCandidate {
                matched_path: source.clone(),
                resolved_path,
                evidence,
                modified: None,
            },
            ResolvedIdentity {
                file_id,
                file_epoch: 1,
                committed_offset: 0,
                framing_resume: FramingResume::Clean,
                lifecycle_state: LifecycleState::Active,
                matched_by: IdentityMatch::NewDiscovery,
            },
        )
        .unwrap();
    runtime.readers = Some(readers);

    std::fs::rename(&source, &rotated).unwrap();
    std::fs::write(&source, b"new\n").unwrap();
    assert!(matches!(
        runtime.readers_mut().unwrap().poll(Instant::now()).unwrap(),
        ReaderPoll::RemovedWithoutDescriptor {
            file_id: unavailable
        } if unavailable == file_id
    ));
    let released = runtime
        .contain_removed_without_descriptor(file_id)
        .unwrap()
        .expect("descriptor quarantine was not cancelled");
    runtime
        .remember_inactive_locator(released, file_id)
        .unwrap();

    let record = runtime.store.table().get(&file_id).unwrap();
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(
        record.quarantine_evidence.as_ref().unwrap().reason_code,
        QUARANTINE_REASON_ROTATION_DESCRIPTOR_UNAVAILABLE
    );
    assert_eq!(runtime.inactive_locators.get(&locator), Some(&file_id));
    runtime.shutdown_resources().unwrap();
}

/// Scenario: source-only reconciliation and rotation deadlines expire while
/// downstream retention pauses all source work.
/// Guarantees: the source-work wait can be due immediately, but the retained
/// command wait ignores those deadlines and remains bounded above zero.
#[test]
fn paused_source_deadlines_do_not_zero_the_command_wait() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("wait.log");
    let runtime_config = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    let mut runtime = WorkerRuntime::new(runtime_config).unwrap();
    let expired = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .unwrap();
    let _ = runtime.rotation_waits.insert(
        FileId::from_bytes([91; 16]),
        RotationWait {
            stable_since: expired,
            deadline: expired,
        },
    );
    runtime.pending_reconciliation_retry_at = Some(expired);

    assert_eq!(runtime.next_wait(None), Duration::ZERO);
    assert!(runtime.next_maintenance_wait() > Duration::ZERO);
    runtime.shutdown_resources().unwrap();
}

/// Scenario: Drain and a matching Ack commit are buffered while out-of-band
/// forced-shutdown cancellation is asserted before the worker resumes.
/// Guarantees: priority cancellation dequeues neither command and restart
/// observes no durable progress from the retained batch.
#[test]
fn forced_shutdown_preempts_queued_commit_progress() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("forced-shutdown.log");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let config = runtime_config(source.to_str().unwrap(), &namespace, 1);
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let mut runtime = WorkerRuntime::new_with_telemetry(
        config.clone(),
        Arc::new(WorkerTelemetryBridge::default()),
        Arc::clone(&shutdown_requested),
    )
    .unwrap();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let (command_tx, command_rx) = sync_channel(4);
    let deadline = Instant::now() + Duration::from_secs(5);

    while runtime.retained.is_none() {
        assert_eq!(
            runtime
                .process_discovery_message(&event_tx, &command_rx)
                .unwrap(),
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
            other => panic!("unexpected reader state before retention: {other:?}"),
        }
        assert!(Instant::now() < deadline, "worker did not retain a batch");
    }
    let batch = match event_rx.try_recv().unwrap() {
        WorkerEvent::Batch(batch) => batch,
        other => panic!("expected retained batch, got {other:?}"),
    };
    command_tx.send(WorkerCommand::Drain).unwrap();
    command_tx
        .send(WorkerCommand::Commit {
            batch_id: batch.batch_id,
            attempt: batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    shutdown_requested.store(true, Ordering::Release);

    runtime.run(&event_tx, &command_rx).unwrap();

    assert!(matches!(command_rx.try_recv(), Ok(WorkerCommand::Drain)));
    assert!(matches!(
        command_rx.try_recv(),
        Ok(WorkerCommand::Commit { .. })
    ));
    runtime.shutdown_resources().unwrap();
    drop(runtime);
    drop(event_rx);
    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&config)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 0);
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
}

/// Scenario: worker startup is waiting for a checkpoint namespace owned by
/// another store when forced-shutdown cancellation is asserted.
/// Guarantees: lock acquisition exits cleanly before ownership_timeout,
/// emits no failure event, and leaves the namespace available to a successor.
#[test]
fn forced_shutdown_preempts_checkpoint_namespace_wait() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("namespace-wait.log");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let config = runtime_config(source.to_str().unwrap(), &namespace, 1);
    let owner =
        CheckpointStore::open(StoreOptions::from_runtime_config(&config)).expect("owner opens");
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(config.clone(), event_tx).unwrap();

    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !worker.join.is_finished(),
        "worker unexpectedly finished while namespace remained owned"
    );
    worker.shutdown_requested.store(true, Ordering::Release);
    drop(worker.command_tx);
    worker.join.join().unwrap().unwrap();

    assert!(matches!(event_rx.try_recv(), Ok(WorkerEvent::Stopped)));
    assert!(event_rx.try_recv().is_err());
    drop(owner);
    let successor =
        CheckpointStore::open(StoreOptions::from_runtime_config(&config)).expect("successor opens");
    assert_eq!(successor.generation(), 0);
}

/// Scenario: a selected source is gated immediately before its read syscall
/// while forced-shutdown cancellation is asserted.
/// Guarantees: no source read, decode quarantine, or checkpoint progress
/// starts after cancellation becomes observable.
#[test]
fn forced_shutdown_after_blocked_poll_suppresses_decode_quarantine() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("blocked-poll.log");
    std::fs::write(&source, [0xff, b'\n']).unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut config = runtime_config(source.to_str().unwrap(), &namespace, 1);
    config.on_decode_error = OnDecodeError::Fail;
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let telemetry = Arc::new(WorkerTelemetryBridge::default());
    let mut runtime = WorkerRuntime::new_with_telemetry(
        config.clone(),
        Arc::clone(&telemetry),
        Arc::clone(&shutdown_requested),
    )
    .unwrap();
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let (command_tx, command_rx) = sync_channel(4);
    let deadline = Instant::now() + Duration::from_secs(5);

    while runtime.readers_ref().unwrap().stats().tracked_readers == 0 {
        assert!(Instant::now() < deadline, "discovery did not admit source");
        assert_eq!(
            runtime
                .process_discovery_message(&event_tx, &command_rx)
                .unwrap(),
            LoopControl::Continue
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let poll_gate = runtime
        .readers_mut()
        .unwrap()
        .gate_next_source_read_for_test();
    let worker = std::thread::spawn(move || {
        let result = runtime.run(&event_tx, &command_rx);
        let shutdown_result = runtime.shutdown_resources();
        result.and(shutdown_result)
    });

    assert!(
        poll_gate.wait_until_entered(Duration::from_secs(5)),
        "worker did not reach the gated source read"
    );
    shutdown_requested.store(true, Ordering::Release);
    poll_gate.release();
    worker.join().unwrap().unwrap();
    drop(command_tx);

    assert_eq!(
        telemetry.counter_for_test(WorkerCounter::SourceBytesRead),
        0
    );
    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&config)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 0);
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
    assert!(record.quarantine_evidence.is_none());
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
            runtime
                .process_discovery_message(&event_tx, &command_rx)
                .unwrap(),
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
    assert!(
        runtime
            .commit_retained(first.batch_id, first.attempt)
            .unwrap()
    );

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
    assert!(
        runtime
            .commit_retained(second.batch_id, second.attempt)
            .unwrap()
    );

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
            runtime
                .process_discovery_message(&event_tx, &command_rx)
                .unwrap(),
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
    assert!(
        runtime
            .commit_retained(first.batch_id, first.attempt)
            .unwrap()
    );

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
    assert!(
        runtime
            .commit_retained(second.batch_id, second.attempt)
            .unwrap()
    );
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
