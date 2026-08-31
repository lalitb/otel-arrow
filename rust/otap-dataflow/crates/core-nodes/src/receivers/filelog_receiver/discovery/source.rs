// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Dedicated periodic discovery-thread lifecycle and bounded handoff.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use super::admission::AdmissionController;
use super::scanner::{DiscoveryPlan, FilesystemScanner};
use super::{DiscoveryError, DiscoveryFeedback, DiscoveryMessage};
use crate::receivers::filelog_receiver::environment::DescriptorPressure;

const EVENT_CHANNEL_CAPACITY: usize = 1;
const COMMAND_CHANNEL_CAPACITY: usize = 2;
const FULL_CHANNEL_COMMAND_POLL: Duration = Duration::from_millis(50);
const SHUTDOWN_SIGNAL_POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
enum DiscoveryCommand {
    Feedback(DiscoveryFeedback),
    ScanNow,
    Shutdown,
}

/// Nonblocking feedback handoff failure that returns ownership for retry.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FeedbackSendError {
    /// The fixed command queue is temporarily full.
    #[error("filelog discovery command channel is full")]
    Full(DiscoveryFeedback),
    /// The discovery thread has stopped.
    #[error("filelog discovery command channel disconnected")]
    Disconnected(DiscoveryFeedback),
}

/// Controller and ordered event stream for one dedicated discovery thread.
#[derive(Debug)]
pub(crate) struct DiscoveryHandle {
    command_tx: SyncSender<DiscoveryCommand>,
    message_rx: Receiver<DiscoveryMessage>,
    shutdown_requested: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl DiscoveryHandle {
    pub(crate) fn send_feedback(
        &self,
        feedback: DiscoveryFeedback,
    ) -> Result<(), FeedbackSendError> {
        match self
            .command_tx
            .try_send(DiscoveryCommand::Feedback(feedback))
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(DiscoveryCommand::Feedback(feedback))) => {
                Err(FeedbackSendError::Full(feedback))
            }
            Err(TrySendError::Disconnected(DiscoveryCommand::Feedback(feedback))) => {
                Err(FeedbackSendError::Disconnected(feedback))
            }
            Err(TrySendError::Full(DiscoveryCommand::ScanNow | DiscoveryCommand::Shutdown))
            | Err(TrySendError::Disconnected(
                DiscoveryCommand::ScanNow | DiscoveryCommand::Shutdown,
            )) => unreachable!("send_feedback submits only feedback commands"),
        }
    }

    pub(crate) fn scan_now(&self) -> Result<(), DiscoveryError> {
        match self.command_tx.try_send(DiscoveryCommand::ScanNow) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(DiscoveryError::ChannelFull { channel: "command" }),
            Err(TrySendError::Disconnected(_)) => {
                Err(DiscoveryError::ChannelDisconnected { channel: "command" })
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<DiscoveryMessage, RecvTimeoutError> {
        self.message_rx.recv_timeout(timeout)
    }

    pub(crate) fn try_recv(&self) -> Result<DiscoveryMessage, std::sync::mpsc::TryRecvError> {
        self.message_rx.try_recv()
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(DiscoveryCommand::Shutdown);
    }

    pub(crate) fn into_join_handle(mut self) -> JoinHandle<()> {
        self.join
            .take()
            .expect("discovery handle owns exactly one thread")
    }
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

/// Spawns one fixed blocking thread so filesystem stalls cannot consume the
/// async runtime's shared blocking pool.
#[cfg(test)]
pub(crate) fn spawn_discovery(plan: DiscoveryPlan) -> Result<DiscoveryHandle, DiscoveryError> {
    spawn_discovery_with_shutdown_signal(plan, Arc::new(AtomicBool::new(false)))
}

/// Spawns discovery with a cancellation signal shared by its owning worker.
#[cfg(test)]
pub(crate) fn spawn_discovery_with_shutdown_signal(
    plan: DiscoveryPlan,
    shutdown_requested: Arc<AtomicBool>,
) -> Result<DiscoveryHandle, DiscoveryError> {
    spawn_discovery_with_shutdown_signal_and_pressure(
        plan,
        shutdown_requested,
        Arc::new(DescriptorPressure::default()),
    )
}

/// Spawns discovery with cancellation and receiver-global descriptor
/// pressure shared with the source reader.
pub(crate) fn spawn_discovery_with_shutdown_signal_and_pressure(
    plan: DiscoveryPlan,
    shutdown_requested: Arc<AtomicBool>,
    descriptor_pressure: Arc<DescriptorPressure>,
) -> Result<DiscoveryHandle, DiscoveryError> {
    let admission = AdmissionController::new(
        plan.max_pending_candidates(),
        plan.max_tracked_files(),
        plan.max_candidate_events(),
        plan.fingerprint_bytes(),
    )?;
    // Both channels are bounded cross-thread handoffs. The discovery thread
    // owns all mutable scan/admission state, so no shared mutex is needed.
    let (command_tx, command_rx) = sync_channel(COMMAND_CHANNEL_CAPACITY);
    let (message_tx, message_rx) = sync_channel(EVENT_CHANNEL_CAPACITY);
    let thread_shutdown_requested = Arc::clone(&shutdown_requested);
    let join = std::thread::Builder::new()
        .name("otap-filelog-discovery".to_owned())
        .spawn(move || {
            discovery_loop(
                plan,
                admission,
                command_rx,
                message_tx,
                thread_shutdown_requested,
                descriptor_pressure,
            );
        })
        .map_err(|source| DiscoveryError::ThreadSpawn { source })?;
    Ok(DiscoveryHandle {
        command_tx,
        message_rx,
        shutdown_requested,
        join: Some(join),
    })
}

fn discovery_loop(
    plan: DiscoveryPlan,
    mut admission: AdmissionController,
    command_rx: Receiver<DiscoveryCommand>,
    message_tx: SyncSender<DiscoveryMessage>,
    shutdown_requested: Arc<AtomicBool>,
    descriptor_pressure: Arc<DescriptorPressure>,
) {
    let reconciliation_schedule = plan.reconciliation_schedule();
    let mut scanner = FilesystemScanner::with_shutdown_signal_and_pressure(
        plan,
        Arc::clone(&shutdown_requested),
        descriptor_pressure,
    );
    loop {
        let batch = match scanner.reconcile(&mut admission, SystemTime::now()) {
            Ok(batch) => batch,
            Err(DiscoveryError::ShutdownRequested) => break,
            Err(error) => {
                send_failure(error, &command_rx, &message_tx, &shutdown_requested);
                break;
            }
        };
        let completed_at = batch.completed_at;
        let scan_requested = match send_batch_interruptibly(
            batch,
            &mut admission,
            &command_rx,
            &message_tx,
            &shutdown_requested,
        ) {
            Ok(SendBatch::Sent { scan_requested }) => scan_requested,
            Ok(SendBatch::Shutdown) | Err(DiscoveryError::ChannelDisconnected { .. }) => break,
            Err(error) => {
                send_failure(error, &command_rx, &message_tx, &shutdown_requested);
                break;
            }
        };
        if scan_requested {
            continue;
        }

        let delay = match reconciliation_schedule.next_delay() {
            Ok(delay) => delay,
            Err(error) => {
                send_failure(error, &command_rx, &message_tx, &shutdown_requested);
                break;
            }
        };
        let deadline = match reconciliation_deadline(completed_at, delay) {
            Ok(deadline) => deadline,
            Err(error) => {
                send_failure(error, &command_rx, &message_tx, &shutdown_requested);
                break;
            }
        };
        loop {
            if shutdown_requested.load(Ordering::Acquire) {
                let _ = message_tx.try_send(DiscoveryMessage::Stopped);
                return;
            }
            let Some(wait) = next_reconciliation_wait(deadline, Instant::now()) else {
                break;
            };
            match command_rx.recv_timeout(wait) {
                Ok(DiscoveryCommand::Feedback(feedback)) => {
                    if let Err(error) = admission.apply_feedback(feedback) {
                        send_failure(error, &command_rx, &message_tx, &shutdown_requested);
                        let _ = message_tx.try_send(DiscoveryMessage::Stopped);
                        return;
                    }
                }
                Ok(DiscoveryCommand::ScanNow) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Ok(DiscoveryCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    let _ = message_tx.try_send(DiscoveryMessage::Stopped);
                    return;
                }
            }
        }
    }
    let _ = message_tx.try_send(DiscoveryMessage::Stopped);
}

fn reconciliation_deadline(now: Instant, delay: Duration) -> Result<Instant, DiscoveryError> {
    now.checked_add(delay)
        .ok_or(DiscoveryError::ScheduleOverflow {
            field: "discovery reconciliation deadline",
        })
}

fn next_reconciliation_wait(deadline: Instant, now: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(now)?;
    if remaining.is_zero() {
        None
    } else {
        Some(remaining.min(SHUTDOWN_SIGNAL_POLL))
    }
}

enum SendBatch {
    Sent { scan_requested: bool },
    Shutdown,
}

fn send_batch_interruptibly(
    batch: super::ReconciliationBatch,
    admission: &mut AdmissionController,
    command_rx: &Receiver<DiscoveryCommand>,
    message_tx: &SyncSender<DiscoveryMessage>,
    shutdown_requested: &AtomicBool,
) -> Result<SendBatch, DiscoveryError> {
    let mut message = DiscoveryMessage::Batch(Box::new(batch));
    let mut scan_requested = false;
    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            return Ok(SendBatch::Shutdown);
        }
        match message_tx.try_send(message) {
            Ok(()) => return Ok(SendBatch::Sent { scan_requested }),
            Err(TrySendError::Disconnected(_)) => {
                return Err(DiscoveryError::ChannelDisconnected { channel: "event" });
            }
            Err(TrySendError::Full(returned)) => {
                message = returned;
                match command_rx.recv_timeout(FULL_CHANNEL_COMMAND_POLL) {
                    Ok(DiscoveryCommand::Feedback(feedback)) => {
                        admission.apply_feedback(feedback)?;
                    }
                    Ok(DiscoveryCommand::ScanNow) => scan_requested = true,
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(DiscoveryCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                        return Ok(SendBatch::Shutdown);
                    }
                }
            }
        }
    }
}

fn send_failure(
    error: DiscoveryError,
    command_rx: &Receiver<DiscoveryCommand>,
    message_tx: &SyncSender<DiscoveryMessage>,
    shutdown_requested: &AtomicBool,
) {
    let mut message = DiscoveryMessage::Failed(error);
    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            return;
        }
        match message_tx.try_send(message) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(returned)) => {
                message = returned;
                match command_rx.recv_timeout(FULL_CHANNEL_COMMAND_POLL) {
                    Ok(DiscoveryCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                    Ok(DiscoveryCommand::Feedback(_) | DiscoveryCommand::ScanNow)
                    | Err(RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{TrySendError, sync_channel};
    use std::time::SystemTime;

    use super::*;

    fn empty_batch() -> super::super::ReconciliationBatch {
        let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
        let _ = admission.begin_scan(SystemTime::UNIX_EPOCH).unwrap();
        admission.finish_scan().unwrap()
    }

    /// Scenario: a forced scan command arrives while the single event slot
    /// still contains the preceding reconciliation batch.
    /// Guarantees: the full-channel send path latches the request and returns
    /// it to the outer loop instead of silently discarding the forced scan.
    #[test]
    fn scan_request_is_latched_while_event_channel_is_full() {
        let (message_tx, message_rx) = sync_channel(EVENT_CHANNEL_CAPACITY);
        message_tx
            .send(DiscoveryMessage::Batch(Box::new(empty_batch())))
            .unwrap();
        let (command_tx, command_rx) = sync_channel(1);
        command_tx.send(DiscoveryCommand::ScanNow).unwrap();
        let sender = std::thread::spawn(move || {
            let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
            let shutdown_requested = AtomicBool::new(false);
            send_batch_interruptibly(
                empty_batch(),
                &mut admission,
                &command_rx,
                &message_tx,
                &shutdown_requested,
            )
        });

        loop {
            match command_tx.try_send(DiscoveryCommand::ScanNow) {
                Ok(()) => break,
                Err(TrySendError::Full(_)) => std::thread::yield_now(),
                Err(TrySendError::Disconnected(_)) => {
                    panic!("discovery sender stopped before consuming scan request")
                }
            }
        }
        assert!(matches!(
            message_rx.recv().unwrap(),
            DiscoveryMessage::Batch(_)
        ));

        assert!(matches!(
            sender.join().unwrap().unwrap(),
            SendBatch::Sent {
                scan_requested: true
            }
        ));
    }

    /// Scenario: discovery reaches a terminal error while the event channel
    /// still contains the preceding reconciliation batch.
    /// Guarantees: the terminal failure waits interruptibly for bounded
    /// handoff capacity instead of being discarded by a single `try_send`.
    #[test]
    fn terminal_failure_survives_a_full_event_channel() {
        let (message_tx, message_rx) = sync_channel(EVENT_CHANNEL_CAPACITY);
        message_tx
            .send(DiscoveryMessage::Batch(Box::new(empty_batch())))
            .unwrap();
        let (command_tx, command_rx) = sync_channel(1);
        command_tx.send(DiscoveryCommand::ScanNow).unwrap();
        let sender = std::thread::spawn(move || {
            let shutdown_requested = AtomicBool::new(false);
            send_failure(
                DiscoveryError::CounterOverflow {
                    counter: "test terminal failure",
                },
                &command_rx,
                &message_tx,
                &shutdown_requested,
            );
        });

        loop {
            match command_tx.try_send(DiscoveryCommand::ScanNow) {
                Ok(()) => break,
                Err(TrySendError::Full(_)) => std::thread::yield_now(),
                Err(TrySendError::Disconnected(_)) => {
                    panic!("failure sender stopped before observing full event channel")
                }
            }
        }
        assert!(matches!(
            message_rx.recv().unwrap(),
            DiscoveryMessage::Batch(_)
        ));
        assert!(matches!(
            message_rx.recv().unwrap(),
            DiscoveryMessage::Failed(DiscoveryError::CounterOverflow {
                counter: "test terminal failure"
            })
        ));
        sender.join().unwrap();
    }

    /// Scenario: a reconciliation delay cannot be added to the current
    /// monotonic clock domain.
    /// Guarantees: scheduling fails with a typed terminal error instead of
    /// wrapping or entering a zero-delay rescan loop.
    #[test]
    fn unrepresentable_reconciliation_deadline_fails_closed() {
        assert!(matches!(
            reconciliation_deadline(Instant::now(), Duration::MAX),
            Err(DiscoveryError::ScheduleOverflow {
                field: "discovery reconciliation deadline"
            })
        ));
    }

    /// Scenario: batch handoff completes after the delay measured from the
    /// scan's own completion time has already elapsed.
    /// Guarantees: the next pass is immediately due instead of adding a
    /// second full interval after channel backpressure.
    #[test]
    fn reconciliation_delay_is_anchored_to_scan_completion() {
        let now = Instant::now();
        let completed_at = now.checked_sub(Duration::from_secs(10)).unwrap();
        let deadline = reconciliation_deadline(completed_at, Duration::from_secs(5)).unwrap();
        assert_eq!(next_reconciliation_wait(deadline, now), None);
    }
}
