// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Local async delivery runtime for the blocking filelog worker.

use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant as StdInstant};

use async_trait::async_trait;
use otap_df_channel::error::SendError;
use otap_df_engine::control::{CallData, NodeControlMsg};
use otap_df_engine::error::{Error, ReceiverErrorKind, TypedError};
use otap_df_engine::local::receiver as local;
use otap_df_engine::terminal_state::TerminalState;
use otap_df_engine::{
    Interests, MessageSourceLocalEffectHandlerExtension, ProducerEffectHandlerExtension,
};
use otap_df_otap::pdata::{Context, OtapPdata};
use otap_df_pdata::OtapPayload;
use otap_df_telemetry::metrics::{MetricSet, MetricSetSnapshot};
use otap_df_telemetry::reporter::ReportOutcome;
use otap_df_telemetry::{otel_debug, otel_info, otel_warn};

use super::config::RuntimeConfig;
use super::delivery::{
    BatchKey, CompletionIgnore, DeliveryDecision, PendingBatch, call_data, key_from_call_data,
};
use super::telemetry::{
    FilelogReceiverMetrics, HealthEventCategory, HealthEventLimiter, WorkerTelemetryBridge,
    add_counter_saturating, duration_ns, terminal_snapshots,
};
use super::worker::{
    WORKER_EVENT_CONTROL_SLOTS, WorkerBatch, WorkerCommand, WorkerEvent, WorkerHandle, spawn_worker,
};

const WORKER_COMMAND_RETRY_LIMIT: usize = 200;
const WORKER_COMMAND_RETRY_DELAY: Duration = Duration::from_millis(10);
const WORKER_JOIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Runtime receiver constructed only after factory validation.
pub(super) struct FilelogReceiver {
    config: RuntimeConfig,
    pub(super) metrics: Option<MetricSet<FilelogReceiverMetrics>>,
}

impl FilelogReceiver {
    pub(super) const fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            metrics: None,
        }
    }
}

#[derive(Default)]
struct DeliveryCounters {
    malformed_completions: u64,
    stale_completions: u64,
    duplicate_completions: u64,
    acks: u64,
    nacks: u64,
    retries: u64,
    explicit_loss_commits: u64,
    checkpoint_failures: u64,
}

impl DeliveryCounters {
    fn record_ignore(&mut self, ignored: CompletionIgnore) -> Result<(), &'static str> {
        let counter = match ignored {
            CompletionIgnore::Malformed => &mut self.malformed_completions,
            CompletionIgnore::Stale => &mut self.stale_completions,
            CompletionIgnore::Duplicate => &mut self.duplicate_completions,
        };
        *counter = counter
            .checked_add(1)
            .ok_or("filelog completion counter overflowed")?;
        Ok(())
    }

    fn record_ack(&mut self) -> Result<(), &'static str> {
        self.acks = self
            .acks
            .checked_add(1)
            .ok_or("filelog Ack counter overflowed")?;
        Ok(())
    }

    fn record_nack(&mut self) -> Result<(), &'static str> {
        self.nacks = self
            .nacks
            .checked_add(1)
            .ok_or("filelog Nack counter overflowed")?;
        Ok(())
    }

    fn record_retry(&mut self) -> Result<(), &'static str> {
        self.retries = self
            .retries
            .checked_add(1)
            .ok_or("filelog retry counter overflowed")?;
        Ok(())
    }

    fn record_explicit_loss(&mut self) -> Result<(), &'static str> {
        self.explicit_loss_commits = self
            .explicit_loss_commits
            .checked_add(1)
            .ok_or("filelog explicit-loss counter overflowed")?;
        Ok(())
    }

    fn record_checkpoint_failure(&mut self) -> Result<(), &'static str> {
        self.checkpoint_failures = self
            .checkpoint_failures
            .checked_add(1)
            .ok_or("filelog checkpoint-failure counter overflowed")?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecisionOutcome {
    Continue,
    Fail(BatchKey),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendOutcome {
    Sent,
    DrainDeadline(StdInstant),
    Shutdown(StdInstant),
}

#[async_trait(?Send)]
impl local::Receiver<OtapPdata> for FilelogReceiver {
    async fn start(
        self: Box<Self>,
        mut control_rx: local::ControlChannel<OtapPdata>,
        effect_handler: local::EffectHandler<OtapPdata>,
    ) -> Result<TerminalState, Error> {
        let FilelogReceiver {
            config,
            mut metrics,
        } = *self;
        if let Some(metrics) = metrics.as_mut() {
            add_counter_saturating(&mut metrics.starts, 1);
        }
        otel_info!("filelog_receiver.start");

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let mut health_events = HealthEventLimiter::default();
        let startup_telemetry = WorkerTelemetryBridge::default();
        let worker_handle = match spawn_worker(config.clone(), event_tx) {
            Ok(worker) => worker,
            Err(error) => {
                let primary = terminal_error(&effect_handler, error.to_string());
                return Err(finish_terminal_error(
                    primary,
                    &mut metrics,
                    &startup_telemetry,
                    &mut health_events,
                    &effect_handler,
                )
                .await);
            }
        };
        let worker_telemetry = Arc::clone(&worker_handle.telemetry);
        let mut worker = Some(worker_handle);
        let mut pending_batch = None;
        let mut retry_deadline = None;
        let mut drain_deadline = None;
        let mut consecutive_checkpoint_failures = 0u32;
        let mut counters = DeliveryCounters::default();
        let result = async {
            loop {
            tokio::select! {
                biased;

                _ = async {
                    if let Some(deadline) = drain_deadline {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    } else {
                        pending::<()>().await;
                    }
                }, if drain_deadline.is_some() => {
                    let deadline = drain_deadline.ok_or_else(|| {
                        terminal_error(
                            &effect_handler,
                            "filelog drain timer fired without a deadline",
                        )
                    })?;
                    if pending_batch.is_some() {
                        if let Some(suppressed) = admit_health_event(
                            &mut health_events,
                            &mut metrics,
                            HealthEventCategory::DrainTimeout,
                        ) {
                            otel_warn!(
                                "filelog_receiver.drain_timeout",
                                message = "Drain deadline reached with an unacknowledged retained batch; checkpoint progress remains unchanged",
                                suppressed_events = suppressed
                            );
                        }
                    }
                    shutdown_worker(&mut worker, &mut event_rx, &effect_handler, deadline).await?;
                    effect_handler.notify_receiver_drained().await?;
                    if let Some(metrics) = metrics.as_mut() {
                        add_counter_saturating(&mut metrics.drains, 1);
                    }
                    log_delivery_counters(&config, &counters);
                    return Ok(terminal_state(deadline, &mut metrics, &worker_telemetry));
                }

                message = control_rx.recv() => {
                    match message {
                        Ok(NodeControlMsg::Ack(ack)) => {
                            counters.record_ack().map_err(|error| terminal_error(&effect_handler, error))?;
                            let decision = completion_ack(&mut pending_batch, &ack.unwind.route.calldata);
                            record_completion_metrics(
                                decision,
                                true,
                                &mut metrics,
                                &mut health_events,
                            );
                            match apply_decision(
                                decision,
                                &config,
                                drain_deadline.is_some(),
                                &mut retry_deadline,
                                worker_sender(&worker)?,
                                &effect_handler,
                                &mut counters,
                                &mut metrics,
                                &mut health_events,
                            ).await? {
                                DecisionOutcome::Continue => {}
                                DecisionOutcome::Fail(key) => {
                                    shutdown_worker(
                                        &mut worker,
                                        &mut event_rx,
                                        &effect_handler,
                                        worker_cleanup_deadline(
                                            drain_deadline,
                                            config.drain_timeout,
                                        ),
                                    )
                                    .await?;
                                    return Err(terminal_error(
                                        &effect_handler,
                                        format!(
                                            "filelog batch {} attempt {} received a terminal Nack under on_nack=fail",
                                            key.batch_id, key.attempt
                                        ),
                                    ));
                                }
                            }
                        }
                        Ok(NodeControlMsg::Nack(nack)) => {
                            counters.record_nack().map_err(|error| terminal_error(&effect_handler, error))?;
                            let decision = completion_nack(
                                &mut pending_batch,
                                &nack.unwind.route.calldata,
                                nack.permanent,
                                nack.cause,
                                &config,
                            );
                            record_completion_metrics(
                                decision,
                                false,
                                &mut metrics,
                                &mut health_events,
                            );
                            match apply_decision(
                                decision,
                                &config,
                                drain_deadline.is_some(),
                                &mut retry_deadline,
                                worker_sender(&worker)?,
                                &effect_handler,
                                &mut counters,
                                &mut metrics,
                                &mut health_events,
                            ).await? {
                                DecisionOutcome::Continue => {}
                                DecisionOutcome::Fail(key) => {
                                    shutdown_worker(
                                        &mut worker,
                                        &mut event_rx,
                                        &effect_handler,
                                        worker_cleanup_deadline(
                                            drain_deadline,
                                            config.drain_timeout,
                                        ),
                                    )
                                    .await?;
                                    return Err(terminal_error(
                                        &effect_handler,
                                        format!(
                                            "filelog batch {} attempt {} received a terminal Nack under on_nack=fail",
                                            key.batch_id, key.attempt
                                        ),
                                    ));
                                }
                            }
                        }
                        Ok(NodeControlMsg::DrainIngress { deadline, .. }) => {
                            let deadline = receiver_drain_deadline(deadline, config.drain_timeout);
                            drain_deadline = Some(drain_deadline.map_or(deadline, |current: StdInstant| current.min(deadline)));
                            retry_deadline = None;
                            send_worker_command(
                                worker_sender(&worker)?,
                                WorkerCommand::Drain,
                                &effect_handler,
                            ).await?;
                            otel_info!(
                                "filelog_receiver.drain_ingress"
                            );
                        }
                        Ok(NodeControlMsg::Shutdown { deadline, .. }) => {
                            shutdown_worker(
                                &mut worker,
                                &mut event_rx,
                                &effect_handler,
                                deadline,
                            )
                            .await?;
                            if let Some(metrics) = metrics.as_mut() {
                                add_counter_saturating(&mut metrics.shutdowns, 1);
                            }
                            log_delivery_counters(&config, &counters);
                            return Ok(terminal_state(deadline, &mut metrics, &worker_telemetry));
                        }
                        Ok(NodeControlMsg::CollectTelemetry { mut metrics_reporter }) => {
                            report_metrics(&mut metrics, &worker_telemetry, &mut metrics_reporter);
                        }
                        Ok(NodeControlMsg::Config { .. }
                            | NodeControlMsg::TimerTick { .. }
                            | NodeControlMsg::Wakeup { .. }
                            | NodeControlMsg::DelayedData { .. }
                            | NodeControlMsg::MemoryPressureChanged { .. }) => {}
                        Err(error) => {
                            shutdown_worker(
                                &mut worker,
                                &mut event_rx,
                                &effect_handler,
                                worker_cleanup_deadline(drain_deadline, config.drain_timeout),
                            )
                            .await?;
                            return Err(Error::ChannelRecvError(error));
                        }
                    }
                }

                _ = async {
                    if let Some(deadline) = retry_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        pending::<()>().await;
                    }
                }, if retry_deadline.is_some() => {
                    retry_deadline = None;
                    let key = pending_batch
                        .as_mut()
                        .and_then(PendingBatch::retry_elapsed)
                        .ok_or_else(|| terminal_error(
                            &effect_handler,
                            "filelog retry timer fired without a pending retry",
                        ))?;
                    send_worker_command(
                        worker_sender(&worker)?,
                        WorkerCommand::Resend {
                            batch_id: key.batch_id,
                            next_attempt: key.attempt,
                        },
                        &effect_handler,
                    ).await?;
                }

                event = event_rx.recv() => {
                    match event {
                        Some(WorkerEvent::Batch(batch)) => {
                            let key = BatchKey::new(batch.batch_id, batch.attempt).ok_or_else(|| {
                                terminal_error(&effect_handler, "filelog worker emitted a zero batch ID or attempt")
                            })?;
                            let resend = pending_batch.is_some();
                            if resend {
                                let accepted = pending_batch
                                    .as_mut()
                                    .is_some_and(|pending| pending.begin_send(key));
                                if !accepted {
                                    shutdown_worker(
                                        &mut worker,
                                        &mut event_rx,
                                        &effect_handler,
                                        worker_cleanup_deadline(
                                            drain_deadline,
                                            config.drain_timeout,
                                        ),
                                    )
                                    .await?;
                                    return Err(terminal_error(
                                        &effect_handler,
                                        format!(
                                            "filelog worker resend ({}, {}) does not match async pending state",
                                            key.batch_id, key.attempt
                                        ),
                                    ));
                                }
                            } else if key.attempt != 1 {
                                shutdown_worker(
                                    &mut worker,
                                    &mut event_rx,
                                    &effect_handler,
                                    worker_cleanup_deadline(
                                        drain_deadline,
                                        config.drain_timeout,
                                    ),
                                )
                                .await?;
                                return Err(terminal_error(
                                    &effect_handler,
                                    "filelog worker emitted a noninitial attempt without pending state",
                                ));
                            }

                            match send_batch(
                                batch,
                                key,
                                &mut control_rx,
                                &effect_handler,
                                worker_sender(&worker)?,
                                &config,
                                &mut drain_deadline,
                                &mut pending_batch,
                                &mut counters,
                                &mut metrics,
                                &worker_telemetry,
                                &mut health_events,
                            ).await? {
                                SendOutcome::Sent => {
                                    if resend {
                                        if !pending_batch
                                            .as_mut()
                                            .is_some_and(|pending| pending.send_succeeded(key))
                                        {
                                            shutdown_worker(
                                                &mut worker,
                                                &mut event_rx,
                                                &effect_handler,
                                                worker_cleanup_deadline(
                                                    drain_deadline,
                                                    config.drain_timeout,
                                                ),
                                            )
                                            .await?;
                                            return Err(terminal_error(
                                                &effect_handler,
                                                "filelog resend completed outside the expected sending state",
                                            ));
                                        }
                                    } else {
                                        pending_batch = Some(PendingBatch::after_send(key));
                                    }
                                }
                                SendOutcome::DrainDeadline(deadline) => {
                                    shutdown_worker(
                                        &mut worker,
                                        &mut event_rx,
                                        &effect_handler,
                                        deadline,
                                    )
                                    .await?;
                                    effect_handler.notify_receiver_drained().await?;
                                    if let Some(metrics) = metrics.as_mut() {
                                        add_counter_saturating(&mut metrics.drains, 1);
                                    }
                                    log_delivery_counters(&config, &counters);
                                    return Ok(terminal_state(deadline, &mut metrics, &worker_telemetry));
                                }
                                SendOutcome::Shutdown(deadline) => {
                                    shutdown_worker(
                                        &mut worker,
                                        &mut event_rx,
                                        &effect_handler,
                                        deadline,
                                    )
                                    .await?;
                                    if let Some(metrics) = metrics.as_mut() {
                                        add_counter_saturating(&mut metrics.shutdowns, 1);
                                    }
                                    log_delivery_counters(&config, &counters);
                                    return Ok(terminal_state(deadline, &mut metrics, &worker_telemetry));
                                }
                            }
                        }
                        Some(WorkerEvent::CommitResult {
                            batch_id,
                            attempt,
                            explicit_loss,
                            result,
                        }) => {
                            let key = BatchKey::new(batch_id, attempt).ok_or_else(|| {
                                terminal_error(&effect_handler, "filelog worker returned a zero commit key")
                            })?;
                            let expected_loss = pending_batch
                                .as_ref()
                                .filter(|pending| pending.key() == key)
                                .and_then(PendingBatch::awaiting_commit);
                            if expected_loss != Some(explicit_loss) {
                                shutdown_worker(
                                    &mut worker,
                                    &mut event_rx,
                                    &effect_handler,
                                    worker_cleanup_deadline(
                                        drain_deadline,
                                        config.drain_timeout,
                                    ),
                                )
                                .await?;
                                return Err(terminal_error(
                                    &effect_handler,
                                    format!(
                                        "filelog commit result ({batch_id}, {attempt}, loss={explicit_loss}) does not match pending state"
                                    ),
                                ));
                            }
                            match result {
                                Ok(()) => {
                                    record_commit_success(&mut metrics, explicit_loss);
                                    pending_batch = None;
                                    retry_deadline = None;
                                    consecutive_checkpoint_failures = 0;
                                }
                                Err(error) => {
                                    counters.record_checkpoint_failure().map_err(|message| terminal_error(&effect_handler, message))?;
                                    if let Some(metrics) = metrics.as_mut() {
                                        add_counter_saturating(
                                            &mut metrics.checkpoint_failures,
                                            1,
                                        );
                                    }
                                    if let Some(suppressed) = admit_health_event(
                                        &mut health_events,
                                        &mut metrics,
                                        HealthEventCategory::CheckpointCommit,
                                    ) {
                                        otel_warn!(
                                            "filelog_receiver.checkpoint_operation_failed",
                                            operation = "commit_progress",
                                            suppressed_events = suppressed
                                        );
                                    }
                                    consecutive_checkpoint_failures = consecutive_checkpoint_failures
                                        .checked_add(1)
                                        .ok_or_else(|| terminal_error(
                                            &effect_handler,
                                            "filelog consecutive checkpoint-failure counter overflowed",
                                        ))?;
                                    if consecutive_checkpoint_failures >= config.checkpoint.max_consecutive_failures {
                                        let message = format!(
                                            "filelog checkpoint commit for batch {batch_id} attempt {attempt} failed {consecutive_checkpoint_failures} consecutive times: {error}"
                                        );
                                        shutdown_worker(
                                            &mut worker,
                                            &mut event_rx,
                                            &effect_handler,
                                            worker_cleanup_deadline(
                                                drain_deadline,
                                                config.drain_timeout,
                                            ),
                                        )
                                        .await?;
                                        return Err(terminal_error(&effect_handler, message));
                                    }
                                    send_worker_command(
                                        worker_sender(&worker)?,
                                        WorkerCommand::Commit {
                                            batch_id,
                                            attempt,
                                            explicit_loss,
                                        },
                                        &effect_handler,
                                    ).await?;
                                }
                            }
                        }
                        Some(WorkerEvent::Drained) => {
                            if pending_batch.is_some() {
                                shutdown_worker(
                                    &mut worker,
                                    &mut event_rx,
                                    &effect_handler,
                                    worker_cleanup_deadline(
                                        drain_deadline,
                                        config.drain_timeout,
                                    ),
                                )
                                .await?;
                                return Err(terminal_error(
                                    &effect_handler,
                                    "filelog worker reported drained with a pending batch",
                                ));
                            }
                            let deadline = drain_deadline.unwrap_or_else(StdInstant::now);
                            shutdown_worker(
                                &mut worker,
                                &mut event_rx,
                                &effect_handler,
                                deadline,
                            )
                            .await?;
                            effect_handler.notify_receiver_drained().await?;
                            if let Some(metrics) = metrics.as_mut() {
                                add_counter_saturating(&mut metrics.drains, 1);
                            }
                            log_delivery_counters(&config, &counters);
                            return Ok(terminal_state(deadline, &mut metrics, &worker_telemetry));
                        }
                        Some(WorkerEvent::Failed(message)) => {
                            close_and_join_worker(
                                &mut worker,
                                &mut event_rx,
                                &effect_handler,
                                worker_cleanup_deadline(drain_deadline, config.drain_timeout),
                            )
                            .await?;
                            return Err(terminal_error(&effect_handler, message));
                        }
                        Some(WorkerEvent::Stopped) | None => {
                            close_and_join_worker(
                                &mut worker,
                                &mut event_rx,
                                &effect_handler,
                                worker_cleanup_deadline(drain_deadline, config.drain_timeout),
                            )
                            .await?;
                            return Err(terminal_error(
                                &effect_handler,
                                "filelog read/checkpoint worker stopped unexpectedly",
                            ));
                        }
                    }
                }
            }
        }
        }
        .await;

        let result = if worker.is_some() {
            let cleanup = shutdown_worker(
                &mut worker,
                &mut event_rx,
                &effect_handler,
                worker_cleanup_deadline(drain_deadline, config.drain_timeout),
            )
            .await;
            match (result, cleanup) {
                (Ok(terminal), Ok(())) => Ok(terminal),
                (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
                (Err(primary), Ok(())) => Err(primary),
                (Err(primary), Err(cleanup_error)) => {
                    if let Some(suppressed) = admit_health_event(
                        &mut health_events,
                        &mut metrics,
                        HealthEventCategory::Cleanup,
                    ) {
                        otel_warn!(
                            "filelog_receiver.cleanup_failed",
                            error_type = "worker_cleanup",
                            suppressed_events = suppressed
                        );
                    }
                    let _ = cleanup_error;
                    Err(primary)
                }
            }
        } else {
            result
        };
        match result {
            Ok(terminal) => Ok(terminal),
            Err(primary) => Err(finish_terminal_error(
                primary,
                &mut metrics,
                &worker_telemetry,
                &mut health_events,
                &effect_handler,
            )
            .await),
        }
    }
}

fn completion_ack(pending_batch: &mut Option<PendingBatch>, data: &CallData) -> DeliveryDecision {
    match pending_batch {
        Some(pending) => pending.on_ack(data),
        None if key_from_call_data(data).is_none() => {
            DeliveryDecision::Ignored(CompletionIgnore::Malformed)
        }
        None => DeliveryDecision::Ignored(CompletionIgnore::Stale),
    }
}

fn completion_nack(
    pending_batch: &mut Option<PendingBatch>,
    data: &CallData,
    permanent: bool,
    cause: otap_df_engine::control::NackCause,
    config: &RuntimeConfig,
) -> DeliveryDecision {
    match pending_batch {
        Some(pending) => pending.on_nack(data, permanent, cause, &config.retry, config.on_nack),
        None if key_from_call_data(data).is_none() => {
            DeliveryDecision::Ignored(CompletionIgnore::Malformed)
        }
        None => DeliveryDecision::Ignored(CompletionIgnore::Stale),
    }
}

async fn apply_decision(
    decision: DeliveryDecision,
    _config: &RuntimeConfig,
    draining: bool,
    retry_deadline: &mut Option<tokio::time::Instant>,
    worker_tx: &SyncSender<WorkerCommand>,
    effect_handler: &local::EffectHandler<OtapPdata>,
    counters: &mut DeliveryCounters,
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    health_events: &mut HealthEventLimiter,
) -> Result<DecisionOutcome, Error> {
    match decision {
        DeliveryDecision::Ignored(ignored) => {
            counters
                .record_ignore(ignored)
                .map_err(|error| terminal_error(effect_handler, error))?;
            Ok(DecisionOutcome::Continue)
        }
        DeliveryDecision::Commit {
            key,
            explicit_loss,
            exhausted: _,
        } => {
            if explicit_loss {
                counters
                    .record_explicit_loss()
                    .map_err(|error| terminal_error(effect_handler, error))?;
                if let Some(suppressed) =
                    admit_health_event(health_events, metrics, HealthEventCategory::ExplicitLoss)
                {
                    otel_warn!(
                        "filelog_receiver.batch_explicit_loss",
                        reason = "drop_and_continue",
                        suppressed_events = suppressed
                    );
                }
            }
            send_worker_command(
                worker_tx,
                WorkerCommand::Commit {
                    batch_id: key.batch_id,
                    attempt: key.attempt,
                    explicit_loss,
                },
                effect_handler,
            )
            .await?;
            Ok(DecisionOutcome::Continue)
        }
        DeliveryDecision::Retry {
            current: _,
            next_attempt: _,
            backoff,
        } => {
            if draining {
                *retry_deadline = None;
                return Ok(DecisionOutcome::Continue);
            }
            counters
                .record_retry()
                .map_err(|error| terminal_error(effect_handler, error))?;
            record_retry_metrics(metrics, backoff);
            if let Some(suppressed) =
                admit_health_event(health_events, metrics, HealthEventCategory::Retry)
            {
                otel_info!(
                    "filelog_receiver.batch_retry",
                    backoff_ns = duration_ns(backoff),
                    suppressed_events = suppressed
                );
            }
            let deadline = StdInstant::now().checked_add(backoff).ok_or_else(|| {
                terminal_error(effect_handler, "filelog retry deadline overflowed")
            })?;
            *retry_deadline = Some(tokio::time::Instant::from_std(deadline));
            Ok(DecisionOutcome::Continue)
        }
        DeliveryDecision::Fail { key, exhausted: _ } => Ok(DecisionOutcome::Fail(key)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_batch(
    batch: WorkerBatch,
    key: BatchKey,
    control_rx: &mut local::ControlChannel<OtapPdata>,
    effect_handler: &local::EffectHandler<OtapPdata>,
    worker_tx: &SyncSender<WorkerCommand>,
    config: &RuntimeConfig,
    drain_deadline: &mut Option<StdInstant>,
    pending_batch: &mut Option<PendingBatch>,
    counters: &mut DeliveryCounters,
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    worker_telemetry: &WorkerTelemetryBridge,
    health_events: &mut HealthEventLimiter,
) -> Result<SendOutcome, Error> {
    let WorkerBatch {
        batch_id: _,
        attempt: _,
        records,
        record_count,
        logical_bytes,
        source_bytes,
    } = batch;
    if let Some(deadline) = *drain_deadline
        && StdInstant::now() >= deadline
    {
        return Ok(SendOutcome::DrainDeadline(deadline));
    }

    let mut pdata = OtapPdata::new(Context::default(), OtapPayload::OtapArrowRecords(records));
    effect_handler.subscribe_to(
        Interests::ACKS | Interests::NACKS,
        call_data(key),
        &mut pdata,
    );
    match effect_handler.try_send_message_with_source_node(pdata) {
        Ok(()) => {
            record_emitted_batch(metrics, key, record_count, source_bytes, logical_bytes);
            otel_debug!(
                "filelog_receiver.batch_sent",
                batch_id = key.batch_id,
                attempt = u64::from(key.attempt),
                record_count = u64::from(record_count),
                logical_bytes = logical_bytes
            );
            Ok(SendOutcome::Sent)
        }
        Err(TypedError::ChannelSendError(SendError::Full(pdata))) => {
            let backpressure_started = StdInstant::now();
            if let Some(suppressed) =
                admit_health_event(health_events, metrics, HealthEventCategory::Backpressure)
            {
                otel_warn!(
                    "filelog_receiver.downstream_backpressure",
                    suppressed_events = suppressed
                );
            }
            let mut send = Box::pin(effect_handler.send_message_with_source_node(pdata));
            loop {
                let outcome = tokio::select! {
                    biased;

                    _ = async {
                        if let Some(deadline) = *drain_deadline {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                        } else {
                            pending::<()>().await;
                        }
                    }, if drain_deadline.is_some() => {
                        let deadline = drain_deadline.ok_or_else(|| {
                            terminal_error(
                                effect_handler,
                                "filelog blocked-send drain timer fired without a deadline",
                            )
                        })?;
                        SendOutcome::DrainDeadline(deadline)
                    }

                    message = control_rx.recv() => {
                        match message {
                            Ok(NodeControlMsg::DrainIngress { deadline, .. }) => {
                                let deadline = receiver_drain_deadline(deadline, config.drain_timeout);
                                *drain_deadline = Some(drain_deadline.map_or(
                                    deadline,
                                    |current| current.min(deadline),
                                ));
                                // Stronger than journald's current blocked-send path:
                                // admission/read shutdown reaches the worker immediately.
                                send_worker_command(worker_tx, WorkerCommand::Drain, effect_handler).await?;
                                continue;
                            }
                            Ok(NodeControlMsg::Shutdown { deadline, .. }) => {
                                SendOutcome::Shutdown(deadline)
                            }
                            Ok(NodeControlMsg::Ack(ack)) => {
                                counters
                                    .record_ack()
                                    .map_err(|error| terminal_error(effect_handler, error))?;
                                let decision = completion_ack(
                                    pending_batch,
                                    &ack.unwind.route.calldata,
                                );
                                record_completion_metrics(
                                    decision,
                                    true,
                                    metrics,
                                    health_events,
                                );
                                record_blocked_completion(decision, counters, effect_handler)?;
                                continue;
                            }
                            Ok(NodeControlMsg::Nack(nack)) => {
                                counters
                                    .record_nack()
                                    .map_err(|error| terminal_error(effect_handler, error))?;
                                let decision = completion_nack(
                                    pending_batch,
                                    &nack.unwind.route.calldata,
                                    nack.permanent,
                                    nack.cause,
                                    config,
                                );
                                record_completion_metrics(
                                    decision,
                                    false,
                                    metrics,
                                    health_events,
                                );
                                record_blocked_completion(decision, counters, effect_handler)?;
                                continue;
                            }
                            Ok(NodeControlMsg::CollectTelemetry { mut metrics_reporter }) => {
                                report_metrics(metrics, worker_telemetry, &mut metrics_reporter);
                                continue;
                            }
                            Ok(NodeControlMsg::Config { .. }
                                | NodeControlMsg::TimerTick { .. }
                                | NodeControlMsg::Wakeup { .. }
                                | NodeControlMsg::DelayedData { .. }
                                | NodeControlMsg::MemoryPressureChanged { .. }) => {
                                continue;
                            }
                            Err(error) => return Err(Error::ChannelRecvError(error)),
                        }
                    }

                    result = send.as_mut() => {
                        result.map_err(|error| {
                            terminal_error(
                                effect_handler,
                                format!("failed to send filelog batch downstream: {error}"),
                            )
                        })?;
                        record_emitted_batch(
                            metrics,
                            key,
                            record_count,
                            source_bytes,
                            logical_bytes,
                        );
                        SendOutcome::Sent
                    }
                };
                if let Some(metrics) = metrics.as_mut() {
                    metrics
                        .backpressure_pause_duration_ns
                        .record(duration_ns(backpressure_started.elapsed()) as f64);
                }
                return Ok(outcome);
            }
        }
        Err(error) => Err(terminal_error(
            effect_handler,
            format!("failed to send filelog batch downstream: {error}"),
        )),
    }
}

fn record_blocked_completion(
    decision: DeliveryDecision,
    counters: &mut DeliveryCounters,
    effect_handler: &local::EffectHandler<OtapPdata>,
) -> Result<(), Error> {
    match decision {
        DeliveryDecision::Ignored(ignored) => counters
            .record_ignore(ignored)
            .map_err(|error| terminal_error(effect_handler, error)),
        DeliveryDecision::Commit { .. }
        | DeliveryDecision::Retry { .. }
        | DeliveryDecision::Fail { .. } => Err(terminal_error(
            effect_handler,
            "filelog completion became actionable before its downstream send succeeded",
        )),
    }
}

fn receiver_drain_deadline(engine_deadline: StdInstant, drain_timeout: Duration) -> StdInstant {
    StdInstant::now()
        .checked_add(drain_timeout)
        .map_or(engine_deadline, |local| engine_deadline.min(local))
}

fn worker_cleanup_deadline(
    lifecycle_deadline: Option<StdInstant>,
    cleanup_timeout: Duration,
) -> StdInstant {
    lifecycle_deadline.unwrap_or_else(|| {
        let now = StdInstant::now();
        now.checked_add(cleanup_timeout).unwrap_or(now)
    })
}

fn worker_sender(worker: &Option<WorkerHandle>) -> Result<&SyncSender<WorkerCommand>, Error> {
    worker
        .as_ref()
        .map(|worker| &worker.command_tx)
        .ok_or_else(|| Error::InternalError {
            message: "filelog worker handle is missing".to_owned(),
        })
}

async fn send_worker_command(
    sender: &SyncSender<WorkerCommand>,
    command: WorkerCommand,
    effect_handler: &local::EffectHandler<OtapPdata>,
) -> Result<(), Error> {
    let mut command = Some(command);
    for _ in 0..WORKER_COMMAND_RETRY_LIMIT {
        let current = command.take().ok_or_else(|| {
            terminal_error(
                effect_handler,
                "filelog worker command ownership was lost during bounded retry",
            )
        })?;
        match sender.try_send(current) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                command = Some(returned);
                tokio::time::sleep(WORKER_COMMAND_RETRY_DELAY).await;
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(terminal_error(
                    effect_handler,
                    "filelog worker command channel disconnected",
                ));
            }
        }
    }
    Err(terminal_error(
        effect_handler,
        "filelog worker command channel remained saturated",
    ))
}

async fn shutdown_worker(
    worker: &mut Option<WorkerHandle>,
    event_rx: &mut tokio::sync::mpsc::Receiver<WorkerEvent>,
    effect_handler: &local::EffectHandler<OtapPdata>,
    deadline: StdInstant,
) -> Result<(), Error> {
    if let Some(handle) = worker.as_ref() {
        handle.shutdown_requested.store(true, Ordering::Release);
        let _ = handle.command_tx.try_send(WorkerCommand::Shutdown);
    }
    close_and_join_worker(worker, event_rx, effect_handler, deadline).await
}

async fn close_and_join_worker(
    worker: &mut Option<WorkerHandle>,
    event_rx: &mut tokio::sync::mpsc::Receiver<WorkerEvent>,
    effect_handler: &local::EffectHandler<OtapPdata>,
    deadline: StdInstant,
) -> Result<(), Error> {
    let Some(worker) = worker.take() else {
        event_rx.close();
        return Ok(());
    };
    worker.shutdown_requested.store(true, Ordering::Release);
    event_rx.close();
    let receiver_id = effect_handler.receiver_id();
    drop(worker.command_tx);
    let join = worker.join;
    loop {
        if join.is_finished() {
            let worker_result = join.join().map_err(|_| {
                receiver_error(receiver_id.clone(), "filelog worker thread panicked")
            })?;
            return worker_result.map_err(|error| receiver_error(receiver_id, error.to_string()));
        }
        let now = StdInstant::now();
        if now >= deadline {
            otel_warn!(
                "filelog_receiver.worker_detached",
                reason = "lifecycle_deadline"
            );
            drop(join);
            return Ok(());
        }
        let next_poll = now
            .checked_add(WORKER_JOIN_POLL_INTERVAL)
            .map_or(deadline, |candidate| candidate.min(deadline));
        tokio::time::sleep_until(tokio::time::Instant::from_std(next_poll)).await;
    }
}

fn report_metrics(
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    bridge: &WorkerTelemetryBridge,
    reporter: &mut otap_df_telemetry::reporter::MetricsReporter,
) {
    if let Some(metrics) = metrics.as_mut() {
        bridge.drain_into(metrics);
        let _ = reporter.report(metrics);
    }
}

fn record_emitted_batch(
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    key: BatchKey,
    record_count: u32,
    source_bytes: u64,
    logical_bytes: u64,
) {
    let Some(metrics) = metrics.as_mut() else {
        return;
    };
    add_counter_saturating(&mut metrics.batches_emitted, 1);
    add_counter_saturating(&mut metrics.records_emitted, u64::from(record_count));
    add_counter_saturating(&mut metrics.source_bytes_emitted, source_bytes);
    add_counter_saturating(&mut metrics.logical_bytes_emitted, logical_bytes);
    if key.attempt > 1 {
        add_counter_saturating(&mut metrics.batches_resent, 1);
    }
}

fn record_retry_metrics(
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    backoff: Duration,
) {
    if let Some(metrics) = metrics.as_mut() {
        add_counter_saturating(&mut metrics.retry_attempts, 1);
        metrics
            .retry_backoff_duration_ns
            .record(duration_ns(backoff) as f64);
    }
}

fn record_commit_success(
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    explicit_loss: bool,
) {
    if explicit_loss && let Some(metrics) = metrics.as_mut() {
        add_counter_saturating(&mut metrics.batches_explicit_loss, 1);
    }
}

fn record_completion_metrics(
    decision: DeliveryDecision,
    ack: bool,
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    health_events: &mut HealthEventLimiter,
) {
    match decision {
        DeliveryDecision::Ignored(ignored) => {
            if let Some(metrics) = metrics.as_mut() {
                let counter = match ignored {
                    CompletionIgnore::Malformed => &mut metrics.malformed_completions,
                    CompletionIgnore::Stale => &mut metrics.stale_completions,
                    CompletionIgnore::Duplicate => &mut metrics.duplicate_completions,
                };
                add_counter_saturating(counter, 1);
            }
            if let Some(suppressed) =
                admit_health_event(health_events, metrics, HealthEventCategory::Completion)
            {
                let completion_type = match ignored {
                    CompletionIgnore::Malformed => "malformed",
                    CompletionIgnore::Stale => "stale",
                    CompletionIgnore::Duplicate => "duplicate",
                };
                otel_warn!(
                    "filelog_receiver.completion_ignored",
                    completion_type = completion_type,
                    suppressed_events = suppressed
                );
            }
        }
        DeliveryDecision::Commit { exhausted, .. } | DeliveryDecision::Fail { exhausted, .. } => {
            if let Some(metrics) = metrics.as_mut() {
                if ack {
                    add_counter_saturating(&mut metrics.batches_acked, 1);
                } else {
                    add_counter_saturating(&mut metrics.batches_nacked, 1);
                }
                if exhausted {
                    add_counter_saturating(&mut metrics.retry_exhausted, 1);
                }
            }
            if !ack
                && let Some(suppressed) =
                    admit_health_event(health_events, metrics, HealthEventCategory::Retry)
            {
                otel_warn!(
                    "filelog_receiver.batch_retry_terminal",
                    exhausted = exhausted,
                    suppressed_events = suppressed
                );
            }
        }
        DeliveryDecision::Retry { .. } => {
            if let Some(metrics) = metrics.as_mut() {
                add_counter_saturating(&mut metrics.batches_nacked, 1);
            }
        }
    }
}

fn admit_health_event(
    limiter: &mut HealthEventLimiter,
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    category: HealthEventCategory,
) -> Option<u64> {
    match limiter.admit(category, StdInstant::now()) {
        Some(suppressed) => {
            if suppressed != 0 {
                otel_info!(
                    "filelog_receiver.health_events_suppressed",
                    category = category.as_str(),
                    suppressed_events = suppressed
                );
            }
            Some(suppressed)
        }
        None => {
            if let Some(metrics) = metrics.as_mut() {
                add_counter_saturating(&mut metrics.health_events_suppressed, 1);
            }
            None
        }
    }
}

fn record_terminal_failure(
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    health_events: &mut HealthEventLimiter,
) {
    if let Some(metrics) = metrics.as_mut() {
        add_counter_saturating(&mut metrics.terminal_failures, 1);
    }
    if let Some(suppressed) =
        admit_health_event(health_events, metrics, HealthEventCategory::Terminal)
    {
        otel_warn!(
            "filelog_receiver.terminal_failure",
            error_type = "receiver",
            suppressed_events = suppressed
        );
    }
}

async fn finish_terminal_error(
    primary: Error,
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    bridge: &WorkerTelemetryBridge,
    health_events: &mut HealthEventLimiter,
    effect_handler: &local::EffectHandler<OtapPdata>,
) -> Error {
    if let Some(metrics) = metrics.as_mut() {
        bridge.drain_into(metrics);
    }
    record_terminal_failure(metrics, health_events);
    if let Some(metrics) = metrics.as_mut() {
        match effect_handler.report_metrics_reliably(metrics).await {
            Ok(ReportOutcome::Sent) => {}
            Ok(ReportOutcome::Deferred) => {
                otel_warn!(
                    "filelog_receiver.terminal_metrics_deferred",
                    report_outcome = "deferred"
                );
            }
            Err(_) => {
                otel_warn!(
                    "filelog_receiver.terminal_metrics_failed",
                    error_type = "telemetry"
                );
            }
        }
    }
    primary
}

fn terminal_state(
    deadline: StdInstant,
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    bridge: &WorkerTelemetryBridge,
) -> TerminalState {
    let snapshots = terminal_snapshots(metrics, bridge);
    if snapshots.is_empty() {
        TerminalState::new::<[MetricSetSnapshot; 0]>(deadline, [])
    } else {
        TerminalState::new(deadline, snapshots)
    }
}

fn receiver_error(receiver: otap_df_engine::node::NodeId, error: impl Into<String>) -> Error {
    Error::ReceiverError {
        receiver,
        kind: ReceiverErrorKind::Other,
        error: error.into(),
        source_detail: String::new(),
    }
}

fn terminal_error(
    effect_handler: &local::EffectHandler<OtapPdata>,
    error: impl Into<String>,
) -> Error {
    receiver_error(effect_handler.receiver_id(), error)
}

fn log_delivery_counters(config: &RuntimeConfig, counters: &DeliveryCounters) {
    let _ = config;
    otel_debug!(
        "filelog_receiver.delivery_summary",
        acks = counters.acks,
        nacks = counters.nacks,
        retries = counters.retries,
        malformed_completions = counters.malformed_completions,
        stale_completions = counters.stale_completions,
        duplicate_completions = counters.duplicate_completions,
        explicit_loss_commits = counters.explicit_loss_commits,
        checkpoint_failures = counters.checkpoint_failures
    );
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::HashMap;

    use otap_df_channel::mpsc;
    use otap_df_engine::context::ControllerContext;
    use otap_df_engine::control::{
        AckMsg, NackCause, NackMsg, RuntimeControlMsg, runtime_ctrl_msg_channel,
    };
    use otap_df_engine::local::message::{LocalReceiver, LocalSender};
    use otap_df_engine::local::receiver::Receiver as _;
    use otap_df_engine::message::{Receiver as EngineReceiver, Sender as EngineSender};
    use otap_df_engine::testing::{setup_test_runtime, test_node};
    use otap_df_otap::testing::{next_ack, next_nack};
    use otap_df_pdata::otap::{Logs, OtapArrowRecords};
    use otap_df_telemetry::registry::TelemetryRegistryHandle;
    use otap_df_telemetry::reporter::MetricsReporter;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::receivers::filelog_receiver::StartAt;
    use crate::receivers::filelog_receiver::checkpoint::store::{CheckpointStore, StoreOptions};
    use crate::receivers::filelog_receiver::config::Config;
    use crate::receivers::filelog_receiver::telemetry::WorkerCounter;
    use crate::receivers::filelog_receiver::worker::WorkerError;

    fn runtime_config() -> RuntimeConfig {
        let config: Config = serde_json::from_value(json!({
            "include": ["/tmp/*.log"],
            "checkpoint": { "id": "runtime-test" },
            "drain_timeout": "1s"
        }))
        .unwrap();
        RuntimeConfig::from_config(config, "").unwrap()
    }

    fn dummy_pdata() -> OtapPdata {
        OtapPdata::new(
            Context::default(),
            OtapPayload::OtapArrowRecords(OtapArrowRecords::Logs(Logs::default())),
        )
    }

    fn worker_batch() -> WorkerBatch {
        WorkerBatch {
            batch_id: 1,
            attempt: 1,
            records: OtapArrowRecords::Logs(Logs::default()),
            record_count: 1,
            logical_bytes: 128,
            source_bytes: 5,
        }
    }

    fn registered_metrics() -> Option<MetricSet<FilelogReceiverMetrics>> {
        let controller = ControllerContext::new(TelemetryRegistryHandle::new());
        let pipeline = controller.pipeline_context_with("group".into(), "pipeline".into(), 0, 1, 0);
        Some(FilelogReceiverMetrics::register(&pipeline))
    }

    fn terminal_metric_value(
        snapshot: &MetricSetSnapshot,
        name: &str,
    ) -> otap_df_telemetry::metrics::MetricValue {
        let index = snapshot
            .descriptor()
            .metrics
            .iter()
            .position(|field| field.name == name)
            .expect("terminal metric exists");
        snapshot.get_metrics()[index].clone()
    }

    /// Scenario: a worker thread remains gated beyond the active lifecycle
    /// deadline while async teardown closes both of its bounded channels.
    /// Guarantees: teardown returns by the deadline, detaches the finished
    /// handle without occupying Tokio's blocking pool, and the worker can exit.
    #[tokio::test(flavor = "current_thread")]
    async fn worker_join_deadline_detaches_blocked_thread() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            release_rx.recv().expect("test releases blocked worker");
            finished_tx.send(()).expect("worker completion is observed");
            Ok::<(), WorkerError>(())
        });
        let (command_tx, _command_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut worker = Some(WorkerHandle {
            command_tx,
            join,
            telemetry: Arc::new(WorkerTelemetryBridge::default()),
            shutdown_requested: Arc::clone(&shutdown_requested),
        });
        let (_event_tx, mut event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let (_metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
        let (runtime_tx, _runtime_rx) = runtime_ctrl_msg_channel(1);
        let effect_handler = local::EffectHandler::new(
            test_node("filelog"),
            HashMap::new(),
            None,
            runtime_tx,
            metrics_reporter,
        );
        let deadline = StdInstant::now() + Duration::from_millis(10);

        tokio::time::timeout(
            Duration::from_secs(1),
            close_and_join_worker(&mut worker, &mut event_rx, &effect_handler, deadline),
        )
        .await
        .expect("teardown obeys its lifecycle deadline")
        .unwrap();

        assert!(worker.is_none());
        assert!(shutdown_requested.load(Ordering::Acquire));
        assert!(matches!(
            finished_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached worker eventually exits");
    }

    /// Scenario: initial send, resend, Ack, retryable Nack, exhausted Nack,
    /// explicit loss, and every ignored completion category are observed.
    /// Guarantees: delivery telemetry increments each authoritative outcome
    /// exactly once and keeps resend, retry, exhaustion, and completion
    /// classifications distinct.
    #[test]
    fn delivery_metrics_classify_every_completion_and_send_exactly() {
        let mut metrics = registered_metrics();
        let mut health_events = HealthEventLimiter::default();
        record_emitted_batch(&mut metrics, BatchKey::new(1, 1).unwrap(), 2, 10, 100);
        record_emitted_batch(&mut metrics, BatchKey::new(1, 2).unwrap(), 2, 10, 100);
        record_completion_metrics(
            DeliveryDecision::Commit {
                key: BatchKey::new(1, 2).unwrap(),
                explicit_loss: false,
                exhausted: false,
            },
            true,
            &mut metrics,
            &mut health_events,
        );
        record_completion_metrics(
            DeliveryDecision::Retry {
                current: BatchKey::new(2, 1).unwrap(),
                next_attempt: 2,
                backoff: Duration::from_millis(25),
            },
            false,
            &mut metrics,
            &mut health_events,
        );
        record_completion_metrics(
            DeliveryDecision::Fail {
                key: BatchKey::new(2, 2).unwrap(),
                exhausted: true,
            },
            false,
            &mut metrics,
            &mut health_events,
        );
        for ignored in [
            CompletionIgnore::Malformed,
            CompletionIgnore::Stale,
            CompletionIgnore::Duplicate,
        ] {
            record_completion_metrics(
                DeliveryDecision::Ignored(ignored),
                true,
                &mut metrics,
                &mut health_events,
            );
        }
        record_retry_metrics(&mut metrics, Duration::from_millis(25));
        record_commit_success(&mut metrics, true);

        let metrics = metrics.unwrap();
        assert_eq!(metrics.batches_emitted.get(), 2);
        assert_eq!(metrics.batches_resent.get(), 1);
        assert_eq!(metrics.records_emitted.get(), 4);
        assert_eq!(metrics.source_bytes_emitted.get(), 20);
        assert_eq!(metrics.logical_bytes_emitted.get(), 200);
        assert_eq!(metrics.batches_acked.get(), 1);
        assert_eq!(metrics.batches_nacked.get(), 2);
        assert_eq!(metrics.retry_attempts.get(), 1);
        assert_eq!(metrics.retry_exhausted.get(), 1);
        assert_eq!(metrics.batches_explicit_loss.get(), 1);
        assert_eq!(metrics.malformed_completions.get(), 1);
        assert_eq!(metrics.stale_completions.get(), 1);
        assert_eq!(metrics.duplicate_completions.get(), 1);
        assert_eq!(metrics.retry_backoff_duration_ns.get().count, 1);
        assert_eq!(
            metrics.retry_backoff_duration_ns.get().sum,
            duration_ns(Duration::from_millis(25)) as f64
        );
    }

    /// Scenario: a terminal receiver error follows an uncollected worker
    /// counter while the production reporter has capacity.
    /// Guarantees: the final worker delta and exactly one lifecycle failure
    /// are submitted reliably before the unchanged primary error is returned.
    #[tokio::test(flavor = "current_thread")]
    async fn terminal_error_reliably_reports_final_worker_metrics_once() {
        let (metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(2);
        let (runtime_tx, _runtime_rx) = runtime_ctrl_msg_channel(1);
        let effect_handler = local::EffectHandler::new(
            test_node("filelog"),
            HashMap::new(),
            None,
            runtime_tx,
            metrics_reporter,
        );
        let mut metrics = registered_metrics();
        let bridge = WorkerTelemetryBridge::default();
        bridge.add(WorkerCounter::SourceBytesRead, 9);
        let mut health_events = HealthEventLimiter::default();
        let primary = terminal_error(&effect_handler, "primary receiver failure");

        let returned = finish_terminal_error(
            primary,
            &mut metrics,
            &bridge,
            &mut health_events,
            &effect_handler,
        )
        .await;
        assert!(returned.to_string().contains("primary receiver failure"));
        let snapshot = metrics_rx.recv().unwrap();
        assert_eq!(
            terminal_metric_value(&snapshot, "lifecycle.failures"),
            otap_df_telemetry::metrics::MetricValue::U64(1)
        );
        assert_eq!(
            terminal_metric_value(&snapshot, "source.bytes.read"),
            otap_df_telemetry::metrics::MetricValue::U64(9)
        );
        assert!(metrics_rx.try_recv().is_err());
    }

    /// Scenario: terminal telemetry reporting fails after cleanup has already
    /// produced the primary receiver error.
    /// Guarantees: reporting neither replaces the primary error nor repeats
    /// lifecycle accounting, and the unsent hot set remains available.
    #[tokio::test(flavor = "current_thread")]
    async fn terminal_reporter_failure_does_not_mask_primary_error() {
        let (metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
        drop(metrics_rx);
        let (runtime_tx, _runtime_rx) = runtime_ctrl_msg_channel(1);
        let effect_handler = local::EffectHandler::new(
            test_node("filelog"),
            HashMap::new(),
            None,
            runtime_tx,
            metrics_reporter,
        );
        let mut metrics = registered_metrics();
        let bridge = WorkerTelemetryBridge::default();
        bridge.add(WorkerCounter::SourceBytesRead, 4);
        let mut health_events = HealthEventLimiter::default();
        let primary = terminal_error(&effect_handler, "primary receiver failure");

        let returned = finish_terminal_error(
            primary,
            &mut metrics,
            &bridge,
            &mut health_events,
            &effect_handler,
        )
        .await;
        assert!(returned.to_string().contains("primary receiver failure"));
        let metrics = metrics.expect("failed report retains the hot metric set");
        assert_eq!(metrics.terminal_failures.get(), 1);
        assert_eq!(metrics.source_bytes_read.get(), 4);
    }

    /// Scenario: terminal telemetry uses a standalone reporter whose bounded
    /// channel already contains an earlier snapshot.
    /// Guarantees: a deferred terminal snapshot emits a fixed failure signal,
    /// preserves the primary error, and retains every hot metric for diagnosis.
    #[tokio::test(flavor = "current_thread")]
    async fn full_terminal_reporter_retains_unsent_metrics() {
        let (metrics_rx, mut metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
        let mut occupying_metrics = registered_metrics().unwrap();
        add_counter_saturating(&mut occupying_metrics.starts, 1);
        assert_eq!(
            metrics_reporter
                .report_with_outcome(&mut occupying_metrics)
                .unwrap(),
            ReportOutcome::Sent
        );
        let (runtime_tx, _runtime_rx) = runtime_ctrl_msg_channel(1);
        let effect_handler = local::EffectHandler::new(
            test_node("filelog"),
            HashMap::new(),
            None,
            runtime_tx,
            metrics_reporter,
        );
        let mut metrics = registered_metrics();
        let bridge = WorkerTelemetryBridge::default();
        bridge.add(WorkerCounter::SourceBytesRead, 6);
        let mut health_events = HealthEventLimiter::default();
        let primary = terminal_error(&effect_handler, "primary receiver failure");

        let returned = finish_terminal_error(
            primary,
            &mut metrics,
            &bridge,
            &mut health_events,
            &effect_handler,
        )
        .await;

        assert!(returned.to_string().contains("primary receiver failure"));
        let metrics = metrics.expect("deferred report retains the hot metric set");
        assert_eq!(metrics.terminal_failures.get(), 1);
        assert_eq!(metrics.source_bytes_read.get(), 6);
        let _occupying_snapshot = metrics_rx.recv().unwrap();
        assert!(metrics_rx.try_recv().is_err());
    }

    /// Scenario: worker setup cannot acquire the checkpoint namespace and
    /// therefore never produces a usable runtime handle.
    /// Guarantees: start and one terminal failure plus the last setup counter
    /// reach the owned reporter before the receiver returns its setup error.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_worker_setup_reports_start_failure_and_worker_delta() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("unused.log");
        let mut runtime = source_runtime(&source, &directory.path().join("checkpoint"));
        runtime.checkpoint.ownership_timeout = Duration::from_millis(20);
        let _namespace_owner =
            CheckpointStore::open(StoreOptions::from_runtime_config(&runtime)).unwrap();
        let (control_tx, control_rx) = mpsc::Channel::new(1);
        let control_rx =
            local::ControlChannel::new(EngineReceiver::Local(LocalReceiver::mpsc(control_rx)));
        let (metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(2);
        let (runtime_tx, _runtime_rx) = runtime_ctrl_msg_channel(1);
        let effect_handler = local::EffectHandler::new(
            test_node("filelog"),
            HashMap::new(),
            None,
            runtime_tx,
            metrics_reporter,
        );
        let mut receiver = FilelogReceiver::new(runtime);
        receiver.metrics = registered_metrics();

        let error = match Box::new(receiver).start(control_rx, effect_handler).await {
            Ok(_) => panic!("namespace contention unexpectedly started the receiver"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("namespace"));
        let snapshot = metrics_rx.recv().unwrap();
        assert_eq!(
            terminal_metric_value(&snapshot, "lifecycle.starts"),
            otap_df_telemetry::metrics::MetricValue::U64(1)
        );
        assert_eq!(
            terminal_metric_value(&snapshot, "lifecycle.failures"),
            otap_df_telemetry::metrics::MetricValue::U64(1)
        );
        assert_eq!(
            terminal_metric_value(&snapshot, "ownership.namespace_lock.failures"),
            otap_df_telemetry::metrics::MetricValue::U64(1)
        );
        drop(control_tx);
    }

    fn source_runtime(source: &std::path::Path, namespace_dir: &std::path::Path) -> RuntimeConfig {
        let mut config: Config = serde_json::from_value(json!({
            "include": [source.to_str().unwrap()],
            "checkpoint": { "id": "runtime-source-test" }
        }))
        .unwrap();
        config.start_at = StartAt::Beginning;
        config.discovery.poll_interval = Duration::from_millis(10);
        config.limits.max_tracked_files = 1;
        config.limits.max_pending_candidates = 1;
        config.limits.max_open_files = 1;
        config.limits.max_read_bytes_per_turn = 64;
        config.batch.max_records = 1;
        config.drain_timeout = Duration::from_secs(10);
        let mut runtime = RuntimeConfig::from_config(config, "").unwrap();
        runtime.checkpoint_namespace_dir = namespace_dir.to_path_buf();
        runtime
    }

    /// Scenario: a filelog batch is blocked behind a full downstream channel
    /// when DrainIngress arrives with an already-due deadline.
    /// Guarantees: Drain is handed to the blocking worker immediately, the
    /// downstream send is interrupted, and no Commit command is synthesized.
    #[tokio::test]
    async fn blocked_send_races_drain_and_never_commits() {
        let (control_tx, control_rx) = mpsc::Channel::new(4);
        control_tx
            .send(NodeControlMsg::DrainIngress {
                deadline: StdInstant::now(),
                reason: "test drain".to_owned(),
            })
            .unwrap();
        let mut control_rx =
            local::ControlChannel::new(EngineReceiver::Local(LocalReceiver::mpsc(control_rx)));

        let (output_tx, _output_rx) = mpsc::Channel::new(1);
        output_tx.send(dummy_pdata()).unwrap();
        let mut outputs = HashMap::new();
        let _ = outputs.insert(
            Cow::Borrowed("out"),
            EngineSender::Local(LocalSender::mpsc(output_tx)),
        );
        let (runtime_tx, _runtime_rx) = runtime_ctrl_msg_channel(4);
        let (_metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
        let effect_handler = local::EffectHandler::new(
            test_node("filelog"),
            outputs,
            Some(Cow::Borrowed("out")),
            runtime_tx,
            metrics_reporter,
        );
        let (worker_tx, worker_rx) = std::sync::mpsc::sync_channel(4);
        let mut drain_deadline = None;
        let mut pending_batch = None;
        let mut counters = DeliveryCounters::default();
        let mut metrics = None;
        let worker_telemetry = WorkerTelemetryBridge::default();
        let mut health_events = HealthEventLimiter::default();

        let outcome = send_batch(
            worker_batch(),
            BatchKey::new(1, 1).unwrap(),
            &mut control_rx,
            &effect_handler,
            &worker_tx,
            &runtime_config(),
            &mut drain_deadline,
            &mut pending_batch,
            &mut counters,
            &mut metrics,
            &worker_telemetry,
            &mut health_events,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, SendOutcome::DrainDeadline(_)));
        assert_eq!(worker_rx.try_recv().unwrap(), WorkerCommand::Drain);
        assert!(matches!(
            worker_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(pending_batch.is_none());
    }

    /// Scenario: a filelog batch is blocked behind a full downstream channel
    /// when Shutdown arrives.
    /// Guarantees: the send future is interrupted immediately and the helper
    /// returns the engine deadline without creating pending or checkpoint
    /// state.
    #[tokio::test]
    async fn blocked_send_races_shutdown_and_never_commits() {
        let deadline = StdInstant::now();
        let (control_tx, control_rx) = mpsc::Channel::new(4);
        control_tx
            .send(NodeControlMsg::Shutdown {
                deadline,
                reason: "test shutdown".to_owned(),
            })
            .unwrap();
        let mut control_rx =
            local::ControlChannel::new(EngineReceiver::Local(LocalReceiver::mpsc(control_rx)));

        let (output_tx, _output_rx) = mpsc::Channel::new(1);
        output_tx.send(dummy_pdata()).unwrap();
        let mut outputs = HashMap::new();
        let _ = outputs.insert(
            Cow::Borrowed("out"),
            EngineSender::Local(LocalSender::mpsc(output_tx)),
        );
        let (runtime_tx, _runtime_rx) = runtime_ctrl_msg_channel(4);
        let (_metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
        let effect_handler = local::EffectHandler::new(
            test_node("filelog"),
            outputs,
            Some(Cow::Borrowed("out")),
            runtime_tx,
            metrics_reporter,
        );
        let (worker_tx, worker_rx) = std::sync::mpsc::sync_channel(4);
        let mut drain_deadline = None;
        let mut pending_batch = None;
        let mut counters = DeliveryCounters::default();
        let mut metrics = None;
        let worker_telemetry = WorkerTelemetryBridge::default();
        let mut health_events = HealthEventLimiter::default();

        let outcome = send_batch(
            worker_batch(),
            BatchKey::new(1, 1).unwrap(),
            &mut control_rx,
            &effect_handler,
            &worker_tx,
            &runtime_config(),
            &mut drain_deadline,
            &mut pending_batch,
            &mut counters,
            &mut metrics,
            &worker_telemetry,
            &mut health_events,
        )
        .await
        .unwrap();
        assert_eq!(outcome, SendOutcome::Shutdown(deadline));
        assert!(matches!(
            worker_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(pending_batch.is_none());
    }

    /// Scenario: DrainIngress reaches the complete local receiver after one
    /// batch was sent, and a real routed Ack arrives before its deadline.
    /// Guarantees: async correlation requests the worker commit, drain waits
    /// for persistence, receiver-drained notification is emitted, and reopen
    /// observes exact EOF.
    #[test]
    fn drain_ack_before_deadline_commits_and_notifies() {
        let (tokio_runtime, local_tasks) = setup_test_runtime();
        tokio_runtime.block_on(local_tasks.run_until(async {
            let directory = tempdir().unwrap();
            let source = directory.path().join("ack.log");
            std::fs::write(&source, b"line\n").unwrap();
            let runtime = source_runtime(&source, &directory.path().join("checkpoint"));
            let reopened_runtime = runtime.clone();

            let (control_tx, control_rx) = mpsc::Channel::new(8);
            let control_rx =
                local::ControlChannel::new(EngineReceiver::Local(LocalReceiver::mpsc(control_rx)));
            let (output_tx, output_rx) = mpsc::Channel::new(1);
            let mut outputs = HashMap::new();
            let _ = outputs.insert(
                Cow::Borrowed("out"),
                EngineSender::Local(LocalSender::mpsc(output_tx)),
            );
            let (runtime_tx, mut runtime_rx) = runtime_ctrl_msg_channel(4);
            let (_metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
            let effect_handler = local::EffectHandler::new(
                test_node("filelog"),
                outputs,
                Some(Cow::Borrowed("out")),
                runtime_tx,
                metrics_reporter,
            );
            let receiver = tokio::task::spawn_local(async move {
                let mut filelog = FilelogReceiver::new(runtime);
                filelog.metrics = registered_metrics();
                Box::new(filelog).start(control_rx, effect_handler).await
            });

            let forwarded = tokio::time::timeout(Duration::from_secs(5), output_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let deadline = StdInstant::now() + Duration::from_secs(10);
            control_tx
                .send_async(NodeControlMsg::DrainIngress {
                    deadline,
                    reason: "test drain".to_owned(),
                })
                .await
                .unwrap();
            let (_, ack) = next_ack(AckMsg::new(forwarded)).expect("filelog subscribed to Ack");
            control_tx
                .send_async(NodeControlMsg::Ack(ack))
                .await
                .unwrap();

            let terminal = tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(terminal.deadline(), deadline);
            assert_eq!(terminal.metrics().len(), 1);
            assert_eq!(
                terminal_metric_value(&terminal.metrics()[0], "lifecycle.starts"),
                otap_df_telemetry::metrics::MetricValue::U64(1)
            );
            assert_eq!(
                terminal_metric_value(&terminal.metrics()[0], "lifecycle.drains"),
                otap_df_telemetry::metrics::MetricValue::U64(1)
            );
            assert_eq!(
                terminal_metric_value(&terminal.metrics()[0], "batches.acked"),
                otap_df_telemetry::metrics::MetricValue::U64(1)
            );
            assert!(matches!(
                runtime_rx.recv().await.unwrap(),
                RuntimeControlMsg::ReceiverDrained { .. }
            ));
            let store = CheckpointStore::open(StoreOptions::from_runtime_config(&reopened_runtime))
                .unwrap();
            assert_eq!(store.table().iter().next().unwrap().1.committed_offset, 5);
        }));
    }

    /// Scenario: DrainIngress reaches the complete local receiver with one
    /// unacknowledged batch, then paused Tokio time advances past the deadline.
    /// Guarantees: the blocked completion wait is interrupted without Ack or
    /// synthetic commit, receiver-drained is notified, and reopen remains at
    /// the original durable offset.
    #[test]
    fn drain_deadline_before_ack_preserves_checkpoint() {
        let (tokio_runtime, local_tasks) = setup_test_runtime();
        tokio_runtime.block_on(local_tasks.run_until(async {
            let directory = tempdir().unwrap();
            let source = directory.path().join("deadline.log");
            std::fs::write(&source, b"line\n").unwrap();
            let runtime = source_runtime(&source, &directory.path().join("checkpoint"));
            let reopened_runtime = runtime.clone();

            let (control_tx, control_rx) = mpsc::Channel::new(8);
            let control_rx =
                local::ControlChannel::new(EngineReceiver::Local(LocalReceiver::mpsc(control_rx)));
            let (output_tx, output_rx) = mpsc::Channel::new(1);
            let mut outputs = HashMap::new();
            let _ = outputs.insert(
                Cow::Borrowed("out"),
                EngineSender::Local(LocalSender::mpsc(output_tx)),
            );
            let (runtime_tx, mut runtime_rx) = runtime_ctrl_msg_channel(4);
            let (_metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
            let effect_handler = local::EffectHandler::new(
                test_node("filelog"),
                outputs,
                Some(Cow::Borrowed("out")),
                runtime_tx,
                metrics_reporter,
            );
            let receiver = tokio::task::spawn_local(async move {
                Box::new(FilelogReceiver::new(runtime))
                    .start(control_rx, effect_handler)
                    .await
            });

            let _unacked = tokio::time::timeout(Duration::from_secs(5), output_rx.recv())
                .await
                .unwrap()
                .unwrap();
            tokio::time::pause();
            let deadline = StdInstant::now() + Duration::from_secs(10);
            control_tx
                .send_async(NodeControlMsg::DrainIngress {
                    deadline,
                    reason: "test deadline".to_owned(),
                })
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(11)).await;

            let terminal = receiver.await.unwrap().unwrap();
            assert_eq!(terminal.deadline(), deadline);
            assert!(matches!(
                runtime_rx.recv().await.unwrap(),
                RuntimeControlMsg::ReceiverDrained { .. }
            ));
            let store = CheckpointStore::open(StoreOptions::from_runtime_config(&reopened_runtime))
                .unwrap();
            assert_eq!(store.table().iter().next().unwrap().1.committed_offset, 0);
        }));
    }

    fn assert_drain_suppresses_retry(drain_before_nack: bool) {
        let (tokio_runtime, local_tasks) = setup_test_runtime();
        tokio_runtime.block_on(local_tasks.run_until(async {
            let directory = tempdir().unwrap();
            let source = directory.path().join("retry.log");
            std::fs::write(&source, b"line\n").unwrap();
            let runtime = source_runtime(&source, &directory.path().join("checkpoint"));
            let reopened_runtime = runtime.clone();

            let (control_tx, control_rx) = mpsc::Channel::new(8);
            let control_rx =
                local::ControlChannel::new(EngineReceiver::Local(LocalReceiver::mpsc(control_rx)));
            let (output_tx, output_rx) = mpsc::Channel::new(2);
            let mut outputs = HashMap::new();
            let _ = outputs.insert(
                Cow::Borrowed("out"),
                EngineSender::Local(LocalSender::mpsc(output_tx)),
            );
            let (runtime_tx, mut runtime_rx) = runtime_ctrl_msg_channel(4);
            let (_metrics_rx, metrics_reporter) = MetricsReporter::create_new_and_receiver(1);
            let effect_handler = local::EffectHandler::new(
                test_node("filelog"),
                outputs,
                Some(Cow::Borrowed("out")),
                runtime_tx,
                metrics_reporter,
            );
            let receiver = tokio::task::spawn_local(async move {
                Box::new(FilelogReceiver::new(runtime))
                    .start(control_rx, effect_handler)
                    .await
            });

            let forwarded = tokio::time::timeout(Duration::from_secs(5), output_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let (_, nack) = next_nack(NackMsg::new_with_cause(
                "retryable route saturation",
                forwarded,
                NackCause::RouteFull,
            ))
            .expect("filelog subscribed to Nack");

            tokio::time::pause();
            let deadline = StdInstant::now() + Duration::from_secs(10);
            let drain = NodeControlMsg::DrainIngress {
                deadline,
                reason: "test retry drain".to_owned(),
            };
            if drain_before_nack {
                control_tx.send_async(drain).await.unwrap();
                control_tx
                    .send_async(NodeControlMsg::Nack(nack))
                    .await
                    .unwrap();
            } else {
                control_tx
                    .send_async(NodeControlMsg::Nack(nack))
                    .await
                    .unwrap();
                control_tx.send_async(drain).await.unwrap();
            }

            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(
                output_rx.try_recv().is_err(),
                "Drain must suppress a downstream resend"
            );
            tokio::time::advance(Duration::from_secs(10)).await;

            let terminal = receiver.await.unwrap().unwrap();
            assert_eq!(terminal.deadline(), deadline);
            assert!(matches!(
                runtime_rx.recv().await.unwrap(),
                RuntimeControlMsg::ReceiverDrained { .. }
            ));
            let store = CheckpointStore::open(StoreOptions::from_runtime_config(&reopened_runtime))
                .unwrap();
            assert_eq!(store.table().iter().next().unwrap().1.committed_offset, 0);
        }));
    }

    /// Scenario: DrainIngress arrives after a retryable Nack has armed the
    /// retained batch's retry backoff.
    /// Guarantees: Drain cancels the backoff, emits no resend, and leaves the
    /// durable checkpoint unchanged when its deadline expires.
    #[test]
    fn drain_during_backoff_cancels_retry() {
        assert_drain_suppresses_retry(false);
    }

    /// Scenario: DrainIngress arrives while a batch awaits completion, then
    /// its matching retryable Nack arrives during the drain window.
    /// Guarantees: the Nack cannot arm a new retry, no resend occurs, and the
    /// drain deadline leaves durable progress unchanged.
    #[test]
    fn retryable_nack_during_drain_does_not_arm_retry() {
        assert_drain_suppresses_retry(true);
    }
}
