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
use otap_df_engine::topic::{
    PipelineTopicBinding, TopicBroadcastAckMode, TopicBroadcastOnLagPolicy, TopicBroker,
    TopicHandle, TopicOptions, TopicSet,
};
use otap_df_otap::OTAP_PIPELINE_FACTORY;
use otap_df_otap::pdata::OtapPdata;
use otap_df_state::store::ObservedStateStore;
use otap_df_telemetry::InternalTelemetrySystem;
use serde_json::json;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TOPIC_LOCAL_NAME: &str = "filelog_out";

/// One pipeline running on its own OS thread, with a control sender captured
/// after the pipeline finished building (and therefore, for the subscriber
/// pipeline, after its topic subscription is registered).
struct RunningPipeline {
    ctrl: otap_df_engine::control::RuntimeCtrlMsgSender<OtapPdata>,
    thread: JoinHandle<()>,
}

impl RunningPipeline {
    fn shutdown(self) {
        self.ctrl
            .try_send(RuntimeControlMsg::Shutdown {
                deadline: Instant::now() + Duration::from_secs(5),
                reason: "filelog ack-gate test shutdown".to_owned(),
            })
            .expect("shutdown request should be accepted");
        self.thread.join().expect("pipeline thread should join");
    }
}

/// Points the filelog checkpoint namespace (`${engine.state_dir}`) at a
/// test-owned directory.
///
/// SAFETY: called once from the test thread before any pipeline thread is
/// spawned, and the value stays fixed for the remainder of the test binary.
#[allow(unsafe_code)]
fn set_state_dir(path: &Path) {
    unsafe {
        std::env::set_var("OTAP_DF_STATE_DIR", path);
    }
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
        let _run_result = runtime_pipeline.run_forever(
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
    });

    let ctrl = ctrl_rx_out
        .recv_timeout(Duration::from_secs(30))
        .expect("pipeline should publish its control sender after building");
    RunningPipeline { ctrl, thread }
}

fn filelog_pipeline_config(
    pipeline_group_id: &PipelineGroupId,
    pipeline_id: &PipelineId,
    include_glob: &Path,
    checkpoint_id: &str,
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
                    "max_attempts": 100,
                    "initial_backoff": "50ms",
                    "max_backoff": "100ms"
                },
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
/// over the unchanged file delivers nothing.
#[test]
fn filelog_checkpoint_advances_only_after_aggregate_topic_ack() {
    let state_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("state directory should be created");
    let log_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("log directory should be created");
    let log_path = log_dir.path().join("app.log");
    {
        let mut file = std::fs::File::create(&log_path).expect("log file should be created");
        file.write_all(b"alpha\nbravo\ncharlie\n")
            .expect("log content should be written");
        file.sync_all().expect("log file should be synced");
    }

    set_state_dir(state_dir.path());

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

    // Run 3: the file is unchanged and the checkpoint advanced past it, so a
    // fresh run must deliver nothing new.
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
        filelog_group,
        filelog_id,
        handle.clone(),
    );
    std::thread::sleep(Duration::from_secs(3));
    source.shutdown();
    subscriber.shutdown();
    handle.close();

    assert_eq!(
        delivered.load(Ordering::Acquire),
        after_ack,
        "an acked checkpoint must not redeliver the same records"
    );

    counting_exporter::unregister_counter(counter_id);
}
