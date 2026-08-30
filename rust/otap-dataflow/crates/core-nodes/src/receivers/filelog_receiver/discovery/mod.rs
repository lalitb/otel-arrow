// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Periodic, bounded filesystem discovery and candidate admission.
//!
//! Filesystem traversal runs on one dedicated OS thread and emits ordered
//! reconciliation batches. The read/checkpoint thread remains the authority
//! for durable identity and reports admission outcomes back through explicit
//! feedback.

pub(crate) mod admission;
pub(crate) mod scanner;
pub(crate) mod source;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;

use super::checkpoint::{AdvisoryPath, Locator};
use super::identity::matcher::CandidateInventory;
use super::identity::{CandidateEvidence, IdentityError};

/// One bounded candidate retained from a secure handle-based observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredCandidate {
    /// Path that matched an include pattern and is retained for diagnostics.
    pub(crate) matched_path: PathBuf,
    /// Canonical target path that passed resolved-target exclusions.
    pub(crate) resolved_path: PathBuf,
    /// Handle-derived identity evidence.
    pub(crate) evidence: CandidateEvidence,
    /// Modification time from the opened handle when the age filter is active.
    pub(crate) modified: Option<SystemTime>,
}

/// Positive policy evidence that makes a retained locator ineligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevocationReason {
    /// At least one observed path for the locator matched an operator exclude.
    ExcludedByPolicy,
}

/// Ordered candidate transition consumed by the read/checkpoint worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CandidateEvent {
    /// A locator became eligible for durable identity admission.
    Observed(DiscoveredCandidate),
    /// Matching evidence or advisory path changed for a live locator.
    Updated(DiscoveredCandidate),
    /// The locator disappeared or stopped matching. Its runtime lease remains
    /// live until the reader explicitly finalizes it.
    Removed {
        /// Runtime locator that is no longer eligible by path.
        locator: Locator,
    },
    /// The locator was positively observed through an ineligible path. Unlike
    /// removal, this transition stops late-write capture immediately.
    Revoked {
        /// Runtime locator that became ineligible.
        locator: Locator,
        /// Positive policy evidence requiring revocation.
        reason: RevocationReason,
    },
}

impl CandidateEvent {
    /// The retained candidate evidence, if this transition carries one.
    #[cfg(test)]
    pub(crate) fn candidate(&self) -> Option<&DiscoveredCandidate> {
        match self {
            CandidateEvent::Observed(candidate) | CandidateEvent::Updated(candidate) => {
                Some(candidate)
            }
            CandidateEvent::Removed { .. } | CandidateEvent::Revoked { .. } => None,
        }
    }
}

/// Bounded counters and completeness evidence for one reconciliation pass.
#[derive(Debug)]
pub(crate) struct DiscoveryStats {
    /// Monotonic scan generation.
    pub(crate) generation: u64,
    /// Paths matching at least one include before exclusions.
    pub(crate) matched_paths: u64,
    /// Eligible regular-file observations delivered to admission.
    pub(crate) eligible_candidates: u64,
    /// Candidates not retained because every bounded admission slot was full.
    pub(crate) overflowed_candidates: u64,
    /// Recoverable filesystem or identity observations that made the pass
    /// incomplete.
    pub(crate) scan_errors: u64,
    /// Retained pending-candidate depth after reconciliation.
    pub(crate) pending_candidates: usize,
    /// Candidate transitions in the emitted batch.
    pub(crate) emitted_events: usize,
    /// Wall-clock duration of the filesystem reconciliation pass.
    pub(crate) scan_duration: Duration,
    /// Age of the oldest retained pending candidate.
    pub(crate) oldest_pending_age: Duration,
    /// Sum of admission delays for candidates emitted by this pass.
    pub(crate) admission_delay: Duration,
    /// Candidates contributing to `admission_delay`.
    pub(crate) admissions: u64,
    /// Age of a continuously observed overflow condition.
    pub(crate) overflow_persistence: Duration,
    /// Whether the pass observed the complete eligible bounded population.
    pub(crate) complete: bool,
    /// First actionable scan issue; later issues are counted but not retained.
    pub(crate) first_issue: Option<DiscoveryIssue>,
}

impl DiscoveryStats {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            matched_paths: 0,
            eligible_candidates: 0,
            overflowed_candidates: 0,
            scan_errors: 0,
            pending_candidates: 0,
            emitted_events: 0,
            scan_duration: Duration::ZERO,
            oldest_pending_age: Duration::ZERO,
            admission_delay: Duration::ZERO,
            admissions: 0,
            overflow_persistence: Duration::ZERO,
            complete: true,
            first_issue: None,
        }
    }

    fn record_issue(&mut self, issue: DiscoveryIssue) -> Result<(), DiscoveryError> {
        self.scan_errors =
            self.scan_errors
                .checked_add(1)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "discovery scan errors",
                })?;
        self.complete = false;
        if self.first_issue.is_none() {
            self.first_issue = Some(issue);
        }
        Ok(())
    }
}

/// One ordered reconciliation result and the exact inventory used to resolve
/// its candidate transitions.
#[derive(Debug)]
pub(crate) struct ReconciliationBatch {
    /// Candidate transitions in scan order, with removals last.
    pub(crate) events: Vec<CandidateEvent>,
    /// Stable locators observed present during this pass, including sources
    /// excluded by policy or ignored by the initial-age filter.
    pub(crate) present_locators: HashSet<Locator>,
    /// Complete or explicitly incomplete identity-matching inventory.
    pub(crate) inventory: CandidateInventory,
    /// Bounded scan and admission evidence.
    pub(crate) stats: DiscoveryStats,
    /// Monotonic time when traversal for this pass began.
    pub(crate) started_at: Instant,
    /// Monotonic completion time for absence-retention evidence.
    pub(crate) completed_at: Instant,
    /// Locators this pass actually emitted a `CandidateEvent` for while
    /// recognized as a validated move/create replacement for a
    /// distinguished matched-path binding that rebounded away from its
    /// prior owner. A candidate recognized this generation but only
    /// deferred to the bounded pending queue (not yet emitted) is never
    /// named here; it reappears once a later pass actually emits it.
    /// Identity resolution bypasses `start_at` and mismatch-based anchor
    /// selection only for these locators (see
    /// `docs/filelog-receiver-phase1-spec.md`, "Discovery and matching").
    pub(crate) recognized_replacements: HashSet<Locator>,
}

/// A locator's registration or metadata update becoming durable, paired
/// with the exact `AdvisoryPath` now authoritative for its checkpoint
/// record. Discovery reconstructs its own distinguished-binding memory from
/// this value rather than the (possibly provisional) candidate path it
/// originally forwarded, so a restart's first traversal-order alias can
/// never silently replace an already-durable binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurableAck {
    /// Locator whose durable state advanced.
    pub(crate) locator: Locator,
    /// The advisory path now authoritative for this locator's checkpoint
    /// record.
    pub(crate) advisory_path: AdvisoryPath,
}

/// Discovery continuity cleanup for one runtime-vetted retention removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetentionRemovalAck {
    /// Locator whose durable Active association was removed.
    pub(crate) locator: Locator,
    /// Exact complete reconciliation generation that proved it absent.
    pub(crate) reconciliation_generation: u64,
}

/// Runtime release outcome applied to discovery continuity state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoveryRelease {
    /// Policy revocation stopped local reading but preserved durable state.
    Revoked(Locator),
    /// Runtime-vetted retention removed the durable Active association.
    RetentionRemoved(RetentionRemovalAck),
}

impl DiscoveryRelease {
    const fn locator(&self) -> Locator {
        match self {
            Self::Revoked(locator) => *locator,
            Self::RetentionRemoved(removal) => removal.locator,
        }
    }
}

/// Read-worker feedback applied before a later reconciliation pass.
#[derive(Debug, Default)]
pub(crate) struct DiscoveryFeedback {
    /// Locators whose registration or metadata update is now durable.
    pub(crate) durable: Vec<DurableAck>,
    /// Locators whose candidate transition failed and must be rediscovered.
    pub(crate) rejected: Vec<Locator>,
    /// New locators that could not consume durable tracked-file capacity and
    /// must return to the bounded pending queue.
    pub(crate) deferred: Vec<Locator>,
    /// Locators whose logical reader and runtime lease are fully finalized.
    pub(crate) finalized: Vec<Locator>,
    /// Policy-revoked or retention-removed locator release outcomes.
    pub(crate) released: Vec<DiscoveryRelease>,
}

/// Messages emitted by the dedicated discovery thread.
#[derive(Debug)]
pub(crate) enum DiscoveryMessage {
    /// One complete ordered reconciliation batch.
    Batch(Box<ReconciliationBatch>),
    /// A terminal source failure.
    Failed(DiscoveryError),
    /// The dedicated thread has stopped.
    Stopped,
}

/// Recoverable issue encountered while scanning one path.
#[derive(Debug, Error)]
pub(crate) enum DiscoveryIssue {
    /// Walking or resolving a filesystem path failed.
    #[error("could not {operation} at {path}: {source}")]
    Io {
        /// Stable operation description.
        operation: &'static str,
        /// Path being inspected.
        path: PathBuf,
        /// Underlying operating-system failure.
        #[source]
        source: std::io::Error,
    },
    /// WalkDir could not inspect one entry.
    #[error("could not traverse below {path}: {source}")]
    Walk {
        /// Root or entry nearest the failure.
        path: PathBuf,
        /// Structured traversal failure.
        #[source]
        source: walkdir::Error,
    },
    /// Handle-based identity collection rejected one candidate.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// A distinguished matched-path binding's prior owner rebound to more
    /// than one live locator within one pass, or two different prior
    /// owners both rebound to the same newly observed locator. Either
    /// makes the affected binding transitions unsafe to apply.
    #[error(
        "filelog discovery observed a conflicting or unstable prior path binding for locator {locator:?}"
    )]
    ConflictingPathRebind {
        /// The tracked locator whose prior distinguished binding could not
        /// be safely resolved this pass.
        locator: Locator,
    },
}

/// Terminal discovery setup, accounting, channel, or thread failure.
#[derive(Debug, Error)]
pub(crate) enum DiscoveryError {
    /// A configured or derived population cannot fit this target.
    #[error("filelog discovery bound {field} with value {value} does not fit this target")]
    BoundTooLarge {
        /// Configuration field or derived population.
        field: &'static str,
        /// Rejected value.
        value: u64,
    },
    /// A monotonic discovery counter exhausted its representation.
    #[error("filelog discovery counter '{counter}' overflowed")]
    CounterOverflow {
        /// Counter that could not advance.
        counter: &'static str,
    },
    /// Reader feedback did not correspond to a live discovery entry.
    #[error("invalid filelog discovery feedback for locator {locator:?}: {reason}")]
    InvalidFeedback {
        /// Locator named by the invalid feedback.
        locator: Locator,
        /// Violated transition invariant.
        reason: &'static str,
    },
    /// The bounded admission controller received events out of generation
    /// order.
    #[error("filelog discovery generation {found} is out of order; expected {expected}")]
    GenerationOutOfOrder {
        /// Expected next generation.
        expected: u64,
        /// Supplied generation.
        found: u64,
    },
    /// The dedicated discovery thread could not start.
    #[error("could not spawn filelog discovery thread: {source}")]
    ThreadSpawn {
        /// Thread creation error.
        #[source]
        source: std::io::Error,
    },
    /// A bounded command or event channel disconnected unexpectedly.
    #[error("filelog discovery {channel} channel disconnected")]
    ChannelDisconnected {
        /// Which side disconnected.
        channel: &'static str,
    },
    /// A nonblocking command handoff reached its fixed capacity.
    #[error("filelog discovery {channel} channel is full")]
    ChannelFull {
        /// Which side reached capacity.
        channel: &'static str,
    },
    /// Lifecycle cancellation interrupted an active reconciliation pass.
    #[error("filelog discovery shutdown was requested")]
    ShutdownRequested,
    /// The dedicated discovery thread panicked.
    #[error("filelog discovery thread panicked")]
    ThreadPanicked,
}

#[cfg(test)]
mod tests;
