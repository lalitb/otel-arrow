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
    /// Installs pending state before the current attempt's downstream send,
    /// for both the initial send and every worker resend. `PendingBatch`
    /// only advances to `AwaitingCompletion` once that send is accepted.
    pub(crate) const fn sending(key: BatchKey) -> Self {
        Self {
            key,
            state: PendingState::Sending,
            next_attempt: None,
            explicit_loss: false,
        }
    }

    /// Test-only convenience constructor that skips straight to
    /// `AwaitingCompletion`, as if a prior send had already been accepted.
    #[cfg(test)]
    const fn after_send(key: BatchKey) -> Self {
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

    /// Correlates and classifies one aggregate Nack under the bounded retry
    /// policy. Every valid current-attempt Nack consumes the retry budget
    /// uniformly: `permanent`, `cause`, and any free-form reason text are
    /// diagnostic only and never change whether this attempt retries.
    pub(crate) fn on_nack(
        &mut self,
        data: &CallData,
        permanent: bool,
        cause: NackCause,
        retry: &RetryConfig,
        on_nack: OnNack,
    ) -> DeliveryDecision {
        // Diagnostic only: retained for callers that log or export them, but
        // they never influence the retry/exhaustion decision below.
        let _ = (permanent, cause);
        let Some(key) = key_from_call_data(data) else {
            return DeliveryDecision::Ignored(CompletionIgnore::Malformed);
        };
        if key != self.key {
            return DeliveryDecision::Ignored(CompletionIgnore::Stale);
        }
        if self.state != PendingState::AwaitingCompletion {
            return DeliveryDecision::Ignored(CompletionIgnore::Duplicate);
        }
        self.retry_or_apply_policy(key, retry, on_nack)
    }

    /// Correlates one pre-publication `NoRoute` failure while the current
    /// attempt is sending. Uses the exact same bounded backoff/exhaustion/
    /// `on_nack` state machine as an aggregate Nack, without any synthetic
    /// Nack `CallData`. Returns `None` only if the caller's `key` does not
    /// match the batch currently being sent, which is an invariant violation
    /// the caller must treat as terminal.
    pub(crate) fn on_no_route(
        &mut self,
        key: BatchKey,
        retry: &RetryConfig,
        on_nack: OnNack,
    ) -> Option<DeliveryDecision> {
        if self.state != PendingState::Sending || self.key != key {
            return None;
        }
        Some(self.retry_or_apply_policy(key, retry, on_nack))
    }

    /// Shared bounded-retry decision for aggregate Nack and pre-publication
    /// `NoRoute`: schedule checked exponential backoff for the next attempt
    /// while the budget remains, otherwise apply the configured `on_nack`
    /// terminal policy.
    fn retry_or_apply_policy(
        &mut self,
        key: BatchKey,
        retry: &RetryConfig,
        on_nack: OnNack,
    ) -> DeliveryDecision {
        if key.attempt < retry.max_attempts
            && let Some(next_attempt) = key.attempt.checked_add(1)
        {
            let backoff = retry_backoff(retry, key.attempt);
            self.next_attempt = Some(next_attempt);
            self.state = PendingState::Backoff;
            return DeliveryDecision::Retry {
                current: key,
                next_attempt,
                backoff,
            };
        }
        self.apply_policy(key, on_nack, true)
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

    /// Scenario: aggregate Nacks arrive until the configured total-send
    /// budget is exhausted.
    /// Guarantees: each retry names the checked next attempt and exact delay,
    /// and exhaustion applies Fail instead of creating an extra send.
    #[test]
    fn nack_respects_total_attempt_budget() {
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

    /// Scenario: every `NackCause` and `permanent` bool is Nacked once while
    /// attempts remain, then again once the retry budget is exhausted, under
    /// both `on_nack` policies.
    /// Guarantees: `permanent`, `cause`, and free-form reason text never
    /// change the outcome. Every combination retries identically while
    /// `attempt < max_attempts`, and applies the identical exhaustion policy
    /// once the budget is spent -- exactly as for `NackCause::RouteFull` with
    /// `permanent: false`.
    #[test]
    fn every_nack_cause_and_permanent_flag_retries_uniformly() {
        let config = retry(2);
        let combinations = [
            (false, NackCause::Unspecified),
            (false, NackCause::RouteFull),
            (false, NackCause::RouteClosed),
            (false, NackCause::NodeShutdown),
            (true, NackCause::Unspecified),
            (true, NackCause::RouteFull),
            (true, NackCause::RouteClosed),
            (true, NackCause::NodeShutdown),
        ];
        for on_nack in [OnNack::Fail, OnNack::DropAndContinue] {
            for (permanent, cause) in combinations {
                let attempt_one = key(20, 1);
                let mut pending = PendingBatch::after_send(attempt_one);
                assert_eq!(
                    pending.on_nack(&call_data(attempt_one), permanent, cause, &config, on_nack),
                    DeliveryDecision::Retry {
                        current: attempt_one,
                        next_attempt: 2,
                        backoff: Duration::from_millis(100),
                    },
                    "permanent={permanent}, cause={cause:?}, on_nack={on_nack:?}"
                );
                assert_eq!(pending.retry_elapsed(), Some(key(20, 2)));
                assert!(pending.begin_send(key(20, 2)));
                assert!(pending.send_succeeded(key(20, 2)));

                let attempt_two = key(20, 2);
                let expected = match on_nack {
                    OnNack::Fail => DeliveryDecision::Fail {
                        key: attempt_two,
                        exhausted: true,
                    },
                    OnNack::DropAndContinue => DeliveryDecision::Commit {
                        key: attempt_two,
                        explicit_loss: true,
                        exhausted: true,
                    },
                };
                assert_eq!(
                    pending.on_nack(&call_data(attempt_two), permanent, cause, &config, on_nack),
                    expected,
                    "permanent={permanent}, cause={cause:?}, on_nack={on_nack:?}"
                );
                if on_nack == OnNack::DropAndContinue {
                    assert_eq!(pending.awaiting_commit(), Some(true));
                }
            }
        }
    }

    /// Scenario: `max_attempts` is exactly one, so the very first Nack is
    /// already at the retry budget for every `NackCause` and `permanent` bool.
    /// Guarantees: attempt one applies `on_nack` immediately -- no backoff is
    /// scheduled and no second attempt is requested.
    #[test]
    fn every_nack_cause_exhausts_immediately_at_max_attempts_one() {
        let config = retry(1);
        for (permanent, cause) in [
            (false, NackCause::Unspecified),
            (false, NackCause::RouteFull),
            (false, NackCause::RouteClosed),
            (false, NackCause::NodeShutdown),
            (true, NackCause::RouteFull),
        ] {
            let expected = key(21, 1);
            let mut fail = PendingBatch::after_send(expected);
            assert_eq!(
                fail.on_nack(
                    &call_data(expected),
                    permanent,
                    cause,
                    &config,
                    OnNack::Fail
                ),
                DeliveryDecision::Fail {
                    key: expected,
                    exhausted: true,
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
                    exhausted: true,
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

    /// Scenario: `PendingBatch::sending` models both the initial send and a
    /// worker resend before publication is accepted, and `on_no_route` is
    /// actionable only for that exact Sending batch.
    /// Guarantees: a `NoRoute` for a different key leaves the pending state
    /// untouched (`None`); the matching `NoRoute` consumes the current
    /// attempt exactly like a Nack, and a second `NoRoute` after the state
    /// already moved on is no longer actionable.
    #[test]
    fn no_route_is_actionable_only_for_the_exact_sending_attempt() {
        let config = retry(3);
        let mut pending = PendingBatch::sending(key(40, 1));

        // Wrong key: state remains untouched.
        assert_eq!(pending.on_no_route(key(41, 1), &config, OnNack::Fail), None);
        assert_eq!(
            pending.on_no_route(key(40, 1), &config, OnNack::Fail),
            Some(DeliveryDecision::Retry {
                current: key(40, 1),
                next_attempt: 2,
                backoff: Duration::from_millis(100),
            })
        );
        // The attempt already resolved to Backoff: a second NoRoute for the
        // stale Sending key is no longer actionable.
        assert_eq!(pending.on_no_route(key(40, 1), &config, OnNack::Fail), None);
    }

    /// Scenario: a `NoRoute` failure occurs on the very first (initial) send
    /// attempt, before any prior successful publication.
    /// Guarantees: `NoRoute` on attempt one retries with the exact same
    /// checked backoff as an aggregate Nack would, requires no synthetic Nack
    /// `CallData`, and retains the exact worker batch key across the retry.
    #[test]
    fn no_route_on_initial_attempt_schedules_retry_like_a_nack() {
        let config = retry(3);
        let mut pending = PendingBatch::sending(key(50, 1));
        assert_eq!(
            pending.on_no_route(key(50, 1), &config, OnNack::Fail),
            Some(DeliveryDecision::Retry {
                current: key(50, 1),
                next_attempt: 2,
                backoff: Duration::from_millis(100),
            })
        );
        assert_eq!(pending.retry_elapsed(), Some(key(50, 2)));
        assert!(pending.begin_send(key(50, 2)));
        // The resent attempt succeeds this time.
        assert!(pending.send_succeeded(key(50, 2)));
        assert_eq!(
            pending.on_ack(&call_data(key(50, 2))),
            DeliveryDecision::Commit {
                key: key(50, 2),
                explicit_loss: false,
                exhausted: false,
            }
        );
    }

    /// Scenario: `NoRoute` recurs on the worker resend attempt too, until the
    /// bounded budget is exhausted.
    /// Guarantees: each `NoRoute` schedules the identical checked backoff a
    /// Nack would, and exhaustion applies `on_nack` exactly as for aggregate
    /// Nack exhaustion, for both `fail` and `drop_and_continue`.
    #[test]
    fn no_route_exhaustion_applies_on_nack_fail_and_drop_and_continue() {
        let config = retry(2);

        let mut fail = PendingBatch::sending(key(60, 1));
        assert_eq!(
            fail.on_no_route(key(60, 1), &config, OnNack::Fail),
            Some(DeliveryDecision::Retry {
                current: key(60, 1),
                next_attempt: 2,
                backoff: Duration::from_millis(100),
            })
        );
        assert_eq!(fail.retry_elapsed(), Some(key(60, 2)));
        assert!(fail.begin_send(key(60, 2)));
        assert_eq!(
            fail.on_no_route(key(60, 2), &config, OnNack::Fail),
            Some(DeliveryDecision::Fail {
                key: key(60, 2),
                exhausted: true,
            })
        );

        let mut drop = PendingBatch::sending(key(61, 1));
        assert_eq!(
            drop.on_no_route(key(61, 1), &config, OnNack::DropAndContinue),
            Some(DeliveryDecision::Retry {
                current: key(61, 1),
                next_attempt: 2,
                backoff: Duration::from_millis(100),
            })
        );
        assert_eq!(drop.retry_elapsed(), Some(key(61, 2)));
        assert!(drop.begin_send(key(61, 2)));
        assert_eq!(
            drop.on_no_route(key(61, 2), &config, OnNack::DropAndContinue),
            Some(DeliveryDecision::Commit {
                key: key(61, 2),
                explicit_loss: true,
                exhausted: true,
            })
        );
        assert_eq!(drop.awaiting_commit(), Some(true));
    }

    /// Scenario: a real Ack or Nack for the current key arrives while the
    /// batch is Sending (before publication is accepted), both for the
    /// initial send and for a worker resend.
    /// Guarantees: no completion is actionable before `send_succeeded`; no
    /// publication failure or in-flight completion can fabricate an Ack or
    /// progress, and a stale prior-attempt Ack remains ignored once resent.
    #[test]
    fn sending_state_ignores_matching_completions_without_fabricating_progress() {
        let config = retry(3);

        // Initial send: no PendingBatch exists yet other than `sending`.
        let mut initial = PendingBatch::sending(key(70, 1));
        assert_eq!(
            initial.on_ack(&call_data(key(70, 1))),
            DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
        );
        assert_eq!(
            initial.on_nack(
                &call_data(key(70, 1)),
                false,
                NackCause::RouteFull,
                &config,
                OnNack::Fail,
            ),
            DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
        );
        assert!(initial.send_succeeded(key(70, 1)));
        assert_eq!(
            initial.on_ack(&call_data(key(70, 1))),
            DeliveryDecision::Commit {
                key: key(70, 1),
                explicit_loss: false,
                exhausted: false,
            }
        );

        // Resend: a stale Ack for the prior attempt is ignored once the
        // worker resend is Sending, and a matching one only Acks after the
        // resend is accepted.
        let mut resend = PendingBatch::after_send(key(71, 1));
        assert_eq!(
            resend.on_nack(
                &call_data(key(71, 1)),
                false,
                NackCause::RouteFull,
                &config,
                OnNack::Fail,
            ),
            DeliveryDecision::Retry {
                current: key(71, 1),
                next_attempt: 2,
                backoff: Duration::from_millis(100),
            }
        );
        assert_eq!(resend.retry_elapsed(), Some(key(71, 2)));
        assert!(resend.begin_send(key(71, 2)));
        assert_eq!(
            resend.on_ack(&call_data(key(71, 1))),
            DeliveryDecision::Ignored(CompletionIgnore::Stale)
        );
        assert_eq!(
            resend.on_ack(&call_data(key(71, 2))),
            DeliveryDecision::Ignored(CompletionIgnore::Duplicate)
        );
        assert!(resend.send_succeeded(key(71, 2)));
        assert_eq!(
            resend.on_ack(&call_data(key(71, 1))),
            DeliveryDecision::Ignored(CompletionIgnore::Stale)
        );
        assert_eq!(
            resend.on_ack(&call_data(key(71, 2))),
            DeliveryDecision::Commit {
                key: key(71, 2),
                explicit_loss: false,
                exhausted: false,
            }
        );
    }

    #[derive(Clone, Copy)]
    enum TraceCompletion {
        Ack,
        NonPermanentNack,
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
            TraceCompletion::NonPermanentNack | TraceCompletion::PermanentNack => match phase {
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
                TraceCompletion::NonPermanentNack => pending.on_nack(
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
    /// enumerated across Ack, Nack (with and without `permanent`), stale, and
    /// malformed inputs under both terminal policies.
    /// Guarantees: only the model's exact current completion can commit,
    /// retry, or fail; retries remain bounded identically regardless of
    /// `permanent`; and all other events are inert.
    #[test]
    fn bounded_completion_traces_match_delivery_model() {
        let events = [
            TraceCompletion::Ack,
            TraceCompletion::NonPermanentNack,
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
