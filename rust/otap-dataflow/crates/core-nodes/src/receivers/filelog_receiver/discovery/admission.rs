// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded pending admission, scan-generation tracking, and fair overflow
//! selection.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime};

use hashbrown::{HashMap as PathMap, HashSet as PathSet};

use super::{
    CandidateEvent, DiscoveredCandidate, DiscoveryError, DiscoveryFeedback, DiscoveryIssue,
    DiscoveryRelease, DiscoveryStats, DurableDiscoveryBinding, ReconciliationBatch,
    RevocationReason,
};
use crate::receivers::filelog_receiver::checkpoint::path::{
    AdvisoryPathKey, AdvisoryPathRef, distinguished_binding_order,
};
use crate::receivers::filelog_receiver::checkpoint::{AdvisoryPath, AdvisoryPathKind, Locator};

use crate::receivers::filelog_receiver::identity::matcher::CandidateInventory;

#[derive(Debug)]
struct PendingEntry {
    candidate: DiscoveredCandidate,
    first_seen_generation: u64,
    first_seen_at: Instant,
    seen_generation: u64,
    /// Whether `candidate` was recognized as a validated move/create
    /// replacement for a rebounding distinguished matched-path binding.
    /// Carried while the candidate waits in the bounded pending queue so
    /// admission capacity pressure emitting it in a later pass cannot
    /// silently lose the recognition (`docs/filelog-receiver-phase1-spec.md`,
    /// "Discovery and matching").
    recognized_replacement: bool,
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
    /// The single durable distinguished `AdvisoryPath` binding this locator
    /// currently owns. Seeded when the locator is first tracked and
    /// corrected from durable feedback (see [`DurableAck`]) so a restart's
    /// first traversal-order alias can never silently replace an
    /// already-durable binding (`docs/filelog-receiver-phase1-spec.md`,
    /// "Discovery and matching").
    distinguished_path: AdvisoryPath,
    /// Whether `distinguished_path` was itself observed, still naming this
    /// same locator, during the scan generation named by `seen_generation`.
    /// Scratch state, reset whenever a new generation's first alias for
    /// this locator is observed.
    binding_seen_this_generation: bool,
    /// The candidate observed at `distinguished_path` this generation, if
    /// `binding_seen_this_generation` is true. Used only to refresh
    /// non-path evidence; the binding itself does not change.
    binding_candidate: Option<DiscoveredCandidate>,
    /// The deterministic-minimum candidate observed this generation whose
    /// path differs from `distinguished_path`, retained only in case
    /// `distinguished_path` turns out not to be observed at all this
    /// generation. Bounded to one retained candidate; never an
    /// accumulated alias list.
    rebind_candidate: Option<DiscoveredCandidate>,
    /// Whether any alias for this locator was observed during the
    /// generation named by `seen_generation`. Scratch state.
    observed_this_generation: bool,
    /// `present` captured before this generation's first observation, used
    /// to detect an unsafe reappearance before an earlier removal
    /// transition finalized. Scratch state.
    was_present_before_generation: bool,
    /// Whether `inflight_candidate` (when present) was admitted as a
    /// validated move/create replacement recognition. Carried so
    /// identity-capacity feedback returning this candidate to the bounded
    /// pending queue (see `feedback.deferred`) does not silently lose the
    /// recognition (`docs/filelog-receiver-phase1-spec.md`, "Discovery and
    /// matching").
    recognized_replacement: bool,
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
    max_durable_bindings: usize,
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
    selected_deferred_age: HashSet<Locator>,
    /// Deterministic-minimum candidate retained per not-yet-tracked locator
    /// already selected this scan. The heap above only orders bounded fair
    /// overflow eviction by `(generation, locator)`; it never compares
    /// paths, so the reported candidate for one selected locator is kept
    /// here instead and swapped in when the heap is drained.
    selected_min_candidates: HashMap<Locator, DiscoveredCandidate>,
    denied_locators: HashSet<Locator>,
    /// Stable presence evidence retained only for this reconciliation. Its
    /// fixed bound is sufficient for every durable record; overflow makes
    /// the inventory incomplete rather than risking a false absence.
    present_locators: HashSet<Locator>,
    denial_overflowed: bool,
    events: Vec<CandidateEvent>,
    stats: Option<DiscoveryStats>,
    scan_now: Option<SystemTime>,
    scan_started: Option<Instant>,
    removal_evidence_complete: bool,
    deferred_overflow: u64,
    overflow_since: Option<Instant>,
    /// Bounded path -> locator index frozen from tracked distinguished
    /// bindings at scan start (`docs/filelog-receiver-phase1-spec.md`,
    /// "Discovery and matching").
    frozen_path_index: PathMap<AdvisoryPathKey, Locator>,
    /// Live durable locator/path bindings recovered before discovery starts.
    ///
    /// These remain authoritative even while no reader is attached, so an
    /// old exact-locator candidate or possible move/create replacement is
    /// classified before the new-unrelated-file age filter.
    durable_bindings: HashMap<Locator, AdvisoryPath>,
    /// Prior-binding owner -> newly observed claimant locator, recorded
    /// when an observation's path matches another locator's frozen
    /// distinguished binding.
    rebind_claims: HashMap<Locator, Locator>,
    /// Owners whose rebind evidence was unstable or conflicting this scan;
    /// their old binding is preserved and no rebind is recognized.
    rebind_conflicts: HashSet<Locator>,
    /// Bounded, per-scan candidate set freshly recognized by
    /// [`finalize_tracked_bindings`] as a validated move/create
    /// replacement. Consulted only while admitting or deferring a
    /// candidate this same pass; recognition that survives across passes
    /// is instead carried on the bounded [`PendingEntry`] and
    /// [`TrackedEntry`] the candidate itself occupies (see
    /// `docs/filelog-receiver-phase1-spec.md`, "Discovery and matching").
    recognized_replacement_candidates: HashSet<Locator>,
    /// Locators this pass actually emitted a `CandidateEvent` for while
    /// carrying a recognized-replacement flag. This is the exact set
    /// returned to the reader; a candidate merely deferred to the pending
    /// queue this pass is never named here.
    recognized_replacements: HashSet<Locator>,
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
            max_durable_bindings: max_tracked_files,
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
            selected_deferred_age: HashSet::new(),
            selected_min_candidates: HashMap::new(),
            denied_locators: HashSet::with_capacity(max_denied_locators),
            present_locators: HashSet::with_capacity(max_live_entries),
            denial_overflowed: false,
            events: Vec::new(),
            stats: None,
            scan_now: None,
            scan_started: None,
            removal_evidence_complete: false,
            deferred_overflow: 0,
            overflow_since: None,
            frozen_path_index: PathMap::with_capacity(max_tracked_files),
            durable_bindings: HashMap::with_capacity(max_tracked_files),
            rebind_claims: HashMap::with_capacity(max_tracked_files),
            rebind_conflicts: HashSet::with_capacity(max_tracked_files),
            recognized_replacement_candidates: HashSet::new(),
            recognized_replacements: HashSet::new(),
        })
    }

    /// Seeds the bounded live checkpoint bindings recovered before traversal.
    pub(crate) fn seed_durable_bindings(
        &mut self,
        bindings: Vec<DurableDiscoveryBinding>,
    ) -> Result<(), DiscoveryError> {
        if bindings.len() > self.max_durable_bindings {
            return Err(DiscoveryError::BoundTooLarge {
                field: "durable discovery bindings",
                value: u64::try_from(bindings.len()).unwrap_or(u64::MAX),
            });
        }
        self.durable_bindings
            .try_reserve(bindings.len())
            .map_err(|source| DiscoveryError::AllocationFailed {
                resource: "durable discovery binding index",
                source,
            })?;
        for binding in bindings {
            if binding.locator == Locator::Unspecified {
                return Err(DiscoveryError::InvalidDurableBinding {
                    locator: binding.locator,
                    reason: "durable discovery binding has no supported locator",
                });
            }
            if self
                .durable_bindings
                .insert(binding.locator, binding.advisory_path)
                .is_some()
            {
                return Err(DiscoveryError::InvalidDurableBinding {
                    locator: binding.locator,
                    reason: "durable discovery binding repeats a live locator",
                });
            }
        }
        Ok(())
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
        self.selected_deferred_age.clear();
        self.selected_min_candidates.clear();
        self.denied_locators.clear();
        self.present_locators.clear();
        self.denial_overflowed = false;
        self.rebind_claims.clear();
        self.rebind_conflicts.clear();
        self.recognized_replacement_candidates.clear();
        self.recognized_replacements.clear();
        self.frozen_path_index.clear();
        let mut ambiguous_paths = PathSet::with_capacity(self.max_durable_bindings);
        for (&locator, path) in &self.durable_bindings {
            freeze_path_binding(
                &mut self.frozen_path_index,
                &mut self.rebind_conflicts,
                &mut ambiguous_paths,
                locator,
                path,
            );
        }
        for (&locator, entry) in &self.tracked {
            if !entry.durable || self.durable_bindings.contains_key(&locator) {
                continue;
            }
            freeze_path_binding(
                &mut self.frozen_path_index,
                &mut self.rebind_conflicts,
                &mut ambiguous_paths,
                locator,
                &entry.distinguished_path,
            );
        }
        let mut stats = DiscoveryStats::new(self.generation);
        stats.overflowed_candidates = std::mem::take(&mut self.deferred_overflow);
        if stats.overflowed_candidates != 0 {
            stats.complete = false;
        }
        self.stats = Some(stats);
        self.scan_now = Some(now);
        self.scan_started = Some(started);
        self.removal_evidence_complete = true;
        for locator in self.rebind_conflicts.iter().copied().collect::<Vec<_>>() {
            self.record_issue(DiscoveryIssue::ConflictingPathRebind { locator })?;
        }
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
        if candidate.evidence.advisory_path.is_truncated() {
            let stats = self.current_stats_mut()?;
            stats.advisory_paths_truncated = stats.advisory_paths_truncated.checked_add(1).ok_or(
                DiscoveryError::CounterOverflow {
                    counter: "truncated advisory paths",
                },
            )?;
        }
        let locator = candidate.evidence.locator;
        self.record_presence(locator)?;
        if self.denied_locators.contains(&locator) {
            return Ok(());
        }
        // A distinguished binding's prior owner is frozen at scan start
        // (`begin_scan_at`); an observation naming a *different* live
        // locator at that same path is evidence the binding rebounded away
        // from its owner. This is detected uniformly here, before any
        // tracked/pending/new branching, so a genuinely new replacement
        // candidate is recognized exactly the same way as one that is
        // already tracked or pending.
        if let Some(&owner) = self
            .frozen_path_index
            .get(&AdvisoryPathRef::new(&candidate.evidence.advisory_path))
            && owner != locator
        {
            self.record_rebind_claim(owner, locator)?;
        }
        if self.tracked.contains_key(&locator) {
            let entry = self
                .tracked
                .get_mut(&locator)
                .expect("contains_key established tracked entry");
            if entry.seen_generation != generation {
                entry.seen_generation = generation;
                entry.binding_seen_this_generation = false;
                entry.binding_candidate = None;
                entry.rebind_candidate = None;
                entry.observed_this_generation = false;
                entry.was_present_before_generation = entry.present;
            }
            entry.observed_this_generation = true;
            if candidate.evidence.advisory_path == entry.distinguished_path {
                entry.binding_seen_this_generation = true;
                entry.binding_candidate = Some(candidate);
            } else {
                let replace = entry.rebind_candidate.as_ref().is_none_or(|existing| {
                    distinguished_binding_order(
                        &candidate.evidence.advisory_path,
                        &existing.evidence.advisory_path,
                    ) == Ordering::Less
                });
                if replace {
                    entry.rebind_candidate = Some(candidate);
                }
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
            } else if distinguished_binding_order(
                &candidate.evidence.advisory_path,
                &entry.candidate.evidence.advisory_path,
            ) == Ordering::Less
            {
                entry.candidate = candidate;
                self.increment_eligible_candidates()?;
            }
            return Ok(());
        }
        let possible_replacement = self
            .frozen_path_index
            .get(&AdvisoryPathRef::new(&candidate.evidence.advisory_path))
            .is_some_and(|owner| *owner != locator);
        let defer_age_filter = candidate_is_too_old(
            candidate.modified,
            self.scan_now.expect("active scan records its start time"),
            ignore_older_than,
        ) && !self.durable_bindings.contains_key(&locator);
        if defer_age_filter && !possible_replacement {
            return Ok(());
        }
        self.increment_eligible_candidates()?;
        self.consider_new_candidate(candidate, defer_age_filter)
    }

    /// Records that `owner`'s frozen distinguished path was observed this
    /// scan naming a different live locator, `claimant`. Two different
    /// claimants observed for the same owner within one pass is unstable
    /// evidence; the owner's rebind is then refused and its old binding is
    /// preserved (`docs/filelog-receiver-phase1-spec.md`, "Discovery and
    /// matching"). Once poisoned this way, `owner` is removed from
    /// `rebind_claims` (so neither claimant can later be recognized as its
    /// replacement) and every later claim for the same `owner` this
    /// generation is silently ignored.
    fn record_rebind_claim(
        &mut self,
        owner: Locator,
        claimant: Locator,
    ) -> Result<(), DiscoveryError> {
        if self.rebind_conflicts.contains(&owner) {
            return Ok(());
        }
        match self.rebind_claims.get(&owner) {
            Some(&existing) if existing != claimant => {
                let _ = self.rebind_claims.remove(&owner);
                let _ = self.rebind_conflicts.insert(owner);
                self.record_issue(DiscoveryIssue::ConflictingPathRebind { locator: owner })?;
            }
            _ => {
                let _ = self.rebind_claims.insert(owner, claimant);
            }
        }
        Ok(())
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
        self.record_presence(locator)?;
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
        batch.completed_at = completed_at;
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

        self.finalize_tracked_bindings()?;

        if self.denial_overflowed {
            self.selected.clear();
            self.selected_locators.clear();
            self.selected_min_candidates.clear();
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
            present_locators: std::mem::take(&mut self.present_locators),
            inventory,
            stats,
            started_at,
            completed_at: finished_at,
            recognized_replacements: std::mem::take(&mut self.recognized_replacements),
        })
    }

    pub(crate) fn apply_feedback(
        &mut self,
        feedback: DiscoveryFeedback,
    ) -> Result<(), DiscoveryError> {
        let mut named = HashSet::new();
        for (locator, requires_inflight, requires_absence) in feedback
            .durable
            .iter()
            .map(|ack| (&ack.locator, true, false))
            .chain(
                feedback
                    .rejected
                    .iter()
                    .map(|locator| (locator, false, false)),
            )
            .chain(
                feedback
                    .deferred
                    .iter()
                    .map(|locator| (locator, true, false)),
            )
            .chain(
                feedback
                    .finalized
                    .iter()
                    .map(|locator| (locator, false, true)),
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
            if requires_absence && entry.revoked {
                return Err(DiscoveryError::InvalidFeedback {
                    locator: *locator,
                    reason: "revoked locator cannot be finalized as ordinary removal",
                });
            }
        }
        for release in &feedback.released {
            let locator = release.locator();
            if !named.insert(locator) {
                return Err(DiscoveryError::InvalidFeedback {
                    locator,
                    reason: "one feedback transaction names the locator more than once",
                });
            }
            if matches!(release, DiscoveryRelease::Revoked(_))
                && !self
                    .tracked
                    .get(&locator)
                    .is_some_and(|entry| entry.revoked && entry.revocation_inflight)
            {
                return Err(DiscoveryError::InvalidFeedback {
                    locator,
                    reason: "locator has no in-flight revocation transition",
                });
            }
            if let DiscoveryRelease::RetentionRemoved(removal) = release
                && removal.reconciliation_generation > self.generation
            {
                return Err(DiscoveryError::InvalidFeedback {
                    locator,
                    reason: "retention removal names a future reconciliation generation",
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
        let new_durable_bindings = feedback
            .durable
            .iter()
            .filter(|ack| !self.durable_bindings.contains_key(&ack.locator))
            .count();
        if self
            .durable_bindings
            .len()
            .checked_add(new_durable_bindings)
            .is_none_or(|count| count > self.max_durable_bindings)
        {
            return Err(DiscoveryError::BoundTooLarge {
                field: "durable discovery bindings",
                value: u64::try_from(
                    self.durable_bindings
                        .len()
                        .saturating_add(new_durable_bindings),
                )
                .unwrap_or(u64::MAX),
            });
        }
        self.durable_bindings
            .try_reserve(new_durable_bindings)
            .map_err(|source| DiscoveryError::AllocationFailed {
                resource: "durable discovery binding index",
                source,
            })?;

        for ack in feedback.durable {
            let entry = self
                .tracked
                .get_mut(&ack.locator)
                .expect("feedback was preflighted");
            entry.inflight_candidate = None;
            entry.first_seen_generation = None;
            entry.first_seen_at = None;
            entry.durable = true;
            entry.distinguished_path = ack.advisory_path.clone();
            let _ = self.durable_bindings.insert(ack.locator, ack.advisory_path);
            // The recognized-replacement identity is now durable; it must
            // never be carried forward onto a later, unrelated in-flight
            // candidate for the same locator.
            entry.recognized_replacement = false;
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
            // The rejected candidate's recognition must not survive onto
            // whatever this locator is later reobserved as.
            entry.recognized_replacement = false;
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
                        // Identity capacity, not recognition, deferred
                        // this candidate: the recognized-replacement
                        // decision survives the round trip through the
                        // pending queue so a later pass emits it again as
                        // a replacement rather than an ordinary discovery.
                        recognized_replacement: entry.recognized_replacement,
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
        for release in feedback.released {
            match release {
                DiscoveryRelease::Revoked(locator) => {
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
                        // A non-durable revoked locator's tracked entry is
                        // removed entirely, including any replacement flag.
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
                DiscoveryRelease::RetentionRemoved(removal) => {
                    let locator = removal.locator;
                    let _ = self.durable_bindings.remove(&locator);
                    if self.pending.get(&locator).is_some_and(|entry| {
                        entry.seen_generation > removal.reconciliation_generation
                    }) || self.tracked.get(&locator).is_some_and(|entry| {
                        entry.seen_generation > removal.reconciliation_generation
                    }) {
                        continue;
                    }
                    let _ = self.pending.remove(&locator);
                    if self
                        .tracked
                        .remove(&locator)
                        .is_some_and(|entry| entry.inflight_candidate.is_some())
                    {
                        self.inflight_count = self.inflight_count.checked_sub(1).ok_or(
                            DiscoveryError::CounterOverflow {
                                counter: "in-flight candidate evidence",
                            },
                        )?;
                    }
                }
            }
        }
        self.rebuild_pending_order();
        self.deferred_overflow = next_deferred_overflow;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn tracked_locators(&self) -> HashSet<Locator> {
        self.tracked.keys().copied().collect()
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn consider_new_candidate(
        &mut self,
        candidate: DiscoveredCandidate,
        defer_age_filter: bool,
    ) -> Result<(), DiscoveryError> {
        let locator = candidate.evidence.locator;
        if self.selected_locators.contains(&locator) {
            if defer_age_filter {
                let _ = self.selected_deferred_age.insert(locator);
            }
            // Another alias for this not-yet-tracked locator was already
            // selected this scan. Retain only the deterministic-minimum
            // observation rather than every alias; the fair-overflow heap
            // orders purely by `(generation, locator)` and never inspects
            // the retained candidate, so replacing it here cannot disturb
            // eviction fairness.
            if let Some(existing) = self.selected_min_candidates.get(&locator)
                && distinguished_binding_order(
                    &candidate.evidence.advisory_path,
                    &existing.evidence.advisory_path,
                ) == Ordering::Less
            {
                let _ = self.selected_min_candidates.insert(locator, candidate);
            }
            return Ok(());
        }
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
        let _ = self.selected_locators.insert(locator);
        if defer_age_filter {
            let _ = self.selected_deferred_age.insert(locator);
        }
        let _ = self
            .selected_min_candidates
            .insert(locator, candidate.clone());
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
            let _ = self
                .selected_deferred_age
                .remove(&displaced.candidate.evidence.locator);
            let _ = self
                .selected_min_candidates
                .remove(&displaced.candidate.evidence.locator);
            self.selected.push(selected);
            self.mark_incomplete_overflow()
        } else {
            let _ = self.selected_locators.remove(&locator);
            let _ = self.selected_deferred_age.remove(&locator);
            let _ = self.selected_min_candidates.remove(&locator);
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
                entry.recognized_replacement,
            )?;
        }
        Ok(())
    }

    fn retain_selected(&mut self, finished_at: Instant) -> Result<(), DiscoveryError> {
        let selected = std::mem::take(&mut self.selected).into_sorted_vec();
        self.selected_locators.clear();
        for selected in selected {
            let locator = selected.candidate.evidence.locator;
            let candidate = self
                .selected_min_candidates
                .remove(&locator)
                .unwrap_or(selected.candidate);
            if self.denied_locators.contains(&locator) {
                continue;
            }
            let recognized_replacement = self.recognized_replacement_candidates.contains(&locator);
            let deferred_age_filter = self.selected_deferred_age.remove(&locator);
            if deferred_age_filter && !recognized_replacement {
                continue;
            }
            if self.inflight_count < self.max_candidate_events {
                let first_seen_at = self
                    .scan_started
                    .expect("active scan always has a monotonic clock");
                self.track_observed(
                    candidate,
                    self.generation,
                    first_seen_at,
                    finished_at,
                    recognized_replacement,
                )?;
            } else if self.pending.len() < self.max_pending_candidates {
                self.pending_order.push_back(locator);
                let previous = self.pending.insert(
                    locator,
                    PendingEntry {
                        candidate,
                        first_seen_generation: self.generation,
                        first_seen_at: self
                            .scan_started
                            .expect("active scan always has a monotonic clock"),
                        seen_generation: self.generation,
                        recognized_replacement,
                    },
                );
                debug_assert!(previous.is_none());
            } else {
                self.mark_incomplete_overflow()?;
            }
        }
        self.selected_deferred_age.clear();
        Ok(())
    }

    fn track_observed(
        &mut self,
        candidate: DiscoveredCandidate,
        first_seen_generation: u64,
        first_seen_at: Instant,
        admitted_at: Instant,
        recognized_replacement: bool,
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
                distinguished_path: candidate.evidence.advisory_path.clone(),
                binding_seen_this_generation: true,
                binding_candidate: None,
                rebind_candidate: None,
                observed_this_generation: true,
                was_present_before_generation: true,
                inflight_candidate: Some(candidate.clone()),
                first_seen_generation: Some(first_seen_generation),
                first_seen_at: Some(first_seen_at),
                recognized_replacement,
            },
        );
        debug_assert!(previous.is_none());
        if recognized_replacement {
            let _ = self.recognized_replacements.insert(locator);
        }
        self.inflight_count =
            self.inflight_count
                .checked_add(1)
                .ok_or(DiscoveryError::CounterOverflow {
                    counter: "in-flight candidate evidence",
                })?;
        self.events.push(CandidateEvent::Observed(candidate));
        Ok(())
    }

    /// Resolves every tracked locator's distinguished matched-path binding
    /// for the just-completed scan, in the order required by
    /// `docs/filelog-receiver-phase1-spec.md`, "Discovery and matching":
    /// prior bindings are frozen (`begin_scan_at`), rebind claims are
    /// grouped to detect conflicts, and only then does each affected
    /// locator's binding change.
    fn finalize_tracked_bindings(&mut self) -> Result<(), DiscoveryError> {
        let mut claimant_owners: HashMap<Locator, Vec<Locator>> = HashMap::new();
        for (&owner, &claimant) in &self.rebind_claims {
            claimant_owners.entry(claimant).or_default().push(owner);
        }
        let mut recognized_replacements = HashSet::new();
        for (&claimant, owners) in &claimant_owners {
            if owners.len() > 1 {
                for &owner in owners {
                    let _ = self.rebind_conflicts.insert(owner);
                }
            } else {
                let _ = recognized_replacements.insert(claimant);
            }
        }
        // A claimant already tracked (rather than still pending or
        // newly selected) picks up the flag directly on its own entry;
        // `admit_pending`/`retain_selected` handle every other case when
        // the candidate is actually admitted or deferred below.
        for &claimant in &recognized_replacements {
            if let Some(entry) = self.tracked.get_mut(&claimant) {
                entry.recognized_replacement = true;
            }
        }
        self.recognized_replacement_candidates = recognized_replacements;
        for issue_owner in claimant_owners
            .values()
            .filter(|owners| owners.len() > 1)
            .flatten()
            .copied()
            .collect::<Vec<_>>()
        {
            self.record_issue(DiscoveryIssue::ConflictingPathRebind {
                locator: issue_owner,
            })?;
        }
        let locators: Vec<Locator> = self.tracked.keys().copied().collect();
        for locator in locators {
            self.finalize_one_tracked_binding(locator)?;
        }
        Ok(())
    }

    fn finalize_one_tracked_binding(&mut self, locator: Locator) -> Result<(), DiscoveryError> {
        let generation = self.generation;
        let (candidate, path_changing, revoked, was_present_before, recognized_replacement) = {
            let entry = self
                .tracked
                .get(&locator)
                .expect("finalize iterates tracked keys");
            if entry.seen_generation != generation || !entry.observed_this_generation {
                return Ok(());
            }
            if entry.binding_seen_this_generation {
                (
                    entry.binding_candidate.clone(),
                    false,
                    entry.revoked,
                    entry.was_present_before_generation,
                    entry.recognized_replacement,
                )
            } else {
                (
                    entry.rebind_candidate.clone(),
                    true,
                    entry.revoked,
                    entry.was_present_before_generation,
                    entry.recognized_replacement,
                )
            }
        };
        let Some(candidate) = candidate else {
            return Ok(());
        };
        if path_changing
            && (!self.removal_evidence_complete || self.rebind_conflicts.contains(&locator))
        {
            // Fail closed: an unstable or conflicting rebind, or an
            // otherwise incomplete pass, must preserve the old durable
            // binding rather than reselect from partial evidence.
            return self.mark_incomplete_overflow();
        }
        let new_signature = candidate_signature(&candidate);
        if revoked {
            let entry = self
                .tracked
                .get(&locator)
                .expect("finalize iterates tracked keys");
            if entry.revocation_inflight
                || entry.inflight_candidate.is_some()
                || self.inflight_count >= self.max_candidate_events
            {
                return self.mark_incomplete_overflow();
            }
            let entry = self
                .tracked
                .get_mut(&locator)
                .expect("finalize iterates tracked keys");
            entry.signature = new_signature;
            entry.inflight_candidate = Some(candidate.clone());
            entry.present = true;
            entry.revoked = false;
            if path_changing {
                entry.distinguished_path = candidate.evidence.advisory_path.clone();
            }
            self.inflight_count += 1;
            self.events.push(CandidateEvent::Updated(candidate));
            if recognized_replacement {
                let _ = self.recognized_replacements.insert(locator);
            }
            return Ok(());
        }
        if !was_present_before {
            return self.mark_incomplete_overflow();
        }
        let entry = self
            .tracked
            .get(&locator)
            .expect("finalize iterates tracked keys");
        let evidence_changed = path_changing || entry.signature != new_signature;
        if !evidence_changed {
            let entry = self
                .tracked
                .get_mut(&locator)
                .expect("finalize iterates tracked keys");
            entry.present = true;
            return Ok(());
        }
        let entry = self
            .tracked
            .get(&locator)
            .expect("finalize iterates tracked keys");
        if entry.inflight_candidate.is_some() || self.inflight_count >= self.max_candidate_events {
            return self.mark_incomplete_overflow();
        }
        let entry = self
            .tracked
            .get_mut(&locator)
            .expect("finalize iterates tracked keys");
        entry.signature = new_signature;
        entry.inflight_candidate = Some(candidate.clone());
        entry.present = true;
        if path_changing {
            entry.distinguished_path = candidate.evidence.advisory_path.clone();
        }
        self.inflight_count += 1;
        self.events.push(CandidateEvent::Updated(candidate));
        if recognized_replacement {
            let _ = self.recognized_replacements.insert(locator);
        }
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

    fn record_presence(&mut self, locator: Locator) -> Result<(), DiscoveryError> {
        if self.present_locators.contains(&locator) {
            return Ok(());
        }
        if self.present_locators.len() >= self.max_live_entries {
            return self.mark_incomplete_overflow();
        }
        let inserted = self.present_locators.insert(locator);
        debug_assert!(inserted);
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

fn freeze_path_binding(
    frozen_path_index: &mut PathMap<AdvisoryPathKey, Locator>,
    rebind_conflicts: &mut HashSet<Locator>,
    ambiguous_paths: &mut PathSet<AdvisoryPathKey>,
    locator: Locator,
    path: &AdvisoryPath,
) {
    if path.kind() == AdvisoryPathKind::Unavailable {
        return;
    }
    let path_ref = AdvisoryPathRef::new(path);
    if ambiguous_paths.contains(&path_ref) {
        let _ = rebind_conflicts.insert(locator);
        return;
    }
    if let Some(previous) = frozen_path_index.insert(AdvisoryPathKey::from(path), locator)
        && previous != locator
    {
        let _ = frozen_path_index.remove(&path_ref);
        let _ = ambiguous_paths.insert(AdvisoryPathKey::from(path));
        let _ = rebind_conflicts.insert(previous);
        let _ = rebind_conflicts.insert(locator);
    }
}

fn candidate_signature(candidate: &DiscoveredCandidate) -> [u8; 32] {
    let evidence = &candidate.evidence;
    let resolved_path = candidate.resolved_path.as_os_str().as_encoded_bytes();
    let mut hasher = blake3::Hasher::new();
    let _ = hasher.update(b"otel-arrow-filelog-discovery-signature-v3\0");
    let _ = hasher.update(&(evidence.fingerprint.len() as u64).to_be_bytes());
    let _ = hasher.update(&evidence.fingerprint);
    // Hashes the advisory path's full kind, length, and digest alongside
    // its stored suffix bytes, never the stored suffix alone: two
    // differently-truncated paths that happen to share a stored suffix
    // must never collide into the same discovery signature.
    let _ = hasher.update(&[evidence.advisory_path.kind().to_wire()]);
    let _ = hasher.update(&evidence.advisory_path.full_path_len().to_be_bytes());
    let _ = hasher.update(evidence.advisory_path.full_path_digest());
    let _ = hasher.update(&(evidence.advisory_path.stored_path_bytes().len() as u64).to_be_bytes());
    let _ = hasher.update(evidence.advisory_path.stored_path_bytes());
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
