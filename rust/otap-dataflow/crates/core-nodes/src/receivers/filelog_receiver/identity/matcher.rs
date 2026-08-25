// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic checkpoint matching and atomic durable identity admission.

use std::collections::{HashMap, HashSet};

use super::{CandidateEvidence, IdentityError};
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    ADVISORY_PATH_MAX_BYTES, FRAMING_PROFILE_VERSION, FileId, FramingResume, LifecycleState,
    Locator, QUARANTINE_REASON_RECOVERY_MISMATCH,
};
use crate::receivers::filelog_receiver::checkpoint::snapshot::SnapshotRecord;
use crate::receivers::filelog_receiver::checkpoint::store::CheckpointStore;
use crate::receivers::filelog_receiver::checkpoint::wal::{
    Operation, QuarantineFile, RegisterFile, UpdateFingerprint, UpdateMetadata,
};
use crate::receivers::filelog_receiver::config::{OnRecoveryMismatch, RuntimeConfig, StartAt};

const FILE_ID_GENERATION_ATTEMPTS: usize = 32;

/// Validated settings needed by identity resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentitySettings {
    pub(crate) fingerprint_bytes: u16,
    pub(crate) ignored_header_bytes: u32,
    pub(crate) start_at: StartAt,
    pub(crate) on_recovery_mismatch: OnRecoveryMismatch,
    pub(crate) framing_profile_version: u16,
    pub(crate) framing_profile_digest: [u8; 32],
    pub(crate) max_candidates: usize,
    pub(crate) max_inventory_candidates: usize,
    pub(crate) max_tracked_files: usize,
}

/// Resolver-wide view of all live and pending candidates participating in
/// one reconciliation pass.
///
/// Fingerprint-only recovery is enabled only when
/// `fingerprint_counts_complete` is true. Callers that cannot inventory
/// every bounded pending/in-flight candidate must set it false.
#[derive(Debug, Clone)]
pub(crate) struct CandidateInventory {
    live_locators: HashSet<Locator>,
    full_fingerprint_counts: HashMap<Vec<u8>, usize>,
    fingerprint_counts_complete: bool,
}

impl CandidateInventory {
    /// Reports whether fingerprint multiplicities cover the complete
    /// reconciliation population.
    pub(crate) fn is_complete(&self) -> bool {
        self.fingerprint_counts_complete
    }

    /// Replaces one live locator's full-fingerprint contribution after the
    /// worker refreshes stale queued evidence from its retained descriptor.
    pub(crate) fn replace_fingerprint_observation(
        &mut self,
        locator: Locator,
        previous: &[u8],
        refreshed: &[u8],
        fingerprint_bytes: u16,
    ) -> Result<(), IdentityError> {
        if !self.live_locators.contains(&locator) {
            return Err(IdentityError::InvalidEvidence {
                reason: "refreshed candidate locator is absent from the reconciliation inventory",
            });
        }
        if previous == refreshed {
            return Ok(());
        }

        let full_len = usize::from(fingerprint_bytes);
        if previous.len() == full_len {
            let remove_previous = {
                let count = self.full_fingerprint_counts.get_mut(previous).ok_or(
                    IdentityError::InvalidEvidence {
                        reason: "refreshed candidate fingerprint is absent from the inventory",
                    },
                )?;
                *count = count.checked_sub(1).ok_or(IdentityError::InvalidEvidence {
                    reason: "refreshed candidate fingerprint count underflowed",
                })?;
                *count == 0
            };
            if remove_previous {
                let _ = self.full_fingerprint_counts.remove(previous);
            }
        }
        if refreshed.len() == full_len {
            let count = self
                .full_fingerprint_counts
                .entry(refreshed.to_vec())
                .or_default();
            *count = count.checked_add(1).ok_or(IdentityError::InvalidEvidence {
                reason: "refreshed candidate fingerprint count overflowed",
            })?;
        }
        Ok(())
    }

    /// Builds an inventory only when the reconciliation owner has retained
    /// every eligible candidate and observed no scan overflow or unstable
    /// evidence.
    pub(crate) fn from_complete_reconciliation(
        all_candidates: &[CandidateEvidence],
        other_live_locators: &HashSet<Locator>,
        fingerprint_bytes: u16,
    ) -> Self {
        Self::build(all_candidates, other_live_locators, fingerprint_bytes, true)
    }

    /// Builds a fail-safe inventory for an incomplete/overflowed
    /// reconciliation. Fingerprint-only matching is disabled.
    pub(crate) fn from_incomplete_reconciliation(
        retained_candidates: &[CandidateEvidence],
        other_live_locators: &HashSet<Locator>,
        fingerprint_bytes: u16,
    ) -> Self {
        Self::build(
            retained_candidates,
            other_live_locators,
            fingerprint_bytes,
            false,
        )
    }

    fn build(
        candidates: &[CandidateEvidence],
        other_live_locators: &HashSet<Locator>,
        fingerprint_bytes: u16,
        fingerprint_counts_complete: bool,
    ) -> Self {
        let mut live_locators = other_live_locators.clone();
        let mut full_fingerprint_counts = HashMap::new();
        for candidate in candidates {
            let _ = live_locators.insert(candidate.locator);
            if candidate.fingerprint.len() == usize::from(fingerprint_bytes) {
                *full_fingerprint_counts
                    .entry(candidate.fingerprint.clone())
                    .or_default() += 1;
            }
        }
        Self {
            live_locators,
            full_fingerprint_counts,
            fingerprint_counts_complete,
        }
    }
}

impl IdentitySettings {
    /// Extracts identity settings from a fully validated receiver
    /// configuration.
    pub(crate) fn from_runtime(config: &RuntimeConfig) -> Self {
        Self {
            fingerprint_bytes: u16::try_from(config.identity.fingerprint_bytes)
                .expect("validated fingerprint_bytes fits u16"),
            ignored_header_bytes: u32::try_from(config.identity.ignored_header_bytes)
                .expect("validated ignored_header_bytes fits u32"),
            start_at: config.start_at,
            on_recovery_mismatch: config.identity.on_recovery_mismatch,
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: config.framing_profile_digest,
            max_candidates: config.limits.max_open_files as usize,
            max_inventory_candidates: usize::try_from(
                u64::from(config.limits.max_tracked_files)
                    + u64::from(config.limits.max_pending_candidates)
                    + u64::from(config.limits.max_open_files),
            )
            .expect("validated candidate inventory population fits usize"),
            max_tracked_files: config.limits.max_tracked_files as usize,
        }
    }
}

/// Why one candidate received its resolved durable identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityMatch {
    /// Strongest match: handle locator plus compatible fingerprint prefix.
    ExactLocator,
    /// Restart-only reconnect by a unique complete fingerprint.
    UniqueFingerprint,
    /// A genuinely new discovery governed by `start_at`.
    NewDiscovery,
    /// Recovery evidence existed but was unsafe to inherit; a new identity
    /// was created under `on_recovery_mismatch`.
    RecoveryMismatch,
}

/// Durable state selected for one candidate after persistence succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedIdentity {
    pub(crate) file_id: FileId,
    pub(crate) file_epoch: u32,
    pub(crate) committed_offset: u64,
    pub(crate) framing_resume: FramingResume,
    pub(crate) lifecycle_state: LifecycleState,
    pub(crate) matched_by: IdentityMatch,
}

/// Capacity-aware result for one candidate in reconciliation order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdentityResolution {
    /// Durable identity operations succeeded and the candidate can be
    /// associated with runtime state.
    Resolved(ResolvedIdentity),
    /// A genuinely new identity could not consume durable tracked capacity.
    /// No operation for this candidate was persisted.
    Deferred,
}

#[derive(Debug)]
struct PlannedIdentity {
    resolved: ResolvedIdentity,
    operations: Vec<Operation>,
}

pub(super) trait FileIdSource {
    fn next_file_id(&mut self) -> FileId;
}

struct RandomFileIdSource;

impl FileIdSource for RandomFileIdSource {
    fn next_file_id(&mut self) -> FileId {
        FileId::from_bytes(rand::random())
    }
}

/// Resolves a bounded candidate batch and persists every registration or
/// matching-evidence update before returning it to a reader.
pub(crate) fn resolve_and_persist(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
) -> Result<Vec<ResolvedIdentity>, IdentityError> {
    resolve_and_persist_with_source(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        &mut RandomFileIdSource,
    )
}

/// Resolves one complete reconciliation while deferring only new identities
/// that cannot consume durable tracked-file capacity.
pub(crate) fn resolve_and_persist_with_admission(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
) -> Result<Vec<IdentityResolution>, IdentityError> {
    resolve_with_source_mode(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        &mut RandomFileIdSource,
        true,
    )
}

pub(super) fn resolve_and_persist_with_source(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    file_ids: &mut impl FileIdSource,
) -> Result<Vec<ResolvedIdentity>, IdentityError> {
    let resolutions = resolve_with_source_mode(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        file_ids,
        false,
    )?;
    resolutions
        .into_iter()
        .map(|resolution| match resolution {
            IdentityResolution::Resolved(resolved) => Ok(resolved),
            IdentityResolution::Deferred => Err(IdentityError::InvalidEvidence {
                reason: "non-admission identity resolution unexpectedly deferred a candidate",
            }),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn resolve_with_source_mode(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    file_ids: &mut impl FileIdSource,
    defer_new_at_capacity: bool,
) -> Result<Vec<IdentityResolution>, IdentityError> {
    validate_resumption_profiles(store, settings)?;
    validate_candidates(candidates, inventory, settings)?;

    let mut batch_locators = HashSet::with_capacity(candidates.len());
    for candidate in candidates {
        if !batch_locators.insert(candidate.locator) {
            return Err(IdentityError::DuplicateCandidateLocator {
                locator: candidate.locator,
            });
        }
    }

    let mut records_by_locator: HashMap<Locator, Vec<&SnapshotRecord>> = HashMap::new();
    let mut records_by_fingerprint: HashMap<&[u8], Vec<&SnapshotRecord>> = HashMap::new();
    let mut records_by_path: HashMap<&[u8], Vec<&SnapshotRecord>> = HashMap::new();
    let mut known_file_ids = HashSet::with_capacity(store.table().len());
    for (file_id, record) in store.table().iter() {
        let _ = known_file_ids.insert(*file_id);
        records_by_locator
            .entry(record.locator)
            .or_default()
            .push(record);
        records_by_fingerprint
            .entry(record.fingerprint.as_slice())
            .or_default()
            .push(record);
        records_by_path
            .entry(record.advisory_path.as_slice())
            .or_default()
            .push(record);
    }

    let mut plans: Vec<Option<PlannedIdentity>> = std::iter::repeat_with(|| None)
        .take(candidates.len())
        .collect();
    let mut recovery_evidence = vec![false; candidates.len()];
    let mut reserved_file_ids = HashSet::new();

    // Exact-locator matches reserve their records before weaker fingerprint
    // matching, making the result independent of candidate iteration order.
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(group) = records_by_locator.get(&candidate.locator) else {
            continue;
        };
        if group.len() != 1 {
            if group
                .iter()
                .any(|record| record.lifecycle_state == LifecycleState::Quarantined)
            {
                return Err(IdentityError::AmbiguousQuarantinedLocator {
                    locator: candidate.locator,
                });
            }
            recovery_evidence[index] = true;
            continue;
        }
        let record = group[0];
        if record.lifecycle_state == LifecycleState::Quarantined {
            let _ = reserved_file_ids.insert(record.file_id);
            plans[index] = Some(plan_existing(
                candidate,
                record,
                IdentityMatch::ExactLocator,
                now_unix_nano,
            ));
            continue;
        }
        recovery_evidence[index] = true;
        if !candidate.fingerprint.starts_with(&record.fingerprint)
            || record.committed_offset > candidate.size
        {
            continue;
        }
        let _ = reserved_file_ids.insert(record.file_id);
        plans[index] = Some(plan_existing(
            candidate,
            record,
            IdentityMatch::ExactLocator,
            now_unix_nano,
        ));
    }

    for (index, candidate) in candidates.iter().enumerate() {
        if plans[index].is_some()
            || !inventory.fingerprint_counts_complete
            || candidate.fingerprint.len() != usize::from(settings.fingerprint_bytes)
            || inventory
                .full_fingerprint_counts
                .get(candidate.fingerprint.as_slice())
                .copied()
                != Some(1)
        {
            continue;
        }

        let Some(group) = records_by_fingerprint.get(candidate.fingerprint.as_slice()) else {
            continue;
        };
        recovery_evidence[index] = group
            .iter()
            .any(|record| record.lifecycle_state == LifecycleState::Active);
        let [record] = group.as_slice() else {
            continue;
        };
        if record.lifecycle_state != LifecycleState::Active
            || record.fingerprint.len() != usize::from(settings.fingerprint_bytes)
            || inventory.live_locators.contains(&record.locator)
            || record.committed_offset > candidate.size
            || reserved_file_ids.contains(&record.file_id)
        {
            continue;
        }
        let _ = reserved_file_ids.insert(record.file_id);
        plans[index] = Some(plan_existing(
            candidate,
            record,
            IdentityMatch::UniqueFingerprint,
            now_unix_nano,
        ));
    }

    for (index, candidate) in candidates.iter().enumerate() {
        if plans[index].is_some() {
            continue;
        }

        if !recovery_evidence[index] {
            recovery_evidence[index] = has_unavailable_recovery_evidence(
                candidate,
                &records_by_fingerprint,
                &records_by_path,
                &inventory.live_locators,
            );
        }
        let mismatch = recovery_evidence[index];
        let (committed_offset, quarantine) = initial_state(candidate.size, mismatch, settings);
        let file_id = generate_unique_file_id(&mut known_file_ids, file_ids)?;
        plans[index] = Some(plan_new(
            candidate,
            file_id,
            committed_offset,
            if mismatch {
                IdentityMatch::RecoveryMismatch
            } else {
                IdentityMatch::NewDiscovery
            },
            quarantine,
            settings,
            now_unix_nano,
        ));
    }

    let plans: Vec<PlannedIdentity> = plans
        .into_iter()
        .map(|plan| plan.expect("every validated candidate receives an identity plan"))
        .collect();
    let mut operation_groups = Vec::new();
    let mut resolutions = Vec::with_capacity(plans.len());
    let mut remaining_capacity = settings
        .max_tracked_files
        .checked_sub(store.table().len())
        .ok_or(IdentityError::InvalidEvidence {
            reason: "checkpoint table exceeds configured tracked-file capacity",
        })?;
    for plan in plans {
        let creates_identity = matches!(
            plan.resolved.matched_by,
            IdentityMatch::NewDiscovery | IdentityMatch::RecoveryMismatch
        );
        if defer_new_at_capacity && creates_identity && remaining_capacity == 0 {
            resolutions.push(IdentityResolution::Deferred);
            continue;
        }
        if creates_identity {
            remaining_capacity =
                remaining_capacity
                    .checked_sub(1)
                    .ok_or(IdentityError::InvalidEvidence {
                        reason: "identity admission capacity accounting underflowed",
                    })?;
        }
        if !plan.operations.is_empty() {
            operation_groups.push(plan.operations);
        }
        resolutions.push(IdentityResolution::Resolved(plan.resolved));
    }
    if !operation_groups.is_empty() {
        let _outcomes = store.append_atomic_groups(operation_groups)?;
    }

    Ok(resolutions)
}

fn validate_candidates(
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
) -> Result<(), IdentityError> {
    if candidates.len() > settings.max_candidates {
        return Err(IdentityError::InvalidEvidence {
            reason: "opened candidate batch exceeds limits.max_open_files",
        });
    }
    if inventory.live_locators.len() > settings.max_inventory_candidates {
        return Err(IdentityError::InvalidEvidence {
            reason: "candidate inventory exceeds configured live-locator bounds",
        });
    }
    let mut inventoried_fingerprints = 0usize;
    for (fingerprint, count) in &inventory.full_fingerprint_counts {
        if fingerprint.len() != usize::from(settings.fingerprint_bytes) || *count == 0 {
            return Err(IdentityError::InvalidEvidence {
                reason: "candidate inventory contains an invalid full fingerprint count",
            });
        }
        inventoried_fingerprints =
            inventoried_fingerprints
                .checked_add(*count)
                .ok_or(IdentityError::InvalidEvidence {
                    reason: "candidate fingerprint inventory count overflows usize",
                })?;
    }
    if inventoried_fingerprints > settings.max_inventory_candidates {
        return Err(IdentityError::InvalidEvidence {
            reason: "candidate fingerprint inventory exceeds configured bounds",
        });
    }
    let mut batch_fingerprint_counts: HashMap<&[u8], usize> = HashMap::new();
    for candidate in candidates {
        if candidate.locator == Locator::Unspecified {
            return Err(IdentityError::InvalidEvidence {
                reason: "candidate has no supported handle-derived locator",
            });
        }
        if candidate.fingerprint.len() > usize::from(settings.fingerprint_bytes) {
            return Err(IdentityError::InvalidEvidence {
                reason: "candidate fingerprint exceeds configured evidence window",
            });
        }
        if candidate.advisory_path.len() > ADVISORY_PATH_MAX_BYTES {
            return Err(IdentityError::InvalidEvidence {
                reason: "candidate advisory path exceeds the durable byte bound",
            });
        }
        let expected_fingerprint_len = u64::from(settings.fingerprint_bytes).min(
            candidate
                .size
                .saturating_sub(u64::from(settings.ignored_header_bytes)),
        );
        if u64::try_from(candidate.fingerprint.len())
            .expect("u16-bounded fingerprint length fits u64")
            != expected_fingerprint_len
        {
            return Err(IdentityError::InvalidEvidence {
                reason: "candidate fingerprint is inconsistent with the observed file size",
            });
        }
        if !inventory.live_locators.contains(&candidate.locator) {
            return Err(IdentityError::InvalidEvidence {
                reason: "candidate locator is absent from the reconciliation inventory",
            });
        }
        if inventory.fingerprint_counts_complete
            && candidate.fingerprint.len() == usize::from(settings.fingerprint_bytes)
        {
            *batch_fingerprint_counts
                .entry(candidate.fingerprint.as_slice())
                .or_default() += 1;
        }
    }
    if inventory.fingerprint_counts_complete {
        for (fingerprint, observed_count) in batch_fingerprint_counts {
            if inventory
                .full_fingerprint_counts
                .get(fingerprint)
                .copied()
                .unwrap_or_default()
                < observed_count
            {
                return Err(IdentityError::InvalidEvidence {
                    reason: "complete reconciliation inventory under-counts a batch fingerprint",
                });
            }
        }
    }
    Ok(())
}

fn validate_resumption_profiles(
    store: &CheckpointStore,
    settings: &IdentitySettings,
) -> Result<(), IdentityError> {
    for (_, record) in store.table().iter() {
        if record.lifecycle_state == LifecycleState::RotatedFinalized {
            continue;
        }
        ensure_profile_compatible(record, settings)?;
        if record.ignored_header_bytes != settings.ignored_header_bytes {
            return Err(IdentityError::InvalidEvidence {
                reason: "checkpoint ignored_header_bytes differs from configuration",
            });
        }
    }
    Ok(())
}

fn ensure_profile_compatible(
    record: &SnapshotRecord,
    settings: &IdentitySettings,
) -> Result<(), IdentityError> {
    if record.framing_profile_version != settings.framing_profile_version
        || record.framing_profile_digest != settings.framing_profile_digest
    {
        return Err(IdentityError::IncompatibleProfile {
            file_id: record.file_id,
            stored_version: record.framing_profile_version,
            stored_digest: record.framing_profile_digest,
            configured_version: settings.framing_profile_version,
            configured_digest: settings.framing_profile_digest,
        });
    }
    Ok(())
}

fn has_unavailable_recovery_evidence(
    candidate: &CandidateEvidence,
    records_by_fingerprint: &HashMap<&[u8], Vec<&SnapshotRecord>>,
    records_by_path: &HashMap<&[u8], Vec<&SnapshotRecord>>,
    live_locators: &HashSet<Locator>,
) -> bool {
    if !candidate.fingerprint.is_empty() {
        if let Some(records) = records_by_fingerprint.get(candidate.fingerprint.as_slice()) {
            if records
                .iter()
                .any(|record| record.lifecycle_state == LifecycleState::Active)
            {
                return true;
            }
        }
    }
    records_by_path
        .get(candidate.advisory_path.as_slice())
        .is_some_and(|records| {
            records.iter().any(|record| {
                record.lifecycle_state == LifecycleState::Active
                    && !live_locators.contains(&record.locator)
            })
        })
}

fn initial_state(size: u64, mismatch: bool, settings: &IdentitySettings) -> (u64, bool) {
    if mismatch {
        match settings.on_recovery_mismatch {
            OnRecoveryMismatch::Beginning => (0, false),
            OnRecoveryMismatch::SkipToEnd => (size, false),
            OnRecoveryMismatch::Fail => (0, true),
        }
    } else {
        match settings.start_at {
            StartAt::Beginning => (0, false),
            StartAt::End => (size, false),
        }
    }
}

fn generate_unique_file_id(
    known: &mut HashSet<FileId>,
    source: &mut impl FileIdSource,
) -> Result<FileId, IdentityError> {
    for _ in 0..FILE_ID_GENERATION_ATTEMPTS {
        let candidate = source.next_file_id();
        if known.insert(candidate) {
            return Ok(candidate);
        }
    }
    Err(IdentityError::FileIdCollisionLimit {
        attempts: FILE_ID_GENERATION_ATTEMPTS,
    })
}

fn plan_existing(
    candidate: &CandidateEvidence,
    record: &SnapshotRecord,
    matched_by: IdentityMatch,
    now_unix_nano: u64,
) -> PlannedIdentity {
    let mut operations = Vec::with_capacity(2);
    if record.lifecycle_state == LifecycleState::Active
        && candidate.fingerprint.len() > record.fingerprint.len()
    {
        operations.push(Operation::UpdateFingerprint(UpdateFingerprint {
            file_id: record.file_id,
            expected_file_epoch: record.file_epoch,
            expected_fingerprint: record.fingerprint.clone(),
            new_fingerprint: candidate.fingerprint.clone(),
        }));
    }
    if record.lifecycle_state != LifecycleState::RotatedFinalized
        && (record.last_seen_time_unix_nano != now_unix_nano
            || record.advisory_path != candidate.advisory_path
            || (record.lifecycle_state == LifecycleState::Active
                && record.locator != candidate.locator))
    {
        operations.push(Operation::UpdateMetadata(UpdateMetadata {
            file_id: record.file_id,
            locator: (record.lifecycle_state == LifecycleState::Active
                && record.locator != candidate.locator)
                .then_some(candidate.locator),
            last_seen_time_unix_nano: now_unix_nano,
            advisory_path: (record.advisory_path != candidate.advisory_path)
                .then(|| candidate.advisory_path.clone()),
        }));
    }
    PlannedIdentity {
        resolved: ResolvedIdentity {
            file_id: record.file_id,
            file_epoch: record.file_epoch,
            committed_offset: record.committed_offset,
            framing_resume: record.framing_resume,
            lifecycle_state: record.lifecycle_state,
            matched_by,
        },
        operations,
    }
}

fn plan_new(
    candidate: &CandidateEvidence,
    file_id: FileId,
    committed_offset: u64,
    matched_by: IdentityMatch,
    quarantine: bool,
    settings: &IdentitySettings,
    now_unix_nano: u64,
) -> PlannedIdentity {
    let register = RegisterFile {
        file_id,
        file_epoch: 1,
        committed_offset,
        fingerprint: candidate.fingerprint.clone(),
        ignored_header_bytes: settings.ignored_header_bytes,
        locator: candidate.locator,
        framing_profile_version: settings.framing_profile_version,
        framing_profile_digest: settings.framing_profile_digest,
        framing_resume: FramingResume::Clean,
        last_seen_time_unix_nano: now_unix_nano,
        advisory_path: candidate.advisory_path.clone(),
    };
    let mut operations = vec![Operation::RegisterFile(register)];
    if quarantine {
        operations.push(Operation::QuarantineFile(QuarantineFile {
            file_id,
            expected_file_epoch: 1,
            reason_code: QUARANTINE_REASON_RECOVERY_MISMATCH,
            locator: candidate.locator,
            observed_size: candidate.size,
            quarantine_epoch: 1,
            quarantine_time_unix_nano: now_unix_nano,
        }));
    }
    PlannedIdentity {
        resolved: ResolvedIdentity {
            file_id,
            file_epoch: 1,
            committed_offset,
            framing_resume: FramingResume::Clean,
            lifecycle_state: if quarantine {
                LifecycleState::Quarantined
            } else {
                LifecycleState::Active
            },
            matched_by,
        },
        operations,
    }
}
