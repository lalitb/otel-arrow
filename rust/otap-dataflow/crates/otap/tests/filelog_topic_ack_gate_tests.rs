// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! End-to-end Ack gating for the filelog receiver over an all-subscriber
//! broadcast topic hop.
//!
//! This exercises the real route the Phase 1 filelog receiver depends on:
//!
//! ```text
//!   filelog receiver -> topic exporter -> topic (broadcast_only, ack_mode all)
//!       -> topic receiver -> downstream required subscriber
//! ```
//!
//! Durable checkpoint progress is observed behaviorally through redelivery
//! after a restart, which is the property the receiver contract actually
//! promises: if the checkpoint did not advance, the same bytes are read again.
//!
//! The test lives here because it needs the full OTAP factory inventory
//! (filelog, topic exporter, topic receiver) plus a downstream acking exporter,
//! which the node crates cannot reference without a dependency cycle.

mod common;

use arrow::array::{DictionaryArray, StringArray, StructArray};
use arrow::datatypes::UInt16Type;
use common::counting_exporter::{self, COUNTING_EXPORTER_URN};
use otap_df_config::observed_state::{ObservedStateSettings, SendPolicy};
use otap_df_config::pipeline::{PipelineConfig, PipelineConfigBuilder, PipelineType};
use otap_df_config::policy::{ChannelCapacityPolicy, TelemetryPolicy};
use otap_df_config::topic::TopicAckPropagationMode;
use otap_df_config::{DeployedPipelineKey, PipelineGroupId, PipelineId, TopicName};
use otap_df_core_nodes::exporters::topic_exporter::TOPIC_EXPORTER_URN;
use otap_df_core_nodes::receivers::filelog_receiver::FILELOG_RECEIVER_URN;
use otap_df_core_nodes::receivers::topic_receiver::TOPIC_RECEIVER_URN;
use otap_df_engine::context::ControllerContext;
use otap_df_engine::control::{
    RuntimeControlMsg, pipeline_completion_msg_channel, runtime_ctrl_msg_channel,
};
use otap_df_engine::entity_context::set_pipeline_entity_key;
use otap_df_engine::error::Error as EngineError;
use otap_df_engine::topic::{
    PipelineTopicBinding, RecvItem, SubscriberOptions, Subscription, SubscriptionMode,
    TopicBroadcastAckMode, TopicBroadcastOnLagPolicy, TopicBroker, TopicHandle, TopicOptions,
    TopicSet,
};
use otap_df_otap::OTAP_PIPELINE_FACTORY;
use otap_df_otap::pdata::OtapPdata;
use otap_df_pdata::OtapPayload;
use otap_df_pdata::otap::{Logs, OtapArrowRecords};
use otap_df_pdata::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use otap_df_state::store::ObservedStateStore;
use otap_df_telemetry::InternalTelemetrySystem;
use serde_json::json;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TOPIC_LOCAL_NAME: &str = "filelog_out";

static STATE_DIR_LOCK: Mutex<()> = Mutex::new(());

/// One pipeline running on its own OS thread, with a control sender captured
/// after the pipeline finished building (and therefore, for the subscriber
/// pipeline, after its topic subscription is registered).
struct RunningPipeline {
    ctrl: otap_df_engine::control::RuntimeCtrlMsgSender<OtapPdata>,
    thread: JoinHandle<()>,
    result: mpsc::Receiver<Result<Vec<()>, EngineError>>,
}

impl RunningPipeline {
    fn shutdown(self) {
        self.ctrl
            .try_send(RuntimeControlMsg::Shutdown {
                deadline: Instant::now() + Duration::from_secs(5),
                reason: "filelog ack-gate test shutdown".to_owned(),
            })
            .expect("shutdown request should be accepted");
        assert!(
            wait_until(Duration::from_secs(10), || self.thread.is_finished()),
            "pipeline did not finish within its shutdown deadline"
        );
        self.thread.join().expect("pipeline thread should join");
        let result = self
            .result
            .recv()
            .expect("pipeline should publish its shutdown result");
        match result {
            Ok(_) => {}
            Err(EngineError::RuntimeMsgError { error })
                if error == "Channel is closed and the message could not be sent" => {}
            Err(error) => panic!("pipeline shutdown failed: {error:?}"),
        }
    }

    fn wait_for_exit(self, deadline: Duration) -> Result<Vec<()>, EngineError> {
        assert!(
            wait_until(deadline, || self.thread.is_finished()),
            "pipeline did not reach its expected terminal state"
        );
        self.thread.join().expect("pipeline thread should join");
        self.result
            .recv()
            .expect("pipeline should publish its terminal result")
    }
}

/// Points the filelog checkpoint namespace (`${engine.state_dir}`) at a
/// test-owned directory.
///
/// SAFETY: the process-wide test lock serializes every environment mutation
/// and remains held until all pipeline threads using the value have joined.
#[allow(unsafe_code)]
fn test_state_dir() -> (MutexGuard<'static, ()>, tempfile::TempDir) {
    let guard = STATE_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("filelog route state directory should be created");
    unsafe {
        std::env::set_var("OTAP_DF_STATE_DIR", directory.path());
    }
    (guard, directory)
}

fn topic_set(handle: &TopicHandle<OtapPdata>) -> TopicSet<OtapPdata> {
    let set = TopicSet::new("ack-gate-set");
    let binding = PipelineTopicBinding::from(handle.clone())
        .with_default_ack_propagation_mode(TopicAckPropagationMode::Auto);
    _ = set.insert(
        TopicName::parse(TOPIC_LOCAL_NAME).expect("topic name"),
        binding,
    );
    set
}

fn spawn_pipeline(
    config: PipelineConfig,
    pipeline_group_id: PipelineGroupId,
    pipeline_id: PipelineId,
    handle: TopicHandle<OtapPdata>,
) -> RunningPipeline {
    let (ctrl_tx_out, ctrl_rx_out) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let telemetry_system = InternalTelemetrySystem::default();
        let registry = telemetry_system.registry();
        let controller_ctx = ControllerContext::new(registry.clone());
        let mut pipeline_ctx = controller_ctx.pipeline_context_with(
            pipeline_group_id.clone(),
            pipeline_id.clone(),
            0,
            1,
            0,
        );
        pipeline_ctx.set_topic_set(topic_set(&handle));
        let pipeline_entity_key = pipeline_ctx.register_pipeline_entity();
        let channel_capacity_policy = ChannelCapacityPolicy::default();
        let runtime_pipeline = OTAP_PIPELINE_FACTORY
            .build(
                pipeline_ctx.clone(),
                config,
                channel_capacity_policy.clone(),
                TelemetryPolicy::default(),
                None,
                std::collections::BTreeMap::new(),
                None,
                None,
            )
            .expect("runtime pipeline should build");

        let (runtime_ctrl_tx, runtime_ctrl_rx) =
            runtime_ctrl_msg_channel(channel_capacity_policy.control.pipeline);
        let (pipeline_completion_tx, pipeline_completion_rx) =
            pipeline_completion_msg_channel(channel_capacity_policy.control.completion);

        // Published only after `build` returned, so a subscriber pipeline has
        // already registered its topic subscription before the caller starts
        // the publishing pipeline.
        ctrl_tx_out
            .send(runtime_ctrl_tx.clone())
            .expect("control sender should be delivered to the test");

        let observed_state_store =
            ObservedStateStore::new(&ObservedStateSettings::default(), registry.clone());
        let pipeline_key = DeployedPipelineKey {
            pipeline_group_id,
            pipeline_id,
            core_id: 0,
            deployment_generation: 0,
        };
        let metrics_reporter = telemetry_system.reporter();
        let event_reporter = observed_state_store.reporter(SendPolicy::default());

        let _pipeline_entity_guard =
            set_pipeline_entity_key(pipeline_ctx.metrics_registry(), pipeline_entity_key);
        let (_memory_pressure_tx, memory_pressure_rx) = tokio::sync::watch::channel(
            otap_df_engine::memory_limiter::MemoryPressureChanged::initial(),
        );
        // A run can end in a node-level terminal error (for example when the
        // filelog receiver exhausts its retry budget with no route); the test
        // asserts on delivery and redelivery, not on the exit status.
        let run_result = runtime_pipeline.run_forever(
            pipeline_key,
            pipeline_ctx,
            event_reporter,
            metrics_reporter,
            Duration::from_secs(1),
            memory_pressure_rx,
            runtime_ctrl_tx,
            runtime_ctrl_rx,
            pipeline_completion_tx,
            pipeline_completion_rx,
        );
        result_tx
            .send(run_result)
            .expect("pipeline result should be delivered to the test");
    });

    let ctrl = ctrl_rx_out
        .recv_timeout(Duration::from_secs(30))
        .expect("pipeline should publish its control sender after building");
    RunningPipeline {
        ctrl,
        thread,
        result: result_rx,
    }
}

fn filelog_pipeline_config(
    pipeline_group_id: &PipelineGroupId,
    pipeline_id: &PipelineId,
    include_glob: &Path,
    checkpoint_id: &str,
) -> PipelineConfig {
    filelog_pipeline_config_with(
        pipeline_group_id,
        pipeline_id,
        include_glob,
        checkpoint_id,
        100,
        "fail",
    )
}

fn filelog_pipeline_config_with(
    pipeline_group_id: &PipelineGroupId,
    pipeline_id: &PipelineId,
    include_glob: &Path,
    checkpoint_id: &str,
    max_attempts: u32,
    on_nack: &str,
) -> PipelineConfig {
    PipelineConfigBuilder::new()
        .add_receiver(
            "filelog",
            FILELOG_RECEIVER_URN,
            Some(json!({
                "include": [include_glob.to_string_lossy()],
                "start_at": "beginning",
                "discovery": {
                    "reconcile_interval": "100ms",
                    "reconcile_jitter_percent": 0
                },
                "reader": { "eof_reprobe_interval": "50ms" },
                "batch": { "max_flush_period": "100ms" },
                "retry": {
                    "max_attempts": max_attempts,
                    "initial_backoff": "50ms",
                    "max_backoff": "100ms"
                },
                "on_nack": on_nack,
                "checkpoint": {
                    "id": checkpoint_id,
                    "sync_interval": "0s"
                }
            })),
        )
        .add_exporter(
            "to_topic",
            TOPIC_EXPORTER_URN,
            Some(json!({ "topic": TOPIC_LOCAL_NAME, "queue_on_full": "block" })),
        )
        .to("filelog", "to_topic")
        .build(
            PipelineType::Otap,
            pipeline_group_id.clone(),
            pipeline_id.clone(),
        )
        .expect("filelog pipeline config should build")
}

fn direct_subscription(handle: &TopicHandle<OtapPdata>) -> Subscription<OtapPdata> {
    handle
        .subscribe(SubscriptionMode::Broadcast, SubscriberOptions::default())
        .expect("direct required subscription should register")
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime should build")
        .block_on(future)
}

fn recv_direct(subscription: &mut Subscription<OtapPdata>) -> RecvItem<OtapPdata> {
    block_on(async {
        tokio::time::timeout(Duration::from_secs(10), subscription.recv())
            .await
            .expect("direct subscription should receive before timeout")
            .expect("direct subscription should remain open")
    })
}

fn empty_pdata() -> Arc<OtapPdata> {
    Arc::new(OtapPdata::new_todo_context(OtapPayload::OtapArrowRecords(
        OtapArrowRecords::Logs(Logs::default()),
    )))
}

fn arrow_records(data: &OtapPdata) -> &OtapArrowRecords {
    match data.payload_ref() {
        OtapPayload::OtapArrowRecords(records) => records,
        OtapPayload::OtlpBytes(_) => panic!("filelog route should emit OTAP Arrow records"),
    }
}

fn log_bodies(data: &OtapPdata) -> Vec<String> {
    let batch = arrow_records(data)
        .get(ArrowPayloadType::Logs)
        .expect("filelog output should contain logs");
    let bodies = batch
        .column_by_name("body")
        .expect("logs should contain body")
        .as_any()
        .downcast_ref::<StructArray>()
        .expect("body should be a struct");
    let strings = bodies
        .column_by_name("str")
        .expect("body should contain string values")
        .as_any()
        .downcast_ref::<DictionaryArray<UInt16Type>>()
        .expect("string bodies should use the OTAP dictionary");
    let values = strings
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("body dictionary values should be strings");
    (0..batch.num_rows())
        .map(|index| {
            let key = strings.key(index).expect("filelog body should be present");
            values.value(key).to_owned()
        })
        .collect()
}

fn restart_stable_records(
    data: &OtapPdata,
) -> Vec<(ArrowPayloadType, arrow::record_batch::RecordBatch)> {
    let records = arrow_records(data);
    records
        .allowed_payload_types()
        .iter()
        .filter_map(|payload_type| {
            let batch = records.get(*payload_type)?;
            let stable = if *payload_type == ArrowPayloadType::Logs {
                let projected: Vec<_> = batch
                    .schema()
                    .fields()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, field)| {
                        (field.name() != "observed_time_unix_nano").then_some(index)
                    })
                    .collect();
                batch
                    .project(&projected)
                    .expect("stable restart projection should be valid")
            } else {
                batch.clone()
            };
            Some((*payload_type, stable))
        })
        .collect()
}

fn ack_next_tracked(subscription: &mut Subscription<OtapPdata>) -> u64 {
    for _ in 0..64 {
        match recv_direct(subscription) {
            RecvItem::Lagged { .. } => {}
            RecvItem::Message(envelope) => match subscription.ack(envelope.id) {
                Ok(()) => return envelope.id,
                Err(EngineError::MessageNotTracked) => {}
                Err(error) => panic!("unexpected direct Ack result: {error}"),
            },
        }
    }
    panic!("direct subscription did not reach a tracked retry");
}

fn subscriber_pipeline_config(
    pipeline_group_id: &PipelineGroupId,
    pipeline_id: &PipelineId,
    counter_id: &str,
) -> PipelineConfig {
    PipelineConfigBuilder::new()
        .add_receiver(
            "from_topic",
            TOPIC_RECEIVER_URN,
            Some(json!({
                "topic": TOPIC_LOCAL_NAME,
                "subscription": { "mode": "broadcast" }
            })),
        )
        .add_exporter(
            "sink",
            COUNTING_EXPORTER_URN,
            Some(json!({ "counter_id": counter_id })),
        )
        .to("from_topic", "sink")
        .build(
            PipelineType::Otap,
            pipeline_group_id.clone(),
            pipeline_id.clone(),
        )
        .expect("subscriber pipeline config should build")
}

fn wait_until(deadline: Duration, condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}

/// Scenario: a real filelog receiver publishes through a topic exporter into a
/// `broadcast_only` topic with `ack_mode: all`, first with zero ready required
/// subscribers and then with a topic receiver feeding a downstream exporter that
/// Acks.
/// Guarantees: with zero ready required membership the durable checkpoint never
/// advances (every record is redelivered on the next run and nothing is Acked);
/// once an aggregate Ack arrives the checkpoint advances, so a subsequent run
/// delivers only a newly appended sentinel.
#[test]
fn filelog_checkpoint_advances_only_after_aggregate_topic_ack() {
    let (_state_guard, _state_dir) = test_state_dir();
    let log_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let log_path = log_dir.path().join("app.log");
    {
        let mut file = std::fs::File::create(&log_path).expect("log file should be created");
        file.write_all(b"alpha\nbravo\ncharlie\n")
            .expect("log content should be written");
        file.sync_all().expect("log file should be synced");
    }

    let checkpoint_id = "filelog-ack-gate-e2e";
    let counter_id = "filelog-ack-gate-e2e";
    let delivered = Arc::new(AtomicU64::new(0));
    counting_exporter::register_counter(counter_id, delivered.clone());

    let filelog_group: PipelineGroupId = "ack-gate".into();
    let filelog_id: PipelineId = "filelog-source".into();
    let subscriber_group: PipelineGroupId = "ack-gate".into();
    let subscriber_id: PipelineId = "topic-subscriber".into();

    // Run 1: publish with zero ready required subscribers.
    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").expect("topic name"),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .expect("topic should be created");
    let source = spawn_pipeline(
        filelog_pipeline_config(&filelog_group, &filelog_id, &log_path, checkpoint_id),
        filelog_group.clone(),
        filelog_id.clone(),
        handle.clone(),
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        delivered.load(Ordering::Acquire),
        0,
        "no subscriber can have received anything"
    );
    source.shutdown();
    handle.close();

    // Run 2: the same file, now with a ready required subscriber. Every record
    // is redelivered, which proves run 1 never advanced the checkpoint.
    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").expect("topic name"),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .expect("topic should be created");
    let subscriber = spawn_pipeline(
        subscriber_pipeline_config(&subscriber_group, &subscriber_id, counter_id),
        subscriber_group.clone(),
        subscriber_id.clone(),
        handle.clone(),
    );
    let source = spawn_pipeline(
        filelog_pipeline_config(&filelog_group, &filelog_id, &log_path, checkpoint_id),
        filelog_group.clone(),
        filelog_id.clone(),
        handle.clone(),
    );
    assert!(
        wait_until(Duration::from_secs(20), || delivered
            .load(Ordering::Acquire)
            >= 3),
        "records withheld by the zero-membership run must be redelivered, got {}",
        delivered.load(Ordering::Acquire)
    );
    // Let the aggregate Ack land and the checkpoint transaction sync.
    std::thread::sleep(Duration::from_secs(1));
    source.shutdown();
    subscriber.shutdown();
    handle.close();

    let after_ack = delivered.load(Ordering::Acquire);
    assert_eq!(
        after_ack, 3,
        "exactly the three withheld records should be delivered once, not duplicated"
    );

    // Run 3: append one sentinel. A live route must deliver it, while the
    // checkpoint must suppress every previously Acked record.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("log file should reopen");
        file.write_all(b"delta\n")
            .expect("sentinel should be appended");
        file.sync_all().expect("sentinel should be synced");
    }
    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").expect("topic name"),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .expect("topic should be created");
    let mut sentinel_subscriber = direct_subscription(&handle);
    let subscriber = spawn_pipeline(
        subscriber_pipeline_config(&subscriber_group, &subscriber_id, counter_id),
        subscriber_group.clone(),
        subscriber_id.clone(),
        handle.clone(),
    );
    let source = spawn_pipeline(
        filelog_pipeline_config(&filelog_group, &filelog_id, &log_path, checkpoint_id),
        filelog_group,
        filelog_id,
        handle.clone(),
    );
    let sentinel = match recv_direct(&mut sentinel_subscriber) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected sentinel lag of {missed}"),
    };
    assert_eq!(log_bodies(sentinel.payload.as_ref()), ["delta"]);
    sentinel_subscriber.ack(sentinel.id).unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || delivered
            .load(Ordering::Acquire)
            > after_ack),
        "the restart route did not deliver its sentinel"
    );
    source.shutdown();
    subscriber.shutdown();
    drop(sentinel_subscriber);
    handle.close();

    assert_eq!(
        delivered.load(Ordering::Acquire),
        after_ack + 1,
        "the live restart route must deliver only the new sentinel"
    );

    counting_exporter::unregister_counter(counter_id);
}

/// Scenario: a real filelog route has one normal topic-receiver subscriber
/// Ack while a second direct required subscriber Nacks the first attempt and
/// Acks the retry.
/// Guarantees: the exact record population is resent, the previously
/// successful subscriber receives one duplicate, a stale first-attempt Ack is
/// rejected, and checkpoint progress suppresses a later restart only after
/// retry consensus.
#[test]
fn required_nack_resends_exact_batch_and_ignores_stale_completion() {
    let (_state_guard, _state_dir) = test_state_dir();
    let log_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let log_path = log_dir.path().join("nack.log");
    std::fs::write(&log_path, b"one\ntwo\nthree\n").expect("log content should be written");

    let checkpoint_id = "filelog-route-required-nack";
    let counter_id = "filelog-route-required-nack";
    let delivered = Arc::new(AtomicU64::new(0));
    counting_exporter::register_counter(counter_id, delivered.clone());
    let filelog_group: PipelineGroupId = "route-nack".into();
    let filelog_id: PipelineId = "source".into();
    let subscriber_group: PipelineGroupId = "route-nack".into();
    let subscriber_id: PipelineId = "acking-subscriber".into();

    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").expect("topic name"),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .expect("topic should be created");
    let mut rejecting = direct_subscription(&handle);
    let subscriber = spawn_pipeline(
        subscriber_pipeline_config(&subscriber_group, &subscriber_id, counter_id),
        subscriber_group.clone(),
        subscriber_id.clone(),
        handle.clone(),
    );
    let source = spawn_pipeline(
        filelog_pipeline_config(&filelog_group, &filelog_id, &log_path, checkpoint_id),
        filelog_group.clone(),
        filelog_id.clone(),
        handle.clone(),
    );

    let first = match recv_direct(&mut rejecting) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected first-attempt lag of {missed}"),
    };
    let first_items = first.payload.num_items() as u64;
    assert_eq!(first_items, 3);
    assert!(wait_until(Duration::from_secs(10), || delivered
        .load(Ordering::Acquire)
        >= first_items));
    rejecting
        .nack(first.id, Arc::<str>::from("required subscriber rejected"))
        .expect("first attempt should be tracked");

    let retry = match recv_direct(&mut rejecting) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected retry lag of {missed}"),
    };
    assert_eq!(retry.payload.num_items() as u64, first_items);
    assert_eq!(
        arrow_records(first.payload.as_ref()),
        arrow_records(retry.payload.as_ref())
    );
    assert_ne!(retry.id, first.id);
    assert!(matches!(
        rejecting.ack(first.id),
        Err(EngineError::MessageNotTracked)
    ));
    assert!(matches!(
        rejecting.nack(first.id, Arc::<str>::from("stale prior attempt")),
        Err(EngineError::MessageNotTracked)
    ));
    rejecting
        .ack(retry.id)
        .expect("retry attempt should still require this subscriber");
    assert!(wait_until(Duration::from_secs(10), || delivered
        .load(Ordering::Acquire)
        >= first_items * 2));
    std::thread::sleep(Duration::from_millis(500));
    source.shutdown();
    subscriber.shutdown();
    drop(rejecting);
    handle.close();
    let after_retry = delivered.load(Ordering::Acquire);
    assert_eq!(after_retry, first_items * 2);
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        file.write_all(b"four\n").unwrap();
        file.sync_all().unwrap();
    }

    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").expect("topic name"),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .expect("topic should be created");
    let mut sentinel_subscriber = direct_subscription(&handle);
    let subscriber = spawn_pipeline(
        subscriber_pipeline_config(&subscriber_group, &subscriber_id, counter_id),
        subscriber_group,
        subscriber_id,
        handle.clone(),
    );
    let source = spawn_pipeline(
        filelog_pipeline_config(&filelog_group, &filelog_id, &log_path, checkpoint_id),
        filelog_group,
        filelog_id,
        handle.clone(),
    );
    let sentinel = match recv_direct(&mut sentinel_subscriber) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected Nack sentinel lag of {missed}"),
    };
    assert_eq!(log_bodies(sentinel.payload.as_ref()), ["four"]);
    sentinel_subscriber.ack(sentinel.id).unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || delivered
            .load(Ordering::Acquire)
            > after_retry),
        "the checkpoint restart did not deliver its new sentinel"
    );
    source.shutdown();
    subscriber.shutdown();
    drop(sentinel_subscriber);
    handle.close();

    assert_eq!(delivered.load(Ordering::Acquire), after_retry + 1);
    counting_exporter::unregister_counter(counter_id);
}

/// Scenario: one direct required subscriber receives a real filelog batch and
/// disappears before Ack while another topic-receiver subscriber has already
/// Acked; a replacement direct subscriber joins before retry.
/// Guarantees: disappearance Nacks the first attempt, retry snapshots fresh
/// required membership, and the already-successful subscriber receives one
/// duplicate before aggregate Ack authorizes progress.
#[test]
fn required_subscriber_disappearance_retries_with_fresh_membership() {
    let (_state_guard, _state_dir) = test_state_dir();
    let log_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let log_path = log_dir.path().join("disappear.log");
    std::fs::write(&log_path, b"line\n").expect("log content should be written");

    let checkpoint_id = "filelog-route-disappearance";
    let counter_id = "filelog-route-disappearance";
    let delivered = Arc::new(AtomicU64::new(0));
    counting_exporter::register_counter(counter_id, delivered.clone());
    let filelog_group: PipelineGroupId = "route-disappearance".into();
    let filelog_id: PipelineId = "source".into();
    let subscriber_group: PipelineGroupId = "route-disappearance".into();
    let subscriber_id: PipelineId = "acking-subscriber".into();

    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").expect("topic name"),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .expect("topic should be created");
    let mut disappearing = direct_subscription(&handle);
    let subscriber = spawn_pipeline(
        subscriber_pipeline_config(&subscriber_group, &subscriber_id, counter_id),
        subscriber_group,
        subscriber_id,
        handle.clone(),
    );
    let source = spawn_pipeline(
        filelog_pipeline_config(&filelog_group, &filelog_id, &log_path, checkpoint_id),
        filelog_group,
        filelog_id,
        handle.clone(),
    );

    let first = match recv_direct(&mut disappearing) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected first-attempt lag of {missed}"),
    };
    assert_eq!(first.payload.num_items(), 1);
    assert!(wait_until(Duration::from_secs(10), || delivered
        .load(Ordering::Acquire)
        >= 1));
    let mut replacement = direct_subscription(&handle);
    drop(disappearing);

    let retry = match recv_direct(&mut replacement) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected retry lag of {missed}"),
    };
    assert_eq!(retry.payload.num_items(), 1);
    replacement
        .ack(retry.id)
        .expect("fresh retry membership should require the replacement");
    assert!(wait_until(Duration::from_secs(10), || delivered
        .load(Ordering::Acquire)
        >= 2));

    std::thread::sleep(Duration::from_millis(500));
    source.shutdown();
    subscriber.shutdown();
    drop(replacement);
    handle.close();
    assert_eq!(delivered.load(Ordering::Acquire), 2);
    counting_exporter::unregister_counter(counter_id);
}

/// Scenario: one direct required subscriber leaves a real filelog publication
/// unread while untracked topic traffic overwrites the small broadcast ring.
/// Guarantees: lag/full pressure Nacks the first tracked attempt, both required
/// subscribers remain usable, and the retained filelog batch is Acked only
/// after each subscriber reaches the retry.
#[test]
fn required_subscriber_lag_nacks_and_retries_real_route() {
    let (_state_guard, _state_dir) = test_state_dir();
    let log_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let log_path = log_dir.path().join("lag.log");
    std::fs::write(&log_path, b"line\n").expect("log content should be written");
    let checkpoint_id = "filelog-route-lag";
    let filelog_group: PipelineGroupId = "route-lag".into();
    let filelog_id: PipelineId = "source".into();

    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").expect("topic name"),
            TopicOptions::BroadcastOnly {
                capacity: 4,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .expect("topic should be created");
    let mut slow = direct_subscription(&handle);
    let mut fast = direct_subscription(&handle);
    let source = spawn_pipeline(
        filelog_pipeline_config_with(
            &filelog_group,
            &filelog_id,
            &log_path,
            checkpoint_id,
            10,
            "fail",
        ),
        filelog_group,
        filelog_id,
        handle.clone(),
    );

    let first_fast = match recv_direct(&mut fast) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected initial fast lag of {missed}"),
    };
    fast.ack(first_fast.id)
        .expect("fast subscriber should Ack the first tracked attempt");

    for _ in 0..8 {
        block_on(handle.publish(empty_pdata())).expect("untracked pressure publish should succeed");
    }
    assert!(matches!(
        recv_direct(&mut slow),
        RecvItem::Lagged { missed } if missed > 0
    ));

    let retry_fast = ack_next_tracked(&mut fast);
    let retry_slow = ack_next_tracked(&mut slow);
    assert_eq!(retry_fast, retry_slow);

    std::thread::sleep(Duration::from_millis(500));
    source.shutdown();
    drop(fast);
    drop(slow);
    handle.close();
}

/// Scenario: real filelog routes exhaust two attempts under transient Nack
/// with both terminal policies, and exhaust two zero-membership aggregate
/// Nacks under `on_nack=fail`.
/// Guarantees: fail leaves the exact batch replayable, drop-and-continue
/// durably advances only after exhaustion, and empty required membership
/// cannot fabricate progress.
#[test]
fn retry_exhaustion_policies_and_zero_membership_preserve_checkpoint_contract() {
    let (_state_guard, _state_dir) = test_state_dir();

    let fail_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let fail_path = fail_dir.path().join("fail.log");
    std::fs::write(&fail_path, b"fail\n").unwrap();
    let fail_checkpoint = "filelog-route-exhaust-fail";
    let fail_group: PipelineGroupId = "route-exhaust-fail".into();
    let fail_id: PipelineId = "source".into();
    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").unwrap(),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .unwrap();
    let mut rejecting = direct_subscription(&handle);
    let source = spawn_pipeline(
        filelog_pipeline_config_with(
            &fail_group,
            &fail_id,
            &fail_path,
            fail_checkpoint,
            2,
            "fail",
        ),
        fail_group.clone(),
        fail_id.clone(),
        handle.clone(),
    );
    let mut fail_first_records = None;
    for attempt in 0..2 {
        let message = match recv_direct(&mut rejecting) {
            RecvItem::Message(envelope) => envelope,
            RecvItem::Lagged { missed } => panic!("unexpected Nack-test lag of {missed}"),
        };
        if attempt == 0 {
            fail_first_records = Some(restart_stable_records(message.payload.as_ref()));
        }
        rejecting
            .nack(message.id, Arc::<str>::from("exhaust fail"))
            .unwrap();
    }
    let fail_result = source.wait_for_exit(Duration::from_secs(10));
    let fail_error = fail_result.expect_err("on_nack=fail should terminate the source");
    assert!(fail_error.to_string().contains("terminal Nack"));
    drop(rejecting);
    handle.close();

    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").unwrap(),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .unwrap();
    let mut accepting = direct_subscription(&handle);
    let source = spawn_pipeline(
        filelog_pipeline_config_with(
            &fail_group,
            &fail_id,
            &fail_path,
            fail_checkpoint,
            2,
            "fail",
        ),
        fail_group,
        fail_id,
        handle.clone(),
    );
    let replay = match recv_direct(&mut accepting) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected fail replay lag of {missed}"),
    };
    assert_eq!(
        &restart_stable_records(replay.payload.as_ref()),
        fail_first_records
            .as_ref()
            .expect("the first rejected batch was captured")
    );
    accepting.ack(replay.id).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    source.shutdown();
    drop(accepting);
    handle.close();

    let drop_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let drop_path = drop_dir.path().join("drop.log");
    std::fs::write(&drop_path, b"drop\n").unwrap();
    let drop_checkpoint = "filelog-route-exhaust-drop";
    let drop_group: PipelineGroupId = "route-exhaust-drop".into();
    let drop_id: PipelineId = "source".into();
    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").unwrap(),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .unwrap();
    let mut rejecting = direct_subscription(&handle);
    let source = spawn_pipeline(
        filelog_pipeline_config_with(
            &drop_group,
            &drop_id,
            &drop_path,
            drop_checkpoint,
            2,
            "drop_and_continue",
        ),
        drop_group.clone(),
        drop_id.clone(),
        handle.clone(),
    );
    for _ in 0..2 {
        let message = match recv_direct(&mut rejecting) {
            RecvItem::Message(envelope) => envelope,
            RecvItem::Lagged { missed } => panic!("unexpected drop-test lag of {missed}"),
        };
        rejecting
            .nack(message.id, Arc::<str>::from("exhaust drop"))
            .unwrap();
    }
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&drop_path)
            .unwrap();
        file.write_all(b"sentinel\n").unwrap();
        file.sync_all().unwrap();
    }
    let continued = match recv_direct(&mut rejecting) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected continued-route lag of {missed}"),
    };
    assert_eq!(log_bodies(continued.payload.as_ref()), ["sentinel"]);
    rejecting.ack(continued.id).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    source.shutdown();
    drop(rejecting);
    handle.close();
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&drop_path)
            .unwrap();
        file.write_all(b"restart\n").unwrap();
        file.sync_all().unwrap();
    }

    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").unwrap(),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .unwrap();
    let mut accepting = direct_subscription(&handle);
    let source = spawn_pipeline(
        filelog_pipeline_config_with(
            &drop_group,
            &drop_id,
            &drop_path,
            drop_checkpoint,
            2,
            "fail",
        ),
        drop_group,
        drop_id,
        handle.clone(),
    );
    let sentinel = match recv_direct(&mut accepting) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected drop restart lag of {missed}"),
    };
    assert_eq!(
        log_bodies(sentinel.payload.as_ref()),
        ["restart"],
        "only the post-restart sentinel should be delivered"
    );
    accepting.ack(sentinel.id).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    source.shutdown();
    drop(accepting);
    handle.close();

    let zero_member_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let zero_member_path = zero_member_dir.path().join("zero-member.log");
    std::fs::write(&zero_member_path, b"zero-member\n").unwrap();
    let zero_member_checkpoint = "filelog-route-exhaust-zero-member";
    let zero_member_group: PipelineGroupId = "route-exhaust-zero-member".into();
    let zero_member_id: PipelineId = "source".into();
    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").unwrap(),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .unwrap();
    let source = spawn_pipeline(
        filelog_pipeline_config_with(
            &zero_member_group,
            &zero_member_id,
            &zero_member_path,
            zero_member_checkpoint,
            2,
            "fail",
        ),
        zero_member_group.clone(),
        zero_member_id.clone(),
        handle.clone(),
    );
    let zero_member_result = source.wait_for_exit(Duration::from_secs(10));
    let zero_member_error = zero_member_result
        .expect_err("zero-membership aggregate Nack exhaustion should terminate the source");
    match zero_member_error {
        EngineError::ReceiverError { error, .. } => assert_eq!(
            error,
            "filelog batch 1 attempt 2 received a terminal Nack under on_nack=fail"
        ),
        error => panic!("unexpected zero-membership terminal error: {error:?}"),
    }
    handle.close();

    let broker = TopicBroker::<OtapPdata>::new();
    let handle = broker
        .create_in_memory_topic(
            TopicName::parse("global::filelog_out").unwrap(),
            TopicOptions::BroadcastOnly {
                capacity: 64,
                on_lag: TopicBroadcastOnLagPolicy::DropOldest,
                ack_mode: TopicBroadcastAckMode::All,
            },
        )
        .unwrap();
    let mut accepting = direct_subscription(&handle);
    let source = spawn_pipeline(
        filelog_pipeline_config_with(
            &zero_member_group,
            &zero_member_id,
            &zero_member_path,
            zero_member_checkpoint,
            2,
            "fail",
        ),
        zero_member_group,
        zero_member_id,
        handle.clone(),
    );
    let replay = match recv_direct(&mut accepting) {
        RecvItem::Message(envelope) => envelope,
        RecvItem::Lagged { missed } => panic!("unexpected zero-member replay lag of {missed}"),
    };
    assert_eq!(log_bodies(replay.payload.as_ref()), ["zero-member"]);
    accepting.ack(replay.id).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    source.shutdown();
    drop(accepting);
    handle.close();
}
