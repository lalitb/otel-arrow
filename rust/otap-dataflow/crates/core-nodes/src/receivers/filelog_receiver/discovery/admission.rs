// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded pending admission, scan-generation tracking, and fair overflow
//! selection.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime};

use super::{
    CandidateEvent, DiscoveredCandidate, DiscoveryError, DiscoveryFeedback, DiscoveryIssue,
    DiscoveryStats, ReconciliationBatch, RevocationReason,
};
use crate::receivers::filelog_receiver::checkpoint::Locator;

use crate::receivers::filelog_receiver::identity::matcher::CandidateInventory;

#[derive(Debug)]
struct PendingEntry {
    candidate: DiscoveredCandidate,
    first_seen_generation: u64,
    first_seen_at: Instant,
    seen_generation: u64,
}

#[derive(Debug)]
struct TrackedEntry {
    signature: [u8; 32],
    seen_generation: u64,
    present: bool,
    durable: bool,
    revoked: bool,
    revocation_inflight: bool,
    inflight_candidate: Option<DiscoveredCandidate>,
    first_seen_generation: Option<u64>,
    first_seen_at: Option<Instant>,
}

#[derive(Debug)]
struct SelectedCandidate {
    priority: [u8; 32],
    locator_key: [u8; 25],
    candidate: DiscoveredCandidate,
}

impl PartialEq for SelectedCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.locator_key == other.locator_key
    }
}

impl Eq for SelectedCandidate {}

impl PartialOrd for SelectedCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SelectedCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| self.locator_key.cmp(&other.locator_key))
    }
}

/// Bounded admission state shared across periodic reconciliation passes.
#[derive(Debug)]
pub(crate) struct AdmissionController {
    max_pending_candidates: usize,
    max_live_entries: usize,
    max_candidate_events: usize,
    max_denied_locators: usize,
    fingerprint_bytes: u16,
    inflight_count: usize,
    generation: u64,
    tracked: HashMap<Locator, TrackedEntry>,
    pending: HashMap<Locator, PendingEntry>,
    pending_order: VecDeque<Locator>,
    selected: BinaryHeap<SelectedCandidate>,
    selected_locators: HashSet<Locator>,
    denied_locators: HashSet<Locator>,
    denial_overflowed: bool,
    events: Vec<CandidateEvent>,
    stats: Option<DiscoveryStats>,
    scan_now: Option<SystemTime>,
    scan_started: Option<Instant>,
    removal_evidence_complete: bool,
    deferred_overflow: u64,
    overflow_since: Option<Instant>,
}

impl AdmissionController {
    pub(crate) fn new(
        max_pending_candidates: usize,
        max_tracked_files: usize,
        max_candidate_events: usize,
        fingerprint_bytes: u16,
    ) -> Result<Self, DiscoveryError> {
        let max_live_entries = max_tracked_files.checked_add(max_candidate_events).ok_or(
            DiscoveryError::CounterOverflow {
                counter: "tracked plus in-flight discovery entries",
            },
        )?;
        let max_denied_locators = max_live_entries
            .checked_add(max_pending_candidates)
            .and_then(|bound| bound.checked_add(max_candidate_events))
            .ok_or(DiscoveryError::CounterOverflow {
                counter: "per-scan denied discovery locators",
            })?;
        Ok(Self {
            max_pending_candidates,
            max_live_entries,
            max_candidate_events,
            max_denied_locators,
            fingerprint_bytes,
            inflight_count: 0,
            generation: 0,
            tracked: HashMap::with_capacity(max_tracked_files),
            pending: HashMap::with_capacity(max_pending_candidates),
            pending_order: VecDeque::with_capacity(max_pending_candidates),
            selected: BinaryHeap::new(),
            selected_locators: HashSet::new(),
            denied_locators: HashSet::with_capacity(max_denied_locators),
            denial_overflowed: false,
            events: Vec::new(),
            stats: None,
            scan_now: None,
            scan_started: None,
            removal_evidence_complete: false,
            deferred_overflow: 0,
            overflow_since: None,
        })
    }

    pub(crate) fn begin_scan(&mut self, now: SystemTime) -> Result<u64, DiscoveryError> {
        self.begin_scan_at(now, Instant::now())
    }

    pub(crate) fn begin_scan_at(
        &mut self,
        now: SystemTime,
        started: Instant,
    ) -> Result<u64, DiscoveryError> {
        if self.stats.is_some() {
            return Err(DiscoveryError::GenerationOutOfOrder {
                expected: self.generation,
                found: self.generation,
            });
        }
        self.generation =
            self.generation
                .checked_add(1)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "reconciliation generation",
                })?;
        self.events.clear();
        self.selected.clear();
        self.selected_locators.clear();
        self.denied_locators.clear();
        self.denial_overflowed = false;
        let mut stats = DiscoveryStats::new(self.generation);
        stats.overflowed_candidates = std::mem::take(&mut self.deferred_overflow);
        if stats.overflowed_candidates != 0 {
            stats.complete = false;
        }
        self.stats = Some(stats);
        self.scan_now = Some(now);
        self.scan_started = Some(started);
        self.removal_evidence_complete = true;
        Ok(self.generation)
    }

    pub(crate) fn increment_matched_paths(&mut self) -> Result<(), DiscoveryError> {
        let stats = self.current_stats_mut()?;
        stats.matched_paths =
            stats
                .matched_paths
                .checked_add(1)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "matched discovery paths",
                })?;
        Ok(())
    }

    pub(crate) fn record_issue(&mut self, issue: DiscoveryIssue) -> Result<(), DiscoveryError> {
        self.removal_evidence_complete = false;
        self.current_stats_mut()?.record_issue(issue)
    }

    pub(crate) fn record_denial_issue(
        &mut self,
        issue: DiscoveryIssue,
    ) -> Result<(), DiscoveryError> {
        self.denial_overflowed = true;
        self.record_issue(issue)
    }

    pub(crate) fn observe(
        &mut self,
        generation: u64,
        candidate: DiscoveredCandidate,
        ignore_older_than: Duration,
    ) -> Result<(), DiscoveryError> {
        if generation != self.generation || self.stats.is_none() {
            return Err(DiscoveryError::GenerationOutOfOrder {
                expected: self.generation,
                found: generation,
            });
        }
        let locator = candidate.evidence.locator;
        if self.denied_locators.contains(&locator) {
            return Ok(());
        }
        if self.tracked.contains_key(&locator) {
            let mut emit_update = false;
            let mut evidence_blocked = false;
            {
                let entry = self
                    .tracked
                    .get_mut(&locator)
                    .expect("contains_key established tracked entry");
                if entry.seen_generation == generation {
                    return Ok(());
                }
                entry.seen_generation = generation;
                let was_present = entry.present;
                let signature = candidate_signature(&candidate);
                if entry.revoked {
                    if entry.revocation_inflight
                        || entry.inflight_candidate.is_some()
                        || self.inflight_count >= self.max_candidate_events
                    {
                        evidence_blocked = true;
                    } else {
                        entry.signature = signature;
                        entry.inflight_candidate = Some(candidate.clone());
                        entry.present = true;
                        entry.revoked = false;
                        emit_update = true;
                    }
                } else if !was_present {
                    evidence_blocked = true;
                } else if entry.signature != signature {
                    if entry.inflight_candidate.is_some()
                        || self.inflight_count >= self.max_candidate_events
                    {
                        evidence_blocked = true;
                    } else {
                        entry.signature = signature;
                        entry.inflight_candidate = Some(candidate.clone());
                        entry.present = true;
                        emit_update = true;
                    }
                } else {
                    entry.present = true;
                }
            }
            if emit_update {
                self.inflight_count += 1;
                self.events.push(CandidateEvent::Updated(candidate));
            } else if evidence_blocked {
                self.mark_incomplete_overflow()?;
            }
            self.increment_eligible_candidates()?;
            return Ok(());
        }
        if self.denial_overflowed {
            return Ok(());
        }
        if let Some(entry) = self.pending.get_mut(&locator) {
            if entry.seen_generation != generation {
                entry.candidate = candidate;
                entry.seen_generation = generation;
                self.increment_eligible_candidates()?;
            }
            return Ok(());
        }
        if candidate_is_too_old(
            candidate.modified,
            self.scan_now.expect("active scan records its start time"),
            ignore_older_than,
        ) {
            return Ok(());
        }
        self.increment_eligible_candidates()?;
        self.consider_new_candidate(candidate)
    }

    /// Records positive path-policy evidence. Unknown excluded locators use
    /// only the bounded, per-scan denial set and never become retained
    /// candidates or tracked entries.
    pub(crate) fn observe_revoked(
        &mut self,
        generation: u64,
        locator: Locator,
        reason: RevocationReason,
    ) -> Result<(), DiscoveryError> {
        if generation != self.generation || self.stats.is_none() {
            return Err(DiscoveryError::GenerationOutOfOrder {
                expected: self.generation,
                found: generation,
            });
        }
        if !self.denied_locators.contains(&locator) && !self.denial_overflowed {
            if self.denied_locators.len() >= self.max_denied_locators {
                self.denial_overflowed = true;
                self.mark_incomplete_overflow()?;
            } else {
                let inserted = self.denied_locators.insert(locator);
                debug_assert!(inserted);
            }
        }
        let Some(entry) = self.tracked.get(&locator) else {
            let _pending = self.pending.remove(&locator);
            self.pending_order
                .retain(|pending_locator| *pending_locator != locator);
            return Ok(());
        };
        if entry.revoked {
            self.tracked
                .get_mut(&locator)
                .expect("tracked revocation entry disappeared")
                .seen_generation = generation;
            return Ok(());
        }

        let current_candidate_event = self.events.iter().position(|event| {
            event
                .candidate()
                .is_some_and(|candidate| candidate.evidence.locator == locator)
        });
        if let Some(index) = current_candidate_event {
            let _cancelled = self.events.remove(index);
            let entry = self
                .tracked
                .get_mut(&locator)
                .expect("tracked revocation entry disappeared");
            if entry.inflight_candidate.take().is_none() {
                return Err(DiscoveryError::InvalidFeedback {
                    locator,
                    reason: "current candidate event has no retained in-flight evidence",
                });
            }
            self.inflight_count =
                self.inflight_count
                    .checked_sub(1)
                    .ok_or(DiscoveryError::CounterOverflow {
                        counter: "in-flight candidate evidence",
                    })?;
        }

        let entry = self
            .tracked
            .get_mut(&locator)
            .expect("tracked revocation entry disappeared");
        entry.seen_generation = generation;
        entry.present = false;
        entry.revoked = true;
        if !entry.revocation_inflight {
            entry.revocation_inflight = true;
            self.events
                .push(CandidateEvent::Revoked { locator, reason });
        }
        Ok(())
    }

    pub(crate) fn finish_scan(&mut self) -> Result<ReconciliationBatch, DiscoveryError> {
        self.finish_scan_with_clock(&mut Instant::now)
    }

    /// Finishes reconciliation using separate decision and completion clocks.
    pub(super) fn finish_scan_with_clock(
        &mut self,
        clock: &mut impl FnMut() -> Instant,
    ) -> Result<ReconciliationBatch, DiscoveryError> {
        let started_at = self
            .scan_started
            .ok_or(DiscoveryError::GenerationOutOfOrder {
                expected: self.generation,
                found: self.generation,
            })?;
        let mut batch = self.finish_scan_at(clock())?;
        let completed_at = clock();
        batch.stats.scan_duration = completed_at.saturating_duration_since(started_at);
        batch.stats.oldest_pending_age = self
            .pending
            .values()
            .map(|entry| completed_at.saturating_duration_since(entry.first_seen_at))
            .max()
            .unwrap_or(Duration::ZERO);
        if batch.stats.overflowed_candidates != 0 {
            let since = self
                .overflow_since
                .expect("an overflowing scan records its persistence epoch");
            batch.stats.overflow_persistence = completed_at.saturating_duration_since(since);
        }
        Ok(batch)
    }

    pub(crate) fn finish_scan_at(
        &mut self,
        finished_at: Instant,
    ) -> Result<ReconciliationBatch, DiscoveryError> {
        let generation = self.generation;
        if self.stats.is_none() {
            return Err(DiscoveryError::GenerationOutOfOrder {
                expected: generation,
                found: generation,
            });
        }

        if self.removal_evidence_complete {
            self.pending
                .retain(|_, entry| entry.seen_generation == generation);
            self.pending_order
                .retain(|locator| self.pending.contains_key(locator));
        }

        if self.denial_overflowed {
            self.selected.clear();
            self.selected_locators.clear();
        } else {
            self.admit_pending(finished_at)?;
            self.retain_selected(finished_at)?;
        }

        if self.removal_evidence_complete {
            for (locator, entry) in &mut self.tracked {
                if entry.present && entry.seen_generation != generation {
                    entry.present = false;
                    self.events
                        .push(CandidateEvent::Removed { locator: *locator });
                }
            }
        }

        let mut inventory_candidates =
            Vec::with_capacity(self.pending.len().checked_add(self.inflight_count).ok_or(
                DiscoveryError::CounterOverflow {
                    counter: "candidate inventory population",
                },
            )?);
        inventory_candidates.extend(
            self.pending
                .values()
                .map(|entry| entry.candidate.evidence.clone()),
        );
        inventory_candidates.extend(self.tracked.values().filter_map(|entry| {
            entry
                .inflight_candidate
                .as_ref()
                .map(|candidate| candidate.evidence.clone())
        }));
        let live_locators: HashSet<Locator> = self.tracked.keys().copied().collect();

        let mut stats = self
            .stats
            .take()
            .expect("active scan always owns statistics");
        stats.pending_candidates = self.pending.len();
        stats.emitted_events = self.events.len();
        let started_at = self
            .scan_started
            .expect("active scan always has a monotonic clock");
        stats.scan_duration = finished_at.saturating_duration_since(started_at);
        stats.oldest_pending_age = self
            .pending
            .values()
            .map(|entry| finished_at.saturating_duration_since(entry.first_seen_at))
            .max()
            .unwrap_or(Duration::ZERO);
        if stats.overflowed_candidates != 0 {
            let since = *self.overflow_since.get_or_insert(started_at);
            stats.overflow_persistence = finished_at.saturating_duration_since(since);
        } else {
            self.overflow_since = None;
        }
        let inventory = if stats.complete {
            CandidateInventory::from_complete_reconciliation(
                &inventory_candidates,
                &live_locators,
                self.fingerprint_bytes,
            )
        } else {
            CandidateInventory::from_incomplete_reconciliation(
                &inventory_candidates,
                &live_locators,
                self.fingerprint_bytes,
            )
        };
        self.scan_now = None;
        self.scan_started = None;
        Ok(ReconciliationBatch {
            events: std::mem::take(&mut self.events),
            inventory,
            stats,
        })
    }

    pub(crate) fn apply_feedback(
        &mut self,
        feedback: DiscoveryFeedback,
    ) -> Result<(), DiscoveryError> {
        let mut named = HashSet::new();
        for (locator, requires_inflight, requires_absence, requires_revocation) in feedback
            .durable
            .iter()
            .map(|locator| (locator, true, false, false))
            .chain(
                feedback
                    .rejected
                    .iter()
                    .map(|locator| (locator, false, false, false)),
            )
            .chain(
                feedback
                    .deferred
                    .iter()
                    .map(|locator| (locator, true, false, false)),
            )
            .chain(
                feedback
                    .finalized
                    .iter()
                    .map(|locator| (locator, false, true, false)),
            )
            .chain(
                feedback
                    .revoked
                    .iter()
                    .map(|locator| (locator, false, false, true)),
            )
        {
            if !named.insert(*locator) {
                return Err(DiscoveryError::InvalidFeedback {
                    locator: *locator,
                    reason: "one feedback transaction names the locator more than once",
                });
            }
            let Some(entry) = self.tracked.get(locator) else {
                return Err(DiscoveryError::InvalidFeedback {
                    locator: *locator,
                    reason: "locator is not tracked by discovery",
                });
            };
            if requires_inflight && entry.inflight_candidate.is_none() {
                return Err(DiscoveryError::InvalidFeedback {
                    locator: *locator,
                    reason: "feedback has no in-flight candidate evidence",
                });
            }
            if requires_absence && entry.present {
                return Err(DiscoveryError::InvalidFeedback {
                    locator: *locator,
                    reason: "locator has not emitted a removal transition",
                });
            }
            if requires_revocation && (!entry.revoked || !entry.revocation_inflight) {
                return Err(DiscoveryError::InvalidFeedback {
                    locator: *locator,
                    reason: "locator has no in-flight revocation transition",
                });
            }
            if requires_absence && entry.revoked {
                return Err(DiscoveryError::InvalidFeedback {
                    locator: *locator,
                    reason: "revoked locator cannot be finalized as ordinary removal",
                });
            }
        }
        let deferred_to_pending = feedback
            .deferred
            .iter()
            .filter(|locator| self.tracked.get(locator).is_some_and(|entry| entry.present))
            .count();
        let deferred_overflow = deferred_to_pending.saturating_sub(
            self.max_pending_candidates
                .saturating_sub(self.pending.len()),
        );
        let deferred_overflow =
            u64::try_from(deferred_overflow).map_err(|_| DiscoveryError::BoundTooLarge {
                field: "deferred candidate overflow",
                value: u64::MAX,
            })?;
        let next_deferred_overflow = self
            .deferred_overflow
            .checked_add(deferred_overflow)
            .ok_or(DiscoveryError::CounterOverflow {
                counter: "deferred candidate overflow",
            })?;

        for locator in feedback.durable {
            let entry = self
                .tracked
                .get_mut(&locator)
                .expect("feedback was preflighted");
            entry.inflight_candidate = None;
            entry.first_seen_generation = None;
            entry.first_seen_at = None;
            entry.durable = true;
            self.inflight_count =
                self.inflight_count
                    .checked_sub(1)
                    .ok_or(DiscoveryError::CounterOverflow {
                        counter: "in-flight candidate evidence",
                    })?;
        }
        for locator in feedback.rejected {
            let mut entry = self
                .tracked
                .remove(&locator)
                .expect("feedback was preflighted");
            if entry.inflight_candidate.take().is_some() {
                self.inflight_count =
                    self.inflight_count
                        .checked_sub(1)
                        .ok_or(DiscoveryError::CounterOverflow {
                            counter: "in-flight candidate evidence",
                        })?;
            }
            if !entry.present {
                entry.first_seen_generation = None;
                entry.first_seen_at = None;
                let previous = self.tracked.insert(locator, entry);
                debug_assert!(previous.is_none());
            }
        }
        for locator in feedback.deferred {
            let mut entry = self
                .tracked
                .remove(&locator)
                .expect("feedback was preflighted");
            let candidate = entry
                .inflight_candidate
                .take()
                .expect("deferred feedback requires candidate evidence");
            self.inflight_count =
                self.inflight_count
                    .checked_sub(1)
                    .ok_or(DiscoveryError::CounterOverflow {
                        counter: "in-flight candidate evidence",
                    })?;
            if !entry.present {
                entry.first_seen_generation = None;
                entry.first_seen_at = None;
                let previous = self.tracked.insert(locator, entry);
                debug_assert!(previous.is_none());
            } else if self.pending.len() < self.max_pending_candidates {
                let first_seen_generation = entry.first_seen_generation.unwrap_or(self.generation);
                let first_seen_at = entry.first_seen_at.unwrap_or_else(Instant::now);
                let previous = self.pending.insert(
                    locator,
                    PendingEntry {
                        candidate,
                        first_seen_generation,
                        first_seen_at,
                        seen_generation: self.generation,
                    },
                );
                debug_assert!(previous.is_none());
            }
        }
        for locator in feedback.finalized {
            if self
                .tracked
                .remove(&locator)
                .is_some_and(|entry| entry.inflight_candidate.is_some())
            {
                self.inflight_count =
                    self.inflight_count
                        .checked_sub(1)
                        .ok_or(DiscoveryError::CounterOverflow {
                            counter: "in-flight candidate evidence",
                        })?;
            }
        }
        for locator in feedback.revoked {
            let durable = self
                .tracked
                .get(&locator)
                .expect("feedback was preflighted")
                .durable;
            if durable {
                self.tracked
                    .get_mut(&locator)
                    .expect("feedback was preflighted")
                    .revocation_inflight = false;
            } else {
                let entry = self
                    .tracked
                    .remove(&locator)
                    .expect("feedback was preflighted");
                if entry.inflight_candidate.is_some() {
                    self.inflight_count = self.inflight_count.checked_sub(1).ok_or(
                        DiscoveryError::CounterOverflow {
                            counter: "in-flight candidate evidence",
                        },
                    )?;
                }
            }
        }
        self.rebuild_pending_order();
        self.deferred_overflow = next_deferred_overflow;
        Ok(())
    }

    pub(crate) fn tracked_locators(&self) -> HashSet<Locator> {
        self.tracked.keys().copied().collect()
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn consider_new_candidate(
        &mut self,
        candidate: DiscoveredCandidate,
    ) -> Result<(), DiscoveryError> {
        let remaining_events = self
            .max_candidate_events
            .saturating_sub(self.inflight_count);
        let free_pending = self
            .max_pending_candidates
            .saturating_sub(self.pending.len());
        let selection_capacity =
            remaining_events
                .checked_add(free_pending)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "candidate selection capacity",
                })?;
        if selection_capacity == 0 {
            return self.mark_incomplete_overflow();
        }
        let locator = candidate.evidence.locator;
        if !self.selected_locators.insert(locator) {
            return Ok(());
        }
        let selected = SelectedCandidate {
            priority: candidate_priority(self.generation, locator),
            locator_key: locator_key(locator),
            candidate,
        };
        if self.selected.len() < selection_capacity {
            self.selected.push(selected);
            return Ok(());
        }
        let replace = self.selected.peek().is_some_and(|worst| selected < *worst);
        if replace {
            let displaced = self
                .selected
                .pop()
                .expect("non-empty full selection has a worst candidate");
            let _ = self
                .selected_locators
                .remove(&displaced.candidate.evidence.locator);
            self.selected.push(selected);
            self.mark_incomplete_overflow()
        } else {
            let _ = self.selected_locators.remove(&locator);
            self.mark_incomplete_overflow()
        }
    }

    fn admit_pending(&mut self, finished_at: Instant) -> Result<(), DiscoveryError> {
        let retained = self.pending_order.len();
        for _ in 0..retained {
            if self.inflight_count >= self.max_candidate_events {
                break;
            }
            let Some(locator) = self.pending_order.pop_front() else {
                break;
            };
            if self
                .pending
                .get(&locator)
                .is_some_and(|entry| entry.seen_generation != self.generation)
            {
                self.pending_order.push_back(locator);
                continue;
            }
            let Some(entry) = self.pending.remove(&locator) else {
                continue;
            };
            self.track_observed(
                entry.candidate,
                entry.first_seen_generation,
                entry.first_seen_at,
                finished_at,
            )?;
        }
        Ok(())
    }

    fn retain_selected(&mut self, finished_at: Instant) -> Result<(), DiscoveryError> {
        let selected = std::mem::take(&mut self.selected).into_sorted_vec();
        self.selected_locators.clear();
        for selected in selected {
            if self
                .denied_locators
                .contains(&selected.candidate.evidence.locator)
            {
                continue;
            }
            if self.inflight_count < self.max_candidate_events {
                let first_seen_at = self
                    .scan_started
                    .expect("active scan always has a monotonic clock");
                self.track_observed(
                    selected.candidate,
                    self.generation,
                    first_seen_at,
                    finished_at,
                )?;
            } else if self.pending.len() < self.max_pending_candidates {
                let locator = selected.candidate.evidence.locator;
                self.pending_order.push_back(locator);
                let previous = self.pending.insert(
                    locator,
                    PendingEntry {
                        candidate: selected.candidate,
                        first_seen_generation: self.generation,
                        first_seen_at: self
                            .scan_started
                            .expect("active scan always has a monotonic clock"),
                        seen_generation: self.generation,
                    },
                );
                debug_assert!(previous.is_none());
            } else {
                self.mark_incomplete_overflow()?;
            }
        }
        Ok(())
    }

    fn track_observed(
        &mut self,
        candidate: DiscoveredCandidate,
        first_seen_generation: u64,
        first_seen_at: Instant,
        admitted_at: Instant,
    ) -> Result<(), DiscoveryError> {
        if self.tracked.len() >= self.max_live_entries {
            return self.mark_incomplete_overflow();
        }
        let locator = candidate.evidence.locator;
        let admission_delay = admitted_at.saturating_duration_since(first_seen_at);
        {
            let stats = self.current_stats_mut()?;
            stats.admission_delay = stats.admission_delay.saturating_add(admission_delay);
            stats.admissions =
                stats
                    .admissions
                    .checked_add(1)
                    .ok_or(DiscoveryError::CounterOverflow {
                        counter: "candidate admissions",
                    })?;
        }
        let previous = self.tracked.insert(
            locator,
            TrackedEntry {
                signature: candidate_signature(&candidate),
                seen_generation: self.generation,
                present: true,
                durable: false,
                revoked: false,
                revocation_inflight: false,
                inflight_candidate: Some(candidate.clone()),
                first_seen_generation: Some(first_seen_generation),
                first_seen_at: Some(first_seen_at),
            },
        );
        debug_assert!(previous.is_none());
        self.inflight_count =
            self.inflight_count
                .checked_add(1)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "in-flight candidate evidence",
                })?;
        self.events.push(CandidateEvent::Observed(candidate));
        Ok(())
    }

    fn increment_eligible_candidates(&mut self) -> Result<(), DiscoveryError> {
        let stats = self.current_stats_mut()?;
        stats.eligible_candidates =
            stats
                .eligible_candidates
                .checked_add(1)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "eligible discovery candidates",
                })?;
        Ok(())
    }

    fn mark_incomplete_overflow(&mut self) -> Result<(), DiscoveryError> {
        let stats = self.current_stats_mut()?;
        stats.overflowed_candidates =
            stats
                .overflowed_candidates
                .checked_add(1)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "overflowed discovery candidates",
                })?;
        stats.complete = false;
        Ok(())
    }

    fn current_stats_mut(&mut self) -> Result<&mut DiscoveryStats, DiscoveryError> {
        self.stats
            .as_mut()
            .ok_or(DiscoveryError::GenerationOutOfOrder {
                expected: self.generation,
                found: self.generation,
            })
    }

    fn rebuild_pending_order(&mut self) {
        let mut ordered: Vec<_> = self
            .pending
            .iter()
            .map(|(locator, entry)| (entry.first_seen_generation, *locator))
            .collect();
        ordered.sort_unstable_by_key(|(generation, locator)| (*generation, locator_key(*locator)));
        self.pending_order = ordered.into_iter().map(|(_, locator)| locator).collect();
    }
}

fn candidate_is_too_old(
    modified: Option<SystemTime>,
    now: SystemTime,
    ignore_older_than: Duration,
) -> bool {
    !ignore_older_than.is_zero()
        && modified
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > ignore_older_than)
}

fn candidate_signature(candidate: &DiscoveredCandidate) -> [u8; 32] {
    let evidence = &candidate.evidence;
    let resolved_path = candidate.resolved_path.as_os_str().as_encoded_bytes();
    let mut hasher = blake3::Hasher::new();
    let _ = hasher.update(b"otel-arrow-filelog-discovery-signature-v2\0");
    let _ = hasher.update(&(evidence.fingerprint.len() as u64).to_be_bytes());
    let _ = hasher.update(&evidence.fingerprint);
    let _ = hasher.update(&(evidence.advisory_path.len() as u64).to_be_bytes());
    let _ = hasher.update(&evidence.advisory_path);
    let _ = hasher.update(&(resolved_path.len() as u64).to_be_bytes());
    let _ = hasher.update(resolved_path);
    *hasher.finalize().as_bytes()
}

fn candidate_priority(generation: u64, locator: Locator) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let _ = hasher.update(b"otel-arrow-filelog-discovery-fairness-v1\0");
    let _ = hasher.update(&generation.to_be_bytes());
    let _ = hasher.update(&locator_key(locator));
    *hasher.finalize().as_bytes()
}

fn locator_key(locator: Locator) -> [u8; 25] {
    let mut key = [0u8; 25];
    match locator {
        Locator::Unspecified => {}
        Locator::PosixDevIno { dev, ino } => {
            key[0] = 1;
            key[1..9].copy_from_slice(&dev.to_be_bytes());
            key[9..17].copy_from_slice(&ino.to_be_bytes());
        }
        Locator::WindowsVolumeFileId {
            volume_serial,
            file_id,
        } => {
            key[0] = 2;
            key[1..9].copy_from_slice(&volume_serial.to_be_bytes());
            key[9..25].copy_from_slice(&file_id);
        }
    }
    key
}
