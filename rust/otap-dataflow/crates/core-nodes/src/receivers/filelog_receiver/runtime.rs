// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Local async delivery runtime for the blocking filelog worker.

use std::future::pending;
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
use otap_df_telemetry::metrics::MetricSetSnapshot;
use otap_df_telemetry::{otel_debug, otel_info, otel_warn};

use super::config::RuntimeConfig;
use super::delivery::{
    BatchKey, CompletionIgnore, DeliveryDecision, PendingBatch, call_data, key_from_call_data,
};
use super::worker::{
    WORKER_EVENT_CONTROL_SLOTS, WorkerBatch, WorkerCommand, WorkerEvent, WorkerHandle, spawn_worker,
};

const WORKER_COMMAND_RETRY_LIMIT: usize = 200;
const WORKER_COMMAND_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Runtime receiver constructed only after factory validation.
pub(super) struct FilelogReceiver {
    config: RuntimeConfig,
}

impl FilelogReceiver {
    pub(super) const fn new(config: RuntimeConfig) -> Self {
        Self { config }
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
        let FilelogReceiver { config } = *self;
        otel_info!(
            "filelog_receiver.start",
            checkpoint_id = config.checkpoint_id.as_str()
        );

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1 + WORKER_EVENT_CONTROL_SLOTS);
        let mut worker = Some(
            spawn_worker(config.clone(), event_tx)
                .map_err(|error| terminal_error(&effect_handler, error.to_string()))?,
        );
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
                        otel_warn!(
                            "filelog_receiver.drain_timeout",
                            checkpoint_id = config.checkpoint_id.as_str(),
                            message = "Drain deadline reached with an unacknowledged retained batch; checkpoint progress remains unchanged"
                        );
                    }
                    shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                    effect_handler.notify_receiver_drained().await?;
                    log_delivery_counters(&config, &counters);
                    return Ok(terminal_state(deadline));
                }

                message = control_rx.recv() => {
                    match message {
                        Ok(NodeControlMsg::Ack(ack)) => {
                            counters.record_ack().map_err(|error| terminal_error(&effect_handler, error))?;
                            let decision = completion_ack(&mut pending_batch, &ack.unwind.route.calldata);
                            match apply_decision(
                                decision,
                                &config,
                                drain_deadline.is_some(),
                                &mut retry_deadline,
                                worker_sender(&worker)?,
                                &effect_handler,
                                &mut counters,
                            ).await? {
                                DecisionOutcome::Continue => {}
                                DecisionOutcome::Fail(key) => {
                                    shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
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
                            match apply_decision(
                                decision,
                                &config,
                                drain_deadline.is_some(),
                                &mut retry_deadline,
                                worker_sender(&worker)?,
                                &effect_handler,
                                &mut counters,
                            ).await? {
                                DecisionOutcome::Continue => {}
                                DecisionOutcome::Fail(key) => {
                                    shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
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
                                "filelog_receiver.drain_ingress",
                                checkpoint_id = config.checkpoint_id.as_str()
                            );
                        }
                        Ok(NodeControlMsg::Shutdown { deadline, .. }) => {
                            shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                            log_delivery_counters(&config, &counters);
                            return Ok(terminal_state(deadline));
                        }
                        Ok(NodeControlMsg::CollectTelemetry { .. }
                            | NodeControlMsg::Config { .. }
                            | NodeControlMsg::TimerTick { .. }
                            | NodeControlMsg::Wakeup { .. }
                            | NodeControlMsg::DelayedData { .. }
                            | NodeControlMsg::MemoryPressureChanged { .. }) => {}
                        Err(error) => {
                            shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
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
                                    shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                                    return Err(terminal_error(
                                        &effect_handler,
                                        format!(
                                            "filelog worker resend ({}, {}) does not match async pending state",
                                            key.batch_id, key.attempt
                                        ),
                                    ));
                                }
                            } else if key.attempt != 1 {
                                shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
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
                            ).await? {
                                SendOutcome::Sent => {
                                    if resend {
                                        if !pending_batch
                                            .as_mut()
                                            .is_some_and(|pending| pending.send_succeeded(key))
                                        {
                                            shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
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
                                    shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                                    effect_handler.notify_receiver_drained().await?;
                                    log_delivery_counters(&config, &counters);
                                    return Ok(terminal_state(deadline));
                                }
                                SendOutcome::Shutdown(deadline) => {
                                    shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                                    log_delivery_counters(&config, &counters);
                                    return Ok(terminal_state(deadline));
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
                                shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                                return Err(terminal_error(
                                    &effect_handler,
                                    format!(
                                        "filelog commit result ({batch_id}, {attempt}, loss={explicit_loss}) does not match pending state"
                                    ),
                                ));
                            }
                            match result {
                                Ok(()) => {
                                    pending_batch = None;
                                    retry_deadline = None;
                                    consecutive_checkpoint_failures = 0;
                                }
                                Err(error) => {
                                    counters.record_checkpoint_failure().map_err(|message| terminal_error(&effect_handler, message))?;
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
                                        shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
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
                                shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                                return Err(terminal_error(
                                    &effect_handler,
                                    "filelog worker reported drained with a pending batch",
                                ));
                            }
                            let deadline = drain_deadline.unwrap_or_else(StdInstant::now);
                            shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                            effect_handler.notify_receiver_drained().await?;
                            log_delivery_counters(&config, &counters);
                            return Ok(terminal_state(deadline));
                        }
                        Some(WorkerEvent::Failed(message)) => {
                            close_and_join_worker(&mut worker, &mut event_rx, &effect_handler).await?;
                            return Err(terminal_error(&effect_handler, message));
                        }
                        Some(WorkerEvent::Stopped) | None => {
                            close_and_join_worker(&mut worker, &mut event_rx, &effect_handler).await?;
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

        if worker.is_some() {
            let cleanup = shutdown_worker(&mut worker, &mut event_rx, &effect_handler).await;
            match (result, cleanup) {
                (Ok(terminal), Ok(())) => Ok(terminal),
                (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
                (Err(primary), Ok(())) => Err(primary),
                (Err(primary), Err(cleanup_error)) => {
                    otel_warn!(
                        "filelog_receiver.cleanup_failed",
                        checkpoint_id = config.checkpoint_id.as_str(),
                        error = cleanup_error.to_string()
                    );
                    Err(primary)
                }
            }
        } else {
            result
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
) -> Result<DecisionOutcome, Error> {
    match decision {
        DeliveryDecision::Ignored(ignored) => {
            counters
                .record_ignore(ignored)
                .map_err(|error| terminal_error(effect_handler, error))?;
            Ok(DecisionOutcome::Continue)
        }
        DeliveryDecision::Commit { key, explicit_loss } => {
            if explicit_loss {
                counters
                    .record_explicit_loss()
                    .map_err(|error| terminal_error(effect_handler, error))?;
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
            let deadline = StdInstant::now().checked_add(backoff).ok_or_else(|| {
                terminal_error(effect_handler, "filelog retry deadline overflowed")
            })?;
            *retry_deadline = Some(tokio::time::Instant::from_std(deadline));
            Ok(DecisionOutcome::Continue)
        }
        DeliveryDecision::Fail { key } => Ok(DecisionOutcome::Fail(key)),
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
) -> Result<SendOutcome, Error> {
    let WorkerBatch {
        batch_id: _,
        attempt: _,
        records,
        record_count,
        logical_bytes,
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
            otel_debug!(
                "filelog_receiver.batch_sent",
                checkpoint_id = config.checkpoint_id.as_str(),
                batch_id = key.batch_id,
                attempt = u64::from(key.attempt),
                record_count = u64::from(record_count),
                logical_bytes = logical_bytes
            );
            Ok(SendOutcome::Sent)
        }
        Err(TypedError::ChannelSendError(SendError::Full(pdata))) => {
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
                                record_blocked_completion(decision, counters, effect_handler)?;
                                continue;
                            }
                            Ok(NodeControlMsg::CollectTelemetry { .. }
                                | NodeControlMsg::Config { .. }
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
                        SendOutcome::Sent
                    }
                };
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
) -> Result<(), Error> {
    if let Some(handle) = worker.as_ref() {
        let _ =
            send_worker_command(&handle.command_tx, WorkerCommand::Shutdown, effect_handler).await;
    }
    close_and_join_worker(worker, event_rx, effect_handler).await
}

async fn close_and_join_worker(
    worker: &mut Option<WorkerHandle>,
    event_rx: &mut tokio::sync::mpsc::Receiver<WorkerEvent>,
    effect_handler: &local::EffectHandler<OtapPdata>,
) -> Result<(), Error> {
    event_rx.close();
    let Some(worker) = worker.take() else {
        return Ok(());
    };
    let receiver_id = effect_handler.receiver_id();
    drop(worker.command_tx);
    let worker_result = tokio::task::spawn_blocking(move || worker.join.join())
        .await
        .map_err(|error| {
            receiver_error(
                receiver_id.clone(),
                format!("filelog worker join task failed: {error}"),
            )
        })?
        .map_err(|_| receiver_error(receiver_id.clone(), "filelog worker thread panicked"))?;
    worker_result.map_err(|error| receiver_error(receiver_id, error.to_string()))
}

fn terminal_state(deadline: StdInstant) -> TerminalState {
    TerminalState::new::<[MetricSetSnapshot; 0]>(deadline, [])
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
    otel_debug!(
        "filelog_receiver.delivery_summary",
        checkpoint_id = config.checkpoint_id.as_str(),
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
    use otap_df_engine::control::{
        AckMsg, NackCause, NackMsg, RuntimeControlMsg, runtime_ctrl_msg_channel,
    };
    use otap_df_engine::local::message::{LocalReceiver, LocalSender};
    use otap_df_engine::local::receiver::Receiver as _;
    use otap_df_engine::message::{Receiver as EngineReceiver, Sender as EngineSender};
    use otap_df_engine::testing::{setup_test_runtime, test_node};
    use otap_df_otap::testing::{next_ack, next_nack};
    use otap_df_pdata::otap::{Logs, OtapArrowRecords};
    use otap_df_telemetry::reporter::MetricsReporter;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::receivers::filelog_receiver::StartAt;
    use crate::receivers::filelog_receiver::checkpoint::store::{CheckpointStore, StoreOptions};
    use crate::receivers::filelog_receiver::config::Config;

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
        }
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
                Box::new(FilelogReceiver::new(runtime))
                    .start(control_rx, effect_handler)
                    .await
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
