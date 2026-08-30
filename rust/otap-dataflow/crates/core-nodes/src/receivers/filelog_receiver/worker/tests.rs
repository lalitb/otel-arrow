// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;
use std::{fs::OpenOptions, io::Write};

use otap_df_engine::testing::setup_test_runtime;
use otap_df_pdata::{
    otap::OtapArrowRecords,
    otlp::{ProtoBuffer, ProtoBytesEncoder, logs::LogsProtoBytesEncoder},
    proto::opentelemetry::{
        arrow::v1::ArrowPayloadType,
        collector::logs::v1::ExportLogsServiceRequest,
        common::v1::{KeyValue, any_value::Value},
        logs::v1::LogRecord,
    },
};
use prost::Message;
use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::receivers::filelog_receiver::MaxLogSizeBehavior;
use crate::receivers::filelog_receiver::batching::{FinalizationOutcome, ProgressFrontier};
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    AdvisoryPath, CommittedFrontierGuard, FRAMING_PROFILE_VERSION, FramingResume, Locator,
    QUARANTINE_REASON_DECODE, QUARANTINE_REASON_TRUNCATE,
};
use crate::receivers::filelog_receiver::checkpoint::store::fault::FaultPoint;
use crate::receivers::filelog_receiver::checkpoint::wal::{RegisterFile, UpdateProgress};
use crate::receivers::filelog_receiver::config::{
    ATTR_KEY_FLUSH_REASON, ATTR_KEY_FRAGMENT_ID, ATTR_KEY_FRAGMENT_INDEX, ATTR_KEY_FRAGMENT_LAST,
    ATTR_KEY_LOG_FILE_NAME, Config, OnDecodeError,
};
use crate::receivers::filelog_receiver::discovery::DiscoveredCandidate;
use crate::receivers::filelog_receiver::discovery::scanner::DiscoveryPlan;
use crate::receivers::filelog_receiver::discovery::source::spawn_discovery;
use crate::receivers::filelog_receiver::identity::matcher::{IdentityMatch, ResolvedIdentity};
use crate::receivers::filelog_receiver::identity::platform::open_candidate;

/// Test-only zero-filled window guard: a deterministic, obviously-fake
/// `CommittedFrontierGuard` for tests that only need a structurally valid
/// guard and do not exercise real continuity evidence. Production code
/// must never do this; see
/// `crate::receivers::filelog_receiver::checkpoint::primitives::CommittedFrontierWindow`
/// for the real, non-fabricated runtime window.
fn zero_guard(committed_offset: u64) -> CommittedFrontierGuard {
    let window_len = committed_offset.min(64) as usize;
    CommittedFrontierGuard::compute(committed_offset, &vec![0u8; window_len]).unwrap()
}

fn runtime_config(
    include: &str,
    namespace_dir: &std::path::Path,
    max_records: u32,
) -> RuntimeConfig {
    runtime_config_with(include, namespace_dir, max_records, |_| {})
}

fn runtime_config_with(
    include: &str,
    namespace_dir: &std::path::Path,
    max_records: u32,
    configure: impl FnOnce(&mut Config),
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
    configure(&mut config);
    let mut runtime = RuntimeConfig::from_config(config, "").unwrap();
    runtime.checkpoint_namespace_dir = namespace_dir.to_path_buf();
    runtime
}

fn decode_worker_records(records: &mut OtapArrowRecords) -> ExportLogsServiceRequest {
    let mut encoder = LogsProtoBytesEncoder::new();
    let mut buffer = ProtoBuffer::default();
    encoder
        .encode(records, &mut buffer)
        .expect("worker OTAP logs encode to OTLP");
    ExportLogsServiceRequest::decode(buffer.as_ref()).expect("worker OTLP logs decode")
}

fn only_log(request: &ExportLogsServiceRequest) -> &LogRecord {
    let logs: Vec<&LogRecord> = request
        .resource_logs
        .iter()
        .flat_map(|resource| &resource.scope_logs)
        .flat_map(|scope| &scope.log_records)
        .collect();
    assert_eq!(logs.len(), 1);
    logs[0]
}

fn log_body_bytes(log: &LogRecord) -> &[u8] {
    match log.body.as_ref().and_then(|body| body.value.as_ref()) {
        Some(Value::StringValue(value)) => value.as_bytes(),
        Some(Value::BytesValue(value)) => value,
        other => panic!("expected one string or byte log body, got {other:?}"),
    }
}

fn log_attr<'a>(log: &'a LogRecord, key: &str) -> Option<&'a Value> {
    log.attributes
        .iter()
        .find(|KeyValue { key: found, .. }| found == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
}

/// Scenario: framed outputs and lifecycle transitions cover decode policies,
/// record truncation, splitting, bounded flushes, copy-truncate policies,
/// descriptor-unavailable rotation, rotation finalization, and checkpoint
/// maintenance.
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
    record_descriptor_unavailable_telemetry(&telemetry);
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
    receive_batch_with_timeout(events, Duration::from_secs(5)).await
}

async fn receive_batch_with_timeout(
    events: &mut tokio::sync::mpsc::Receiver<WorkerEvent>,
    timeout: Duration,
) -> WorkerBatch {
    loop {
        let event = tokio::time::timeout(timeout, events.recv())
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

async fn wait_for_worker_counter_at_least(
    telemetry: &WorkerTelemetryBridge,
    counter: WorkerCounter,
    minimum: u64,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if telemetry.counter_for_test(counter) >= minimum {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("worker telemetry counter minimum timeout");
}

async fn wait_for_worker_gauge(
    telemetry: &WorkerTelemetryBridge,
    gauge: WorkerGauge,
    expected: u64,
) {
    wait_for_worker_gauge_with_timeout(telemetry, gauge, expected, Duration::from_secs(5)).await;
}

async fn wait_for_worker_gauge_with_timeout(
    telemetry: &WorkerTelemetryBridge,
    gauge: WorkerGauge,
    expected: u64,
    timeout: Duration,
) {
    tokio::time::timeout(timeout, async {
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

/// Scenario: a split record's first fragment is Acked, the worker stops, and
/// a new worker resumes the same unchanged source.
/// Guarantees: the Ack persists continuation offset and index atomically, so
/// restart emits the next fragment with the same ID and no duplicate bytes.
#[tokio::test]
async fn acked_split_fragment_resumes_across_worker_restart() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("fragment.log");
    std::fs::write(&source, b"abcdefgh\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config_with(source.to_str().unwrap(), &namespace, 1, |config| {
        config.framing.max_line_bytes = 64;
        config.framing.max_record_bytes = 4;
        config.framing.max_log_size_behavior = MaxLogSizeBehavior::Split;
    });

    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let mut first = receive_batch(&mut events).await;
    let first_request = decode_worker_records(&mut first.records);
    let first_log = only_log(&first_request);
    assert_eq!(log_body_bytes(first_log), b"abcd");
    let fragment_id = match log_attr(first_log, ATTR_KEY_FRAGMENT_ID) {
        Some(Value::StringValue(value)) => value.clone(),
        other => panic!("expected fragment ID, got {other:?}"),
    };
    assert_eq!(
        log_attr(first_log, ATTR_KEY_FRAGMENT_INDEX),
        Some(&Value::IntValue(0))
    );
    assert_eq!(
        log_attr(first_log, ATTR_KEY_FRAGMENT_LAST),
        Some(&Value::BoolValue(false))
    );
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: first.batch_id,
            attempt: first.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let checkpoint = store.table().iter().next().unwrap().1;
    assert_eq!(checkpoint.committed_offset, 4);
    assert_eq!(
        checkpoint.framing_resume,
        FramingResume::Continuation {
            record_start_offset: 0,
            record_end_offset: 0,
            next_fragment_index: 1,
        }
    );
    drop(store);

    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let mut second = receive_batch(&mut events).await;
    let second_request = decode_worker_records(&mut second.records);
    let second_log = only_log(&second_request);
    assert_eq!(log_body_bytes(second_log), b"efgh");
    assert_eq!(
        log_attr(second_log, ATTR_KEY_FRAGMENT_ID),
        Some(&Value::StringValue(fragment_id))
    );
    assert_eq!(
        log_attr(second_log, ATTR_KEY_FRAGMENT_INDEX),
        Some(&Value::IntValue(1))
    );
    assert_eq!(
        log_attr(second_log, ATTR_KEY_FRAGMENT_LAST),
        Some(&Value::BoolValue(true))
    );
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: second.batch_id,
            attempt: second.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let checkpoint = store.table().iter().next().unwrap().1;
    assert_eq!(checkpoint.committed_offset, 9);
    assert_eq!(checkpoint.framing_resume, FramingResume::Clean);
}

/// Scenario: an unterminated record is timeout-flushed and Acked, then a new
/// worker starts after a complete record is appended.
/// Guarantees: restart begins at the Acked partial frontier and emits only
/// newly appended bytes, without duplicating or skipping source content.
#[tokio::test]
async fn acked_partial_flush_resumes_cleanly_across_worker_restart() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("partial.log");
    std::fs::write(&source, b"partial").unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config_with(source.to_str().unwrap(), &namespace, 1, |config| {
        config.framing.force_flush_period = Duration::from_millis(10);
    });

    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let mut partial = receive_batch(&mut events).await;
    let partial_request = decode_worker_records(&mut partial.records);
    let partial_log = only_log(&partial_request);
    assert_eq!(log_body_bytes(partial_log), b"partial");
    assert_eq!(
        log_attr(partial_log, ATTR_KEY_FLUSH_REASON),
        Some(&Value::StringValue("timeout".to_owned()))
    );
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: partial.batch_id,
            attempt: partial.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    {
        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        let checkpoint = store.table().iter().next().unwrap().1;
        assert_eq!(checkpoint.committed_offset, 7);
        assert_eq!(checkpoint.framing_resume, FramingResume::Clean);
    }

    OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"next\n")
        .unwrap();
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let mut next = receive_batch(&mut events).await;
    let next_request = decode_worker_records(&mut next.records);
    let next_log = only_log(&next_request);
    assert_eq!(log_body_bytes(next_log), b"next");
    assert!(log_attr(next_log, ATTR_KEY_FLUSH_REASON).is_none());
    for key in [
        ATTR_KEY_FRAGMENT_ID,
        ATTR_KEY_FRAGMENT_INDEX,
        ATTR_KEY_FRAGMENT_LAST,
    ] {
        assert!(
            log_attr(next_log, key).is_none(),
            "clean restart unexpectedly emitted fragment attribute {key}"
        );
    }
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: next.batch_id,
            attempt: next.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let checkpoint = store.table().iter().next().unwrap().1;
    assert_eq!(checkpoint.committed_offset, 12);
    assert_eq!(checkpoint.framing_resume, FramingResume::Clean);
}

/// Scenario: two sequential Acks advance one file's committed offset past
/// its starting nonzero window, with the second advance well below the
/// 64-byte guard window.
/// Guarantees: the second Ack's durably persisted committed-frontier guard
/// is computed from the real trailing bytes spanning both the first Ack's
/// already-committed region and the newly consumed bytes -- never only the
/// most recent write's bytes -- proving the reader-retained window combines
/// prior and new evidence across a real Ack-driven framer reconstruction
/// (the framer is discarded at every batch seal and rebuilt from the
/// reader's retained bytes, never a reread of the source).
#[tokio::test]
async fn second_ack_below_64_bytes_combines_prior_and_new_committed_frontier_window() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("combine-window.log");
    let line1 = vec![b'a'; 60];
    let line2 = vec![b'b'; 10];
    let mut content = Vec::new();
    content.extend_from_slice(&line1);
    content.push(b'\n');
    content.extend_from_slice(&line2);
    content.push(b'\n');
    std::fs::write(&source, &content).unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);

    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    let mut first = receive_batch(&mut events).await;
    let first_request = decode_worker_records(&mut first.records);
    assert_eq!(log_body_bytes(only_log(&first_request)), line1.as_slice());
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: first.batch_id,
            attempt: first.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    let mut second = receive_batch(&mut events).await;
    let second_request = decode_worker_records(&mut second.records);
    assert_eq!(log_body_bytes(only_log(&second_request)), line2.as_slice());
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: second.batch_id,
            attempt: second.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 72);
    // The last 64 bytes ending at offset 72: the tail of the first line
    // (skipping its first 8 bytes, already outside the window) plus its
    // terminating LF, followed by the entire second line -- prior evidence
    // from the first Ack combined with new evidence from the second.
    let expected_bytes = content[content.len() - 64..].to_vec();
    assert_eq!(expected_bytes.len(), 64);
    let expected_guard = CommittedFrontierGuard::compute(72, &expected_bytes).unwrap();
    assert_eq!(record.committed_frontier_guard, expected_guard);
}

/// Scenario: under `max_open_files: 1`, a second file's admission evicts the
/// first file's resident descriptor and in-memory framer, then the first
/// file receives a new advance well below 64 bytes before it is reopened.
/// Guarantees: reopen independently rereads and revalidates the real
/// committed-frontier window from disk (never trusting stale in-memory
/// bytes across the evicted descriptor), and the reconstructed framer
/// correctly combines that freshly reread tail with the short new advance
/// into the exact real trailing window, never a partial or fabricated one.
#[tokio::test]
async fn descriptor_eviction_then_short_advance_combines_reread_committed_frontier_window() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("first.log");
    let second = directory.path().join("second.log");
    let line1 = vec![b'a'; 70];
    let mut content = Vec::new();
    content.extend_from_slice(&line1);
    content.push(b'\n');
    std::fs::write(&first, &content).unwrap();
    let include = directory.path().join("*.log");
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(include.to_str().unwrap(), &namespace, 1);
    runtime.limits.max_open_files = 1;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    // First Ack establishes a real, nonzero committed-frontier window for
    // `first.log` strictly larger than the 64-byte guard bound.
    let first_batch = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: first_batch.batch_id,
            attempt: first_batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    // Admitting and reading `second.log` under `max_open_files: 1` evicts
    // `first.log`'s resident descriptor and discards its in-memory framer
    // (see `Worker::seal_open_batch`'s `clear_framers`, plus the eviction
    // path's own `discard_framer`).
    std::fs::write(&second, b"one\n").unwrap();
    let second_batch = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: second_batch.batch_id,
            attempt: second_batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();

    // A short new advance, well below 64 bytes, arrives on the now-evicted
    // `first.log`. Serving it requires reopening the file (evicting
    // `second.log`'s descriptor in turn under the same `max_open_files: 1`
    // bound) and independently rereading and revalidating the real
    // committed-frontier window from disk before any new byte is combined.
    let short_line = b"shortline\n".to_vec();
    content.extend_from_slice(&short_line);
    let mut handle = OpenOptions::new().append(true).open(&first).unwrap();
    handle.write_all(&short_line).unwrap();
    drop(handle);

    let third_batch = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: third_batch.batch_id,
            attempt: third_batch.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    let record = store
        .table()
        .iter()
        .map(|(_, record)| record)
        .find(|record| record.committed_offset > 10)
        .expect("first.log's record has the larger committed offset");
    assert_eq!(record.committed_offset, content.len() as u64);
    let expected_bytes = content[content.len() - 64..].to_vec();
    assert_eq!(expected_bytes.len(), 64);
    let expected_guard =
        CommittedFrontierGuard::compute(content.len() as u64, &expected_bytes).unwrap();
    assert_eq!(record.committed_frontier_guard, expected_guard);
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
/// Guarantees: the exact quarantine retries once within the checkpoint
/// failure budget, success is reported only after required sync, and the
/// worker remains operational with durable quarantine state.
#[tokio::test]
async fn decode_quarantine_faults_retry_before_success() {
    for point in [
        FaultPoint::BeforeWalTransactionWrite,
        FaultPoint::BeforeWalSync,
        FaultPoint::AfterWalSync,
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

        wait_for_worker_counter(&telemetry, WorkerCounter::QuarantineDecode, 1).await;
        assert_eq!(telemetry.counter_for_test(WorkerCounter::DecodeFailures), 1);
        assert_eq!(
            telemetry.counter_for_test(WorkerCounter::QuarantineDecode),
            1
        );
        assert_eq!(
            telemetry.counter_for_test(WorkerCounter::CheckpointFailures),
            1
        );
        assert_eq!(telemetry.gauge_for_test(WorkerGauge::FilesQuarantined), 1);
        stop_worker(worker, &mut events).await.unwrap();

        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        let record = store.table().iter().next().unwrap().1;
        assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
        assert_eq!(record.committed_offset, 0);
    }
}

/// Scenario: each WAL append/sync boundary fails once while discovery is
/// registering a newly observed file.
/// Guarantees: the retained identity plan retries the exact transaction,
/// preserves its generated file ID, and emits the source batch without
/// terminating the worker.
#[tokio::test]
async fn identity_registration_retries_each_wal_fault_boundary() {
    for point in FaultPoint::WAL_DURABILITY {
        let directory = tempdir().unwrap();
        let source = directory.path().join("identity-retry.log");
        std::fs::write(&source, b"line\n").unwrap();
        let namespace = directory.path().join("checkpoint");
        let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
        runtime.checkpoint.max_consecutive_failures = 2;
        let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let worker = spawn_worker_with_store_fault(runtime.clone(), event_tx, point, 0).unwrap();

        let batch = receive_batch(&mut events).await;
        assert_eq!(batch.record_count, 1);
        assert_eq!(
            worker
                .telemetry
                .counter_for_test(WorkerCounter::CheckpointFailures),
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
        stop_worker(worker, &mut events).await.unwrap();

        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        assert_eq!(store.table().len(), 1);
        assert_eq!(store.table().iter().next().unwrap().1.committed_offset, 5);
    }
}

/// Scenario: a direct identity-registration append fails with a configured
/// checkpoint failure budget of one attempt.
/// Guarantees: no retry exceeds the configured bound, the worker terminates
/// after one reported failure, and no registration becomes durable.
#[tokio::test]
async fn direct_checkpoint_retry_honors_the_failure_budget() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("identity-budget.log");
    std::fs::write(&source, b"line\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.checkpoint.max_consecutive_failures = 1;
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker_with_store_fault(
        runtime.clone(),
        event_tx,
        FaultPoint::BeforeWalTransactionWrite,
        0,
    )
    .unwrap();
    let telemetry = Arc::clone(&worker.telemetry);

    let failure = loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("worker failure timeout")
            .expect("worker event channel closed")
        {
            WorkerEvent::Failed(message) => break message,
            WorkerEvent::Stopped => panic!("worker stopped without failure evidence"),
            WorkerEvent::Batch(_) | WorkerEvent::CommitResult { .. } | WorkerEvent::Drained => {}
        }
    };
    assert!(failure.contains("checkpoint"), "{failure}");
    assert_eq!(
        telemetry.counter_for_test(WorkerCounter::CheckpointFailures),
        1
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
    assert!(store.table().is_empty());
}

/// Scenario: each WAL append/sync boundary fails once for an Ack transaction,
/// then the same retained commit is retried.
/// Guarantees: every first failure returns a CommitResult, each exact bounded
/// retry succeeds, the retained batch is released once, and reopen observes
/// one committed progress transaction.
#[tokio::test]
async fn checkpoint_fault_commits_on_the_exact_bounded_retry() {
    for point in FaultPoint::WAL_DURABILITY {
        let directory = tempdir().unwrap();
        let source = directory.path().join("fault.log");
        std::fs::write(&source, b"line\n").unwrap();
        let namespace = directory.path().join("checkpoint");
        let runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
        let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let worker = spawn_worker_with_store_fault(runtime.clone(), event_tx, point, 1).unwrap();

        let first = receive_batch(&mut events).await;
        assert_eq!((first.batch_id, first.attempt), (1, 1));
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
        assert!(result.is_err(), "{point} must fail the first commit");

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
        result.unwrap_or_else(|error| panic!("the exact retry after {point} failed: {error}"));
        stop_worker(worker, &mut events).await.unwrap();

        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        let record = store.table().iter().next().unwrap().1;
        assert_eq!(record.committed_offset, 5);
        assert_eq!(store.recovery().transactions_replayed, 2);
    }
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
            committed_frontier_guard: zero_guard(0),
            fingerprint: b"0123456789abcdef".to_vec(),
            ignored_header_bytes: 0,
            locator: Locator::PosixDevIno { dev: 1, ino: 88 },
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: runtime.framing_profile_digest,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 1,
            advisory_path: AdvisoryPath::from_unix_bytes(b"unused.log").unwrap(),
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
            new_committed_frontier_guard: zero_guard(0),
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
        new_committed_frontier_guard: zero_guard(8),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: record.last_seen_time_unix_nano,
        finalize: false,
    }]);
    assert!(stale.is_err());
    let unchanged = store.table().get(&file_id).unwrap();
    assert_eq!((unchanged.file_epoch, unchanged.committed_offset), (2, 4));
}

/// Scenario: finalized history and its active replacement share one reused
/// locator, then discovery emits an `Updated` event for the replacement.
/// Guarantees: truncation detection consults only the sole live locator claim
/// and does not reject valid retained `RotatedFinalized` history.
#[test]
fn updated_reused_locator_ignores_finalized_history() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("reused.log");
    std::fs::write(&source, b"new\n").unwrap();
    let runtime_config = runtime_config(
        source.to_str().unwrap(),
        &directory.path().join("checkpoint"),
        1,
    );
    let mut runtime = WorkerRuntime::new(runtime_config).unwrap();
    runtime.readers.take().unwrap().shutdown().unwrap();

    let resolved_path = std::fs::canonicalize(&source).unwrap();
    let evidence = open_candidate(&source, false, 16, 0).unwrap().evidence;
    let old_file_id = FileId::from_bytes([94; 16]);
    let new_file_id = FileId::from_bytes([95; 16]);
    let registration = |file_id| RegisterFile {
        file_id,
        file_epoch: 1,
        committed_offset: 0,
        committed_frontier_guard: CommittedFrontierGuard::empty(),
        fingerprint: evidence.fingerprint.clone(),
        ignored_header_bytes: 0,
        locator: evidence.locator,
        framing_profile_version: FRAMING_PROFILE_VERSION,
        framing_profile_digest: runtime.config.framing_profile_digest,
        framing_resume: FramingResume::Clean,
        last_seen_time_unix_nano: 1,
        advisory_path: evidence.advisory_path.clone(),
    };
    let _registered = runtime
        .store
        .register_files(vec![registration(old_file_id)])
        .unwrap();
    let _finalized = runtime
        .store
        .commit_progress(vec![UpdateProgress {
            file_id: old_file_id,
            expected_committed_offset: 0,
            expected_file_epoch: 1,
            new_committed_offset: 0,
            new_committed_frontier_guard: CommittedFrontierGuard::empty(),
            new_framing_resume: FramingResume::Clean,
            new_last_seen_time_unix_nano: 2,
            finalize: true,
        }])
        .unwrap();
    let _replacement = runtime
        .store
        .register_files(vec![registration(new_file_id)])
        .unwrap();

    let candidate = DiscoveredCandidate {
        matched_path: source,
        resolved_path,
        evidence: evidence.clone(),
        modified: None,
    };
    let mut readers = ReaderTable::new(ReaderSettings::from_runtime(&runtime.config)).unwrap();
    readers
        .insert(
            candidate.clone(),
            ResolvedIdentity {
                file_id: new_file_id,
                file_epoch: 1,
                committed_offset: 0,
                framing_resume: FramingResume::Clean,
                lifecycle_state: LifecycleState::Active,
                matched_by: IdentityMatch::RecoveryMismatch,
                committed_frontier_guard: CommittedFrontierGuard::empty(),
                advisory_path: evidence.advisory_path,
            },
        )
        .unwrap();
    runtime.readers = Some(readers);

    runtime
        .detect_updated_truncations(&[CandidateEvent::Updated(candidate)])
        .unwrap();
    assert!(runtime.detected_truncations.is_empty());
    assert_eq!(
        runtime
            .store
            .table()
            .get(&old_file_id)
            .unwrap()
            .lifecycle_state,
        LifecycleState::RotatedFinalized
    );
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
            committed_frontier_guard: zero_guard(4),
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
                committed_frontier_guard: zero_guard(4),
                advisory_path: evidence.advisory_path.clone(),
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
/// Guarantees: each exact transition retries within the checkpoint failure
/// budget, reports success only after required sync, and remains durable
/// without terminating the worker.
#[tokio::test]
async fn truncate_transitions_retry_at_wal_fault_boundary() {
    for policy in [OnTruncate::Fail, OnTruncate::ReadNew] {
        for point in [
            FaultPoint::BeforeWalTransactionWrite,
            FaultPoint::AfterWalTransactionWrite,
            FaultPoint::BeforeWalSync,
            FaultPoint::AfterWalSync,
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

            match policy {
                OnTruncate::Fail => {
                    wait_for_worker_counter(&telemetry, WorkerCounter::CopytruncateFail, 1).await;
                }
                OnTruncate::ReadNew => {
                    wait_for_worker_counter(&telemetry, WorkerCounter::CopytruncateReadNew, 1)
                        .await;
                }
            }
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CopytruncateDetected),
                1
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CopytruncateFail),
                u64::from(policy == OnTruncate::Fail)
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CopytruncateReadNew),
                u64::from(policy == OnTruncate::ReadNew)
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::QuarantineTruncate),
                u64::from(policy == OnTruncate::Fail)
            );
            assert_eq!(
                telemetry.counter_for_test(WorkerCounter::CheckpointFailures),
                1
            );
            stop_worker(worker, &mut events).await.unwrap();

            let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
            let record = store.table().iter().next().unwrap().1;
            match policy {
                OnTruncate::Fail => {
                    assert_eq!((record.file_epoch, record.committed_offset), (1, 4));
                    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
                    assert_eq!(
                        record.quarantine_evidence.as_ref().unwrap().reason_code,
                        QUARANTINE_REASON_TRUNCATE
                    );
                }
                OnTruncate::ReadNew => {
                    assert_eq!((record.file_epoch, record.committed_offset), (2, 0));
                    assert_eq!(record.lifecycle_state, LifecycleState::Active);
                }
            }
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
/// Guarantees: the affected durable identity remains Active and unfinalized,
/// the volatile reader is released, and unrelated file collection continues.
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
    let unavailable = store
        .table()
        .iter()
        .find(|(_, record)| record.committed_offset == 4)
        .map(|(_, record)| record)
        .expect("descriptor-free rotation must retain its durable record");
    assert_eq!(unavailable.lifecycle_state, LifecycleState::Active);
    assert!(unavailable.quarantine_evidence.is_none());
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
/// Scenario: a matched file is unlinked while both the worker and a writer
/// retain open descriptors, then the writer appends one complete late record.
/// Guarantees: the worker reads and Ack-commits the unlinked inode through its
/// descriptor before finalizing that identity at the stable rotation frontier.
#[tokio::test]
async fn unlink_reads_acked_late_write_before_finalization() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("unlinked.log");
    std::fs::write(&source, b"old\n").unwrap();
    let mut late_writer = OpenOptions::new().append(true).open(&source).unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.rotation.rotate_wait = Duration::from_millis(100);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let telemetry = Arc::clone(&worker.telemetry);

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

    std::fs::remove_file(&source).unwrap();
    wait_for_worker_gauge(&telemetry, WorkerGauge::FilesRemovedWaiting, 1).await;
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

    wait_for_worker_gauge(&telemetry, WorkerGauge::FilesRemovedWaiting, 0).await;
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().len(), 1);
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 9);
    assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
}

#[cfg(windows)]
/// Scenario: a Windows writer and the worker both permit read, write, and
/// delete sharing before the matched name is removed and a late write occurs.
/// Guarantees: name removal or delete-pending state does not revoke the live
/// reader; the late record is Acked before the identity finalizes at offset 9.
#[tokio::test]
async fn windows_compatible_delete_pending_reads_late_write_before_finalization() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let directory = tempdir().unwrap();
    let source = directory.path().join("delete-pending.log");
    std::fs::write(&source, b"old\n").unwrap();
    let mut late_writer = OpenOptions::new()
        .append(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&source)
        .unwrap();
    let namespace = directory.path().join("checkpoint");
    let mut runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    runtime.rotation.rotate_wait = Duration::from_millis(100);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let telemetry = Arc::clone(&worker.telemetry);

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

    std::fs::remove_file(&source).unwrap();
    wait_for_worker_gauge(&telemetry, WorkerGauge::FilesRemovedWaiting, 1).await;
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

    wait_for_worker_gauge(&telemetry, WorkerGauge::FilesRemovedWaiting, 0).await;
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().len(), 1);
    let record = store.table().iter().next().unwrap().1;
    assert_eq!(record.committed_offset, 9);
    assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
}

#[cfg(windows)]
/// Scenario: an existing Windows writer denies every sharing mode while the
/// worker repeatedly discovers a matched file, then releases its handle.
/// Guarantees: sharing violations are counted without reading or registering
/// the file, and normal admission resumes without checkpoint loss after release.
#[tokio::test]
async fn windows_incompatible_sharing_defers_worker_admission_until_release() {
    use std::os::windows::fs::OpenOptionsExt;

    let directory = tempdir().unwrap();
    let source = directory.path().join("exclusive.log");
    std::fs::write(&source, b"line\n").unwrap();
    let exclusive = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&source)
        .unwrap();
    let namespace = directory.path().join("checkpoint");
    let runtime = runtime_config(source.to_str().unwrap(), &namespace, 1);
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

    wait_for_worker_counter_at_least(&worker.telemetry, WorkerCounter::DiscoveryScanErrors, 1)
        .await;
    assert_eq!(
        worker
            .telemetry
            .counter_for_test(WorkerCounter::SourceBytesRead),
        0
    );
    assert_eq!(
        worker.telemetry.gauge_for_test(WorkerGauge::FilesTracked),
        0
    );

    drop(exclusive);
    let admitted = receive_batch(&mut events).await;
    worker
        .command_tx
        .send(WorkerCommand::Commit {
            batch_id: admitted.batch_id,
            attempt: admitted.attempt,
            explicit_loss: false,
        })
        .unwrap();
    receive_commit(&mut events).await.3.unwrap();
    stop_worker(worker, &mut events).await.unwrap();

    let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
    assert_eq!(store.table().len(), 1);
    assert_eq!(store.table().iter().next().unwrap().1.committed_offset, 5);
}

/// Scenario: one continuously readable file competes with at least two
/// thousand one-record files while descriptor residency is capped at 32.
/// Guarantees: every cold file reaches an Acked batch within a bounded number
/// of hot records, and tracked, pending, and open populations respect limits.
#[test]
#[ignore = "resource-intensive thousands-file fairness stress"]
fn thousands_of_files_preserve_fair_progress_and_resource_bounds() {
    const FILES_ENV: &str = "OTAP_FILELOG_FAIRNESS_STRESS_FILES";
    let cold_files = std::env::var(FILES_ENV)
        .map(|value| value.parse::<usize>().expect("stress file count is valid"))
        .unwrap_or(2_000);
    assert!(cold_files > 0);
    let total_files = cold_files.checked_add(1).expect("file count fits usize");
    let total_files_u32 = u32::try_from(total_files).expect("file count fits u32");
    let max_open_files = 32u32.min(total_files_u32);

    let (tokio_runtime, local_tasks) = setup_test_runtime();
    tokio_runtime.block_on(local_tasks.run_until(async {
        let directory = tempdir().unwrap();
        let hot_name = "0000-hot.log";
        let hot_path = directory.path().join(hot_name);
        std::fs::write(&hot_path, b"").unwrap();
        for index in 0..cold_files {
            std::fs::write(directory.path().join(format!("cold-{index:05}.log")), b"").unwrap();
        }

        let pattern = directory.path().join("*.log");
        let namespace = directory.path().join("checkpoint");
        let runtime = runtime_config_with(pattern.to_str().unwrap(), &namespace, 16, |config| {
            config.limits.max_tracked_files = total_files_u32;
            config.limits.max_pending_candidates = total_files_u32;
            config.limits.max_open_files = max_open_files;
            config.limits.max_read_bytes_per_turn = 5;
            config.batch.max_flush_period = Duration::from_millis(20);
        });
        let (event_tx, mut events) =         tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
        let registration_started = Instant::now();
        wait_for_worker_gauge_with_timeout(
        &worker.telemetry,
        WorkerGauge::FilesTracked,
        u64::from(total_files_u32),
        Duration::from_secs(60),
        )
        .await;
        let registration_elapsed = registration_started.elapsed();

        std::fs::write(
        &hot_path,
        "hot0\n".repeat(cold_files.checked_add(256).unwrap()),
        )
        .unwrap();
        for index in 0..cold_files {
        std::fs::write(
            directory.path().join(format!("cold-{index:05}.log")),
            b"cold\n",
        )
        .unwrap();
        }

        let started = Instant::now();
        let mut cold_seen = HashSet::with_capacity(cold_files);
        let mut hot_before_all_cold = 0usize;
        let mut records_before_all_cold = 0usize;
        let maximum_records = cold_files
            .checked_mul(2)
            .and_then(|value| value.checked_add(128))
            .expect("stress record bound fits");
        while cold_seen.len() < cold_files {
            let mut batch = receive_batch(&mut events).await;
            let request = decode_worker_records(&mut batch.records);
            for log in request
                .resource_logs
                .iter()
                .flat_map(|resource| &resource.scope_logs)
                .flat_map(|scope| &scope.log_records)
            {
                if cold_seen.len() == cold_files {
                    break;
                }
                let name = match log_attr(log, ATTR_KEY_LOG_FILE_NAME) {
                    Some(Value::StringValue(value)) => value,
                    other => panic!("expected UTF-8 file name, got {other:?}"),
                };
                records_before_all_cold = records_before_all_cold
                    .checked_add(1)
                    .expect("stress record count fits");
                if name == hot_name {
                    hot_before_all_cold = hot_before_all_cold
                        .checked_add(1)
                        .expect("hot record count fits");
                } else {
                    let inserted = cold_seen.insert(name.clone());
                    assert!(inserted, "cold file emitted more than one source record");
                }
                assert!(
                    records_before_all_cold <= maximum_records,
                    "hot input starved cold-file progress: cold_seen={}, hot={hot_before_all_cold}, \
                     records={records_before_all_cold}, maximum={maximum_records}",
                    cold_seen.len()
                );
                if records_before_all_cold.is_multiple_of(500) {
                    eprintln!(
                        "filelog fairness progress: cold_seen={} \
                         records={records_before_all_cold} hot={hot_before_all_cold}",
                        cold_seen.len()
                    );
                }
            }
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

        assert!(hot_before_all_cold <= cold_files);
        assert_eq!(
            worker.telemetry.gauge_for_test(WorkerGauge::FilesTracked),
            u64::from(total_files_u32)
        );
        assert!(
            worker.telemetry.gauge_for_test(WorkerGauge::FilesPending)
                <= u64::from(total_files_u32)
        );
        assert!(
            worker.telemetry.gauge_for_test(WorkerGauge::FilesOpen) <= u64::from(max_open_files)
        );
        eprintln!(
            "filelog fairness stress: cold_files={cold_files} \
             records_before_all_cold={records_before_all_cold} \
             hot_before_all_cold={hot_before_all_cold} \
             max_open_files={max_open_files} registration_micros={} elapsed_micros={}",
            registration_elapsed.as_micros(),
            started.elapsed().as_micros()
        );
        stop_worker(worker, &mut events).await.unwrap();

        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        assert_eq!(store.table().len(), total_files);
        assert!(
            store
                .table()
                .iter()
                .all(|(_, checkpoint)| checkpoint.committed_offset > 0)
        );
    }));
}

#[cfg(unix)]
/// Scenario: a bounded population is registered and Acked, every matched file
/// is move/create rotated at once, and checkpoint compaction is due after each
/// transaction.
/// Guarantees: the storm preserves all old and replacement identities,
/// finalizes every old descriptor, and repeatedly compacts without data loss.
#[test]
#[ignore = "resource-intensive registration, rotation, and compaction storm"]
fn registration_rotation_and_compaction_storm_preserves_every_identity() {
    const FILES_ENV: &str = "OTAP_FILELOG_STORM_FILES";
    let files = std::env::var(FILES_ENV)
        .map(|value| value.parse::<usize>().expect("storm file count is valid"))
        .unwrap_or(128);
    assert!(files > 0);
    let tracked = files.checked_mul(2).expect("tracked population fits");
    let files_u32 = u32::try_from(files).expect("storm file count fits u32");
    let tracked_u32 = u32::try_from(tracked).expect("tracked population fits u32");

    let (tokio_runtime, local_tasks) = setup_test_runtime();
    tokio_runtime.block_on(local_tasks.run_until(async {
        let directory = tempdir().unwrap();
        for index in 0..files {
            std::fs::write(
                directory.path().join(format!("storm-{index:05}.log")),
                format!("old-{index:05}\n"),
            )
            .unwrap();
        }
        let pattern = directory.path().join("storm-*.log");
        let namespace = directory.path().join("checkpoint");
        let runtime = runtime_config_with(pattern.to_str().unwrap(), &namespace, 64, |config| {
            config.limits.max_tracked_files = tracked_u32;
            config.limits.max_pending_candidates = tracked_u32;
            config.limits.max_open_files = files_u32;
            config.batch.max_flush_period = Duration::from_millis(20);
            config.rotation.rotate_wait = Duration::from_millis(20);
            config.checkpoint.compact_after_transactions = 1;
        });
        let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let worker = spawn_worker(runtime.clone(), event_tx).unwrap();

        let started = Instant::now();
        let mut initial_records = 0usize;
        while initial_records < files {
            let batch = receive_batch(&mut events).await;
            initial_records = initial_records
                .checked_add(usize::try_from(batch.record_count).expect("record count fits usize"))
                .expect("initial record count fits");
            assert!(initial_records <= files);
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
        wait_for_worker_counter_at_least(
            &worker.telemetry,
            WorkerCounter::CheckpointCompactions,
            1,
        )
        .await;
        let initial_compactions = worker
            .telemetry
            .counter_for_test(WorkerCounter::CheckpointCompactions);

        for index in 0..files {
            let source = directory.path().join(format!("storm-{index:05}.log"));
            std::fs::rename(
                &source,
                directory.path().join(format!("storm-{index:05}.log.1")),
            )
            .unwrap();
            std::fs::write(&source, format!("new-{index:05}\n")).unwrap();
        }

        let mut replacement_records = 0usize;
        while replacement_records < files {
            let batch = receive_batch(&mut events).await;
            replacement_records = replacement_records
                .checked_add(usize::try_from(batch.record_count).expect("record count fits usize"))
                .expect("replacement record count fits");
            assert!(replacement_records <= files);
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
        wait_for_worker_counter_at_least(
            &worker.telemetry,
            WorkerCounter::RotationFinalizations,
            u64::from(files_u32),
        )
        .await;
        wait_for_worker_counter_at_least(
            &worker.telemetry,
            WorkerCounter::CheckpointCompactions,
            initial_compactions
                .checked_add(1)
                .expect("compaction target fits u64"),
        )
        .await;
        let compactions = worker
            .telemetry
            .counter_for_test(WorkerCounter::CheckpointCompactions);
        eprintln!(
            "filelog storm: files={files} tracked={tracked} \
             compactions={compactions} elapsed_micros={}",
            started.elapsed().as_micros()
        );
        stop_worker(worker, &mut events).await.unwrap();

        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        assert_eq!(store.table().len(), tracked);
        let active = store
            .table()
            .iter()
            .filter(|(_, record)| record.lifecycle_state == LifecycleState::Active)
            .count();
        let finalized = store
            .table()
            .iter()
            .filter(|(_, record)| record.lifecycle_state == LifecycleState::RotatedFinalized)
            .count();
        assert_eq!(active, files);
        assert_eq!(finalized, files);
        assert!(
            store
                .table()
                .iter()
                .all(|(_, checkpoint)| checkpoint.committed_offset > 0)
        );
    }));
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
/// Scenario: a broad glob (`app.log*`) matches both a locator being renamed
/// away and the brand-new locator that appears at the vacated distinguished
/// path, under `start_at: end`.
/// Guarantees: the ordered path rebound recognizes the new locator as A's
/// move/create replacement and registers it clean at offset zero -- proving
/// B never inherits A's offset and never applies `start_at: end` -- while A
/// keeps reading its own pre-rotation content through its retained
/// descriptor.
#[tokio::test]
async fn broad_glob_move_create_gives_new_locator_zero_offset_under_start_at_end() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("app.log");
    let rotated = directory.path().join("app.log.1");
    std::fs::write(&source, b"before-rotation\n").unwrap();
    let namespace = directory.path().join("checkpoint");
    let include = directory
        .path()
        .join("app.log*")
        .to_str()
        .unwrap()
        .to_owned();
    let runtime = runtime_config_with(&include, &namespace, 1, |config| {
        config.start_at = crate::receivers::filelog_receiver::StartAt::End;
    });
    let (event_tx, mut events) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
    let worker = spawn_worker(runtime.clone(), event_tx).unwrap();
    let telemetry = Arc::clone(&worker.telemetry);

    // `start_at: end` anchors A past its pre-existing bytes, so no batch is
    // emitted yet; wait for the reader to actually open before rotating.
    wait_for_worker_gauge(&telemetry, WorkerGauge::FilesOpen, 1).await;

    std::fs::rename(&source, &rotated).unwrap();
    std::fs::write(&source, b"after-rotation\n").unwrap();

    let mut replacement = receive_batch_with_timeout(&mut events, Duration::from_secs(15)).await;
    let request = decode_worker_records(&mut replacement.records);
    assert_eq!(log_body_bytes(only_log(&request)), b"after-rotation");
    wait_for_worker_counter(&telemetry, WorkerCounter::RotationRecognizedReplacement, 1).await;

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
    let records: Vec<_> = store.table().iter().collect();
    assert_eq!(records.len(), 2);
    let a_record = records
        .iter()
        .find(|(_, record)| record.committed_offset == 16)
        .expect("A keeps its own pre-rotation committed offset");
    assert_eq!(a_record.1.lifecycle_state, LifecycleState::Active);
    let b_record = records
        .iter()
        .find(|(_, record)| record.locator != a_record.1.locator)
        .expect("B is a distinct identity");
    assert_eq!(b_record.1.committed_offset, 15);
    assert_eq!(b_record.1.lifecycle_state, LifecycleState::Active);
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
            committed_frontier_guard: zero_guard(17),
            fingerprint: b"0123456789abcdef".to_vec(),
            ignored_header_bytes: 0,
            locator: Locator::PosixDevIno { dev: 1, ino: 91 },
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: runtime.framing_profile_digest,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 10,
            advisory_path: AdvisoryPath::from_unix_bytes(b"finalize.log").unwrap(),
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
/// Guarantees: reopen reports descriptor unavailability, the worker releases
/// volatile state, the old durable identity remains Active and unfinalized,
/// and same-epoch record numbering does not restart.
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
            committed_frontier_guard: zero_guard(0),
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
    let durable_before = runtime.store.table().get(&file_id).unwrap().clone();
    let mut readers = ReaderTable::new(ReaderSettings::from_runtime(&runtime.config)).unwrap();
    readers
        .insert(
            DiscoveredCandidate {
                matched_path: source.clone(),
                resolved_path,
                evidence: evidence.clone(),
                modified: None,
            },
            ResolvedIdentity {
                file_id,
                file_epoch: 1,
                committed_offset: 0,
                framing_resume: FramingResume::Clean,
                lifecycle_state: LifecycleState::Active,
                matched_by: IdentityMatch::NewDiscovery,
                committed_frontier_guard: zero_guard(0),
                advisory_path: evidence.advisory_path.clone(),
            },
        )
        .unwrap();
    runtime.readers = Some(readers);
    let first_number = runtime
        .record_numbers
        .prepare(file_id, 1, None)
        .and_then(|reservation| runtime.record_numbers.commit(reservation))
        .unwrap();
    assert_eq!(first_number, Some(0));

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
        .expect("descriptor containment was not cancelled");
    runtime
        .remember_inactive_locator(released, file_id)
        .unwrap();
    let record = runtime.store.table().get(&file_id).unwrap();
    assert_eq!(record, &durable_before);
    assert_eq!(runtime.inactive_locators.get(&locator), Some(&file_id));
    assert_eq!(
        runtime
            .record_numbers
            .prepare(file_id, 1, None)
            .unwrap()
            .record_number(),
        Some(1)
    );
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
