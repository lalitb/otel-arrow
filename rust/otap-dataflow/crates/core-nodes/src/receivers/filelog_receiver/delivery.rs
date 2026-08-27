// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded delivery correlation and retry decisions.
//!
//! The async receiver owns only this compact state. Arrow records and
//! checkpoint deltas remain exclusively in the blocking worker's retained
//! logical batch.

use std::time::Duration;

use otap_df_engine::control::{CallData, Context8u8, NackCause};

use super::config::{OnNack, RetryConfig};

/// Exact completion key carried in the two filelog `CallData` slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BatchKey {
    pub(crate) batch_id: u64,
    pub(crate) attempt: u32,
}

impl BatchKey {
    pub(crate) const fn new(batch_id: u64, attempt: u32) -> Option<Self> {
        if batch_id == 0 || attempt == 0 {
            return None;
        }
        Some(Self { batch_id, attempt })
    }
}

/// Creates the exact two-slot completion context for one send attempt.
pub(crate) fn call_data(key: BatchKey) -> CallData {
    let mut data = CallData::new();
    data.push(Context8u8::from(key.batch_id));
    data.push(Context8u8::from(u64::from(key.attempt)));
    data
}

/// Decodes only the exact two-slot filelog completion shape.
pub(crate) fn key_from_call_data(data: &CallData) -> Option<BatchKey> {
    let [batch_id, attempt] = data.as_slice() else {
        return None;
    };
    let batch_id = u64::from(*batch_id);
    let attempt = u32::try_from(u64::from(*attempt)).ok()?;
    BatchKey::new(batch_id, attempt)
}

/// Compact async-only state for the sole receiver-wide pending batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingBatch {
    key: BatchKey,
    state: PendingState,
    next_attempt: Option<u32>,
    explicit_loss: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingState {
    AwaitingCompletion,
    Backoff,
    AwaitingWorkerResend,
    Sending,
    AwaitingCommit,
    Terminal,
}

impl PendingBatch {
    /// Installs pending state only after the first downstream send succeeds.
    pub(crate) const fn after_send(key: BatchKey) -> Self {
        Self {
            key,
            state: PendingState::AwaitingCompletion,
            next_attempt: None,
            explicit_loss: false,
        }
    }

    pub(crate) const fn key(&self) -> BatchKey {
        self.key
    }

    pub(crate) const fn awaiting_commit(&self) -> Option<bool> {
        match self.state {
            PendingState::AwaitingCommit => Some(self.explicit_loss),
            _ => None,
        }
    }

    /// Advances an elapsed retry timer to the worker-resend handoff.
    pub(crate) fn retry_elapsed(&mut self) -> Option<BatchKey> {
        if self.state != PendingState::Backoff {
            return None;
        }
        let next_attempt = self.next_attempt.take()?;
        self.key.attempt = next_attempt;
        self.state = PendingState::AwaitingWorkerResend;
        Some(self.key)
    }

    /// Validates the worker's resend event before a downstream send starts.
    pub(crate) fn begin_send(&mut self, key: BatchKey) -> bool {
        if self.state != PendingState::AwaitingWorkerResend || self.key != key {
            return false;
        }
        self.state = PendingState::Sending;
        true
    }

    /// Installs the fresh completion subscription only after send success.
    pub(crate) fn send_succeeded(&mut self, key: BatchKey) -> bool {
        if self.state != PendingState::Sending || self.key != key {
            return false;
        }
        self.state = PendingState::AwaitingCompletion;
        true
    }

    /// Correlates one Ack without accepting stale, duplicate, or malformed
    /// completion state.
    pub(crate) fn on_ack(&mut self, data: &CallData) -> DeliveryDecision {
        let Some(key) = key_from_call_data(data) else {
            return DeliveryDecision::Ignored(CompletionIgnore::Malformed);
        };
        if key != self.key {
            return DeliveryDecision::Ignored(CompletionIgnore::Stale);
        }
        if self.state != PendingState::AwaitingCompletion {
            return DeliveryDecision::Ignored(CompletionIgnore::Duplicate);
        }
        self.explicit_loss = false;
        self.state = PendingState::AwaitingCommit;
        DeliveryDecision::Commit {
            key,
            explicit_loss: false,
            exhausted: false,
        }
    }

    /// Correlates and classifies one Nack under the bounded retry policy.
    pub(crate) fn on_nack(
        &mut self,
        data: &CallData,
        permanent: bool,
        cause: NackCause,
        retry: &RetryConfig,
        on_nack: OnNack,
    ) -> DeliveryDecision {
        let Some(key) = key_from_call_data(data) else {
            return DeliveryDecision::Ignored(CompletionIgnore::Malformed);
        };
        if key != self.key {
            return DeliveryDecision::Ignored(CompletionIgnore::Stale);
        }
        if self.state != PendingState::AwaitingCompletion {
            return DeliveryDecision::Ignored(CompletionIgnore::Duplicate);
        }

        let retryable =
            !permanent && matches!(cause, NackCause::RouteFull | NackCause::Unspecified);
        if retryable && key.attempt < retry.max_attempts {
            let Some(next_attempt) = key.attempt.checked_add(1) else {
                return self.apply_policy(key, on_nack, true);
            };
            let backoff = retry_backoff(retry, key.attempt);
            self.next_attempt = Some(next_attempt);
            self.state = PendingState::Backoff;
            return DeliveryDecision::Retry {
                current: key,
                next_attempt,
                backoff,
            };
        }

        self.apply_policy(key, on_nack, retryable && key.attempt >= retry.max_attempts)
    }

    fn apply_policy(
        &mut self,
        key: BatchKey,
        on_nack: OnNack,
        exhausted: bool,
    ) -> DeliveryDecision {
        match on_nack {
            OnNack::Fail => {
                self.state = PendingState::Terminal;
                DeliveryDecision::Fail { key, exhausted }
            }
            OnNack::DropAndContinue => {
                self.explicit_loss = true;
                self.state = PendingState::AwaitingCommit;
                DeliveryDecision::Commit {
                    key,
                    explicit_loss: true,
                    exhausted,
                }
            }
        }
    }
}

/// Why a completion could not affect the retained batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionIgnore {
    Malformed,
    Stale,
    Duplicate,
}

/// One bounded state transition requested by completion correlation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryDecision {
    Ignored(CompletionIgnore),
    Commit {
        key: BatchKey,
        explicit_loss: bool,
        exhausted: bool,
    },
    Retry {
        current: BatchKey,
        next_attempt: u32,
        backoff: Duration,
    },
    Fail {
        key: BatchKey,
        exhausted: bool,
    },
}

/// Calculates the delay after `attempt` was Nacked.
///
/// Attempt one waits `initial_backoff`; each later attempt doubles with
/// checked arithmetic and clamps at `max_backoff`.
pub(crate) fn retry_backoff(retry: &RetryConfig, attempt: u32) -> Duration {
    debug_assert!(attempt > 0);
    let mut delay = retry.initial_backoff.min(retry.max_backoff);
    for _ in 1..attempt {
        delay = delay
            .checked_mul(2)
            .unwrap_or(retry.max_backoff)
            .min(retry.max_backoff);
        if delay == retry.max_backoff {
            break;
        }
    }
    delay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry(max_attempts: u32) -> RetryConfig {
        RetryConfig {
            max_attempts,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(450),
        }
    }

    fn key(batch_id: u64, attempt: u32) -> BatchKey {
        BatchKey::new(batch_id, attempt).unwrap()
    }

    /// Scenario: completion call data has exactly two valid nonzero slots.
    /// Guarantees: batch ID and attempt round-trip exactly, while missing,
    /// extra, zero, and oversized-attempt shapes fail decoding.
    #[test]
    fn call_data_requires_exact_batch_and_attempt_slots() {
        let expected = key(42, 7);
        assert_eq!(key_from_call_data(&call_data(expected)), Some(expected));
        assert_eq!(key_from_call_data(&CallData::new()), None);

        let mut one = CallData::new();
        one.push(Context8u8::from(42u64));
        assert_eq!(key_from_call_data(&one), None);

        let mut extra = call_data(expected);
        extra.push(Context8u8::from(9u64));
        assert_eq!(key_from_call_data(&extra), None);

        let mut zero = CallData::new();
        zero.push(Context8u8::from(0u64));
        zero.push(Context8u8::from(1u64));
        assert_eq!(key_from_call_data(&zero), None);

        let mut oversized = CallData::new();
        oversized.push(Context8u8::from(42u64));
        oversized.push(Context8u8::from(u64::from(u32::MAX) + 1));
        assert_eq!(key_from_call_data(&oversized), None);
    }

    /// Scenario: Ack correlation receives matching, duplicate, stale, and
    /// malformed completion contexts.
    /// Guarantees: only the first exact ID-and-attempt Ack requests a commit;
    /// every other completion leaves the decision unchanged and is classified.
    #[test]
    fn ack_correlation_accepts_only_one_exact_completion() {
        let expected = key(10, 1);
        let mut pending = PendingBatch::after_send(expected);
        assert_eq!(
            pending.on_ack(&call_data(key(9, 1))),
            DeliveryDecision::Ignored(CompletionIgnore::Stale)
        );
        assert_eq!(
            pending.on_ack(&CallData::new()),
            DeliveryDecision::Ignored(CompletionIgnore::Malformed)
        );
        assert_eq!(
            pending.on_ack(&call_data(expected)),
            DeliveryDecision::Commit {
                key: expected,
                explicit_loss: false,
                exhausted: false,
            }
        );
        assert_eq!(pending.awaiting_commit(), Some(false));
        assert_eq!(
            pending.on_ack(&call_data(expected)),
            DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
        );
    }

    /// Scenario: bounded exponential retry advances through attempts whose
    /// delays reach and then exceed the configured cap.
    /// Guarantees: the exact sequence is initial, doubled, doubled-and-clamped,
    /// then permanently clamped without overflow.
    #[test]
    fn retry_backoff_sequence_clamps_exactly() {
        let config = retry(8);
        assert_eq!(
            (1..=6)
                .map(|attempt| retry_backoff(&config, attempt))
                .collect::<Vec<_>>(),
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(450),
                Duration::from_millis(450),
                Duration::from_millis(450),
            ]
        );
        assert_eq!(retry_backoff(&config, u32::MAX), config.max_backoff);
    }

    /// Scenario: retryable Nacks arrive until the configured total-send
    /// budget is exhausted.
    /// Guarantees: each retry names the checked next attempt and exact delay,
    /// and exhaustion applies Fail instead of creating an extra send.
    #[test]
    fn retryable_nack_respects_total_attempt_budget() {
        let config = retry(3);
        let mut pending = PendingBatch::after_send(key(12, 1));
        assert_eq!(
            pending.on_nack(
                &call_data(key(12, 1)),
                false,
                NackCause::RouteFull,
                &config,
                OnNack::Fail,
            ),
            DeliveryDecision::Retry {
                current: key(12, 1),
                next_attempt: 2,
                backoff: Duration::from_millis(100),
            }
        );
        assert_eq!(pending.retry_elapsed(), Some(key(12, 2)));
        assert!(pending.begin_send(key(12, 2)));
        assert!(pending.send_succeeded(key(12, 2)));
        assert_eq!(
            pending.on_nack(
                &call_data(key(12, 2)),
                false,
                NackCause::Unspecified,
                &config,
                OnNack::Fail,
            ),
            DeliveryDecision::Retry {
                current: key(12, 2),
                next_attempt: 3,
                backoff: Duration::from_millis(200),
            }
        );
        assert_eq!(pending.retry_elapsed(), Some(key(12, 3)));
        assert!(pending.begin_send(key(12, 3)));
        assert!(pending.send_succeeded(key(12, 3)));
        assert_eq!(
            pending.on_nack(
                &call_data(key(12, 3)),
                false,
                NackCause::RouteFull,
                &config,
                OnNack::Fail,
            ),
            DeliveryDecision::Fail {
                key: key(12, 3),
                exhausted: true,
            }
        );
    }

    /// Scenario: permanent and route-terminal Nacks are evaluated under both
    /// configured terminal policies.
    /// Guarantees: Fail never requests progress, while DropAndContinue requests
    /// an explicit-loss commit immediately without entering retry backoff.
    #[test]
    fn permanent_nack_policy_never_retries() {
        let config = retry(8);
        for (permanent, cause) in [
            (true, NackCause::RouteFull),
            (false, NackCause::RouteClosed),
            (false, NackCause::NodeShutdown),
        ] {
            let expected = key(20, 1);
            let mut fail = PendingBatch::after_send(expected);
            assert_eq!(
                fail.on_nack(
                    &call_data(expected),
                    permanent,
                    cause,
                    &config,
                    OnNack::Fail,
                ),
                DeliveryDecision::Fail {
                    key: expected,
                    exhausted: false,
                }
            );

            let mut drop = PendingBatch::after_send(expected);
            assert_eq!(
                drop.on_nack(
                    &call_data(expected),
                    permanent,
                    cause,
                    &config,
                    OnNack::DropAndContinue,
                ),
                DeliveryDecision::Commit {
                    key: expected,
                    explicit_loss: true,
                    exhausted: false,
                }
            );
            assert_eq!(drop.awaiting_commit(), Some(true));
        }
    }

    /// Scenario: an old attempt completes while the current retained batch is
    /// in backoff, worker resend, or downstream-send state.
    /// Guarantees: stale attempts and duplicate current-attempt completions
    /// cannot advance the pending state before a fresh send succeeds.
    #[test]
    fn resend_transition_rejects_stale_and_premature_completions() {
        let config = retry(3);
        let mut pending = PendingBatch::after_send(key(30, 1));
        let _ = pending.on_nack(
            &call_data(key(30, 1)),
            false,
            NackCause::RouteFull,
            &config,
            OnNack::Fail,
        );
        assert_eq!(
            pending.on_ack(&call_data(key(30, 1))),
            DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
        );
        assert_eq!(pending.retry_elapsed(), Some(key(30, 2)));
        assert_eq!(
            pending.on_ack(&call_data(key(30, 1))),
            DeliveryDecision::Ignored(CompletionIgnore::Stale)
        );
        assert!(pending.begin_send(key(30, 2)));
        assert_eq!(
            pending.on_ack(&call_data(key(30, 2))),
            DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
        );
        assert!(pending.send_succeeded(key(30, 2)));
        assert_eq!(
            pending.on_ack(&call_data(key(30, 2))),
            DeliveryDecision::Commit {
                key: key(30, 2),
                explicit_loss: false,
                exhausted: false,
            }
        );
    }

    #[derive(Clone, Copy)]
    enum TraceCompletion {
        Ack,
        RetryableNack,
        PermanentNack,
        StaleAck,
        StaleNack,
        MalformedAck,
        MalformedNack,
    }

    #[derive(Clone, Copy)]
    enum ModelPhase {
        AwaitingCompletion,
        AwaitingCommit,
        Terminal,
    }

    fn expected_trace_decision(
        event: TraceCompletion,
        phase: ModelPhase,
        current: BatchKey,
        policy: OnNack,
    ) -> DeliveryDecision {
        match event {
            TraceCompletion::StaleAck | TraceCompletion::StaleNack => {
                DeliveryDecision::Ignored(CompletionIgnore::Stale)
            }
            TraceCompletion::MalformedAck | TraceCompletion::MalformedNack => {
                DeliveryDecision::Ignored(CompletionIgnore::Malformed)
            }
            TraceCompletion::Ack => match phase {
                ModelPhase::AwaitingCompletion => DeliveryDecision::Commit {
                    key: current,
                    explicit_loss: false,
                    exhausted: false,
                },
                ModelPhase::AwaitingCommit | ModelPhase::Terminal => {
                    DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
                }
            },
            TraceCompletion::RetryableNack => match phase {
                ModelPhase::AwaitingCompletion if current.attempt < 3 => DeliveryDecision::Retry {
                    current,
                    next_attempt: current.attempt + 1,
                    backoff: if current.attempt == 1 {
                        Duration::from_millis(10)
                    } else {
                        Duration::from_millis(20)
                    },
                },
                ModelPhase::AwaitingCompletion => match policy {
                    OnNack::Fail => DeliveryDecision::Fail {
                        key: current,
                        exhausted: true,
                    },
                    OnNack::DropAndContinue => DeliveryDecision::Commit {
                        key: current,
                        explicit_loss: true,
                        exhausted: true,
                    },
                },
                ModelPhase::AwaitingCommit | ModelPhase::Terminal => {
                    DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
                }
            },
            TraceCompletion::PermanentNack => match phase {
                ModelPhase::AwaitingCompletion => match policy {
                    OnNack::Fail => DeliveryDecision::Fail {
                        key: current,
                        exhausted: false,
                    },
                    OnNack::DropAndContinue => DeliveryDecision::Commit {
                        key: current,
                        explicit_loss: true,
                        exhausted: false,
                    },
                },
                ModelPhase::AwaitingCommit | ModelPhase::Terminal => {
                    DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
                }
            },
        }
    }

    fn exercise_completion_trace(
        encoded_trace: usize,
        trace_len: usize,
        policy: OnNack,
        events: &[TraceCompletion],
    ) {
        let retry = RetryConfig {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(25),
        };
        let mut pending = PendingBatch::after_send(key(77, 1));
        let mut phase = ModelPhase::AwaitingCompletion;
        let mut attempt = 1;
        let mut remaining = encoded_trace;

        for step in 0..trace_len {
            let event = events[remaining % events.len()];
            remaining /= events.len();
            let current = key(77, attempt);
            let expected = expected_trace_decision(event, phase, current, policy);
            let actual = match event {
                TraceCompletion::Ack => pending.on_ack(&call_data(current)),
                TraceCompletion::RetryableNack => pending.on_nack(
                    &call_data(current),
                    false,
                    NackCause::RouteFull,
                    &retry,
                    policy,
                ),
                TraceCompletion::PermanentNack => pending.on_nack(
                    &call_data(current),
                    true,
                    NackCause::RouteFull,
                    &retry,
                    policy,
                ),
                TraceCompletion::StaleAck => pending.on_ack(&call_data(key(76, attempt))),
                TraceCompletion::StaleNack => pending.on_nack(
                    &call_data(key(76, attempt)),
                    false,
                    NackCause::RouteFull,
                    &retry,
                    policy,
                ),
                TraceCompletion::MalformedAck => pending.on_ack(&CallData::new()),
                TraceCompletion::MalformedNack => pending.on_nack(
                    &CallData::new(),
                    false,
                    NackCause::RouteFull,
                    &retry,
                    policy,
                ),
            };
            assert_eq!(
                actual, expected,
                "trace={encoded_trace}, len={trace_len}, step={step}, policy={policy:?}"
            );

            match actual {
                DeliveryDecision::Retry { next_attempt, .. } => {
                    let next = key(77, next_attempt);
                    assert_eq!(pending.retry_elapsed(), Some(next));
                    assert!(pending.begin_send(next));
                    assert!(pending.send_succeeded(next));
                    attempt = next_attempt;
                    phase = ModelPhase::AwaitingCompletion;
                }
                DeliveryDecision::Commit { .. } => phase = ModelPhase::AwaitingCommit,
                DeliveryDecision::Fail { .. } => phase = ModelPhase::Terminal,
                DeliveryDecision::Ignored(_) => {}
            }
        }
    }

    /// Scenario: every completion trace of length zero through five is
    /// enumerated across Ack, retryable/permanent Nack, stale, and malformed
    /// inputs under both terminal policies.
    /// Guarantees: only the model's exact current completion can commit,
    /// retry, or fail; retries remain bounded and all other events are inert.
    #[test]
    fn bounded_completion_traces_match_delivery_model() {
        let events = [
            TraceCompletion::Ack,
            TraceCompletion::RetryableNack,
            TraceCompletion::PermanentNack,
            TraceCompletion::StaleAck,
            TraceCompletion::StaleNack,
            TraceCompletion::MalformedAck,
            TraceCompletion::MalformedNack,
        ];
        for policy in [OnNack::Fail, OnNack::DropAndContinue] {
            for trace_len in 0..=5 {
                for encoded_trace in 0..events.len().pow(trace_len as u32) {
                    exercise_completion_trace(encoded_trace, trace_len, policy, &events);
                }
            }
        }
    }
}
