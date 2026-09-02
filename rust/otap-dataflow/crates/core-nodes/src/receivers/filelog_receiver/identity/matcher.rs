// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic checkpoint matching and atomic durable identity admission.

use std::collections::{HashMap, HashSet};

use super::{CandidateEvidence, IdentityError};
use crate::receivers::filelog_receiver::checkpoint::CommittedFrontierGuard;
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    AdvisoryPath, FRAMING_PROFILE_VERSION, FileId, FramingResume, LifecycleState, Locator,
    QUARANTINE_REASON_RECOVERY_MISMATCH, REMOVAL_REASON_LOCATOR_SUPERSEDED,
};
use crate::receivers::filelog_receiver::checkpoint::snapshot::SnapshotRecord;
use crate::receivers::filelog_receiver::checkpoint::store::{
    AtomicGroupAppendOutcome, AtomicGroupAppendPlan, CheckpointStore,
};
use crate::receivers::filelog_receiver::checkpoint::wal::{
    Operation, QuarantineFile, RegisterFile, RemoveFile, UpdateFingerprint, UpdateMetadata,
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
/// Fingerprint multiplicity is retained only for diagnostics and batch
/// consistency validation; it never selects an existing durable `file_id`.
/// The only durable recovery match is an exact runtime locator (see
/// [`IdentityMatch::ExactLocator`]). Callers that cannot inventory every
/// bounded pending/in-flight candidate must set `fingerprint_counts_complete`
/// false so batch validation stays conservative.
#[derive(Debug, Clone)]
pub(crate) struct CandidateInventory {
    live_locators: HashSet<Locator>,
    full_fingerprint_counts: HashMap<Vec<u8>, usize>,
    fingerprint_counts_complete: bool,
}

impl CandidateInventory {
    /// Reports whether fingerprint multiplicities cover the complete
    /// reconciliation population.
    #[cfg(test)]
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
    /// reconciliation. Batch fingerprint-multiplicity validation stays
    /// conservative; no durable recovery match depends on this flag.
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
    /// The only supported durable recovery match: handle locator plus
    /// compatible fingerprint prefix.
    ExactLocator,
    /// A genuinely new discovery governed by `start_at`.
    NewDiscovery,
    /// Recovery evidence existed but was unsafe to inherit; a new identity
    /// was created under `on_recovery_mismatch`.
    RecoveryMismatch,
    /// A validated move/create replacement recognized through another
    /// identity's rebounding distinguished matched-path binding. Registered
    /// at offset zero with clean framing regardless of `start_at`, and
    /// never through fingerprint- or path-based mismatch inheritance (see
    /// `docs/filelog-receiver-phase1-spec.md`, "Discovery and matching").
    RecognizedReplacement,
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
    /// The durable committed-frontier guard paired with `committed_offset`.
    ///
    /// For [`IdentityMatch::NewDiscovery`] or [`IdentityMatch::RecoveryMismatch`],
    /// this is real evidence already validated against the candidate's own
    /// handle (empty at offset `0`, or the candidate's exact EOF window).
    /// For [`IdentityMatch::ExactLocator`], this is the guard already
    /// durably recorded for the resumed identity;
    /// the reader must independently re-validate it against a freshly read
    /// window once its own descriptor is opened, never trusting the
    /// candidate's own (differently offset) evidence.
    pub(crate) committed_frontier_guard: CommittedFrontierGuard,
    /// The `AdvisoryPath` now authoritative for this identity's checkpoint
    /// record. For [`IdentityMatch::ExactLocator`] this is the candidate's
    /// path only when discovery supplied a confirmed, validated
    /// distinguished-binding reselection; otherwise it is the durable
    /// record's existing value, unchanged. This is what discovery
    /// reconstructs its own binding memory from (see
    /// `docs/filelog-receiver-phase1-spec.md`, "Discovery and matching").
    pub(crate) advisory_path: AdvisoryPath,
}

/// Why one durable identity is blocked without changing checkpoint state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdentityBlockReason {
    /// Stored framing-profile version or digest differs from configuration.
    IncompatibleProfile,
    /// Stored identity evidence begins at a different source boundary.
    IncompatibleIgnoredHeaderBytes,
}

/// Existing durable identity that cannot safely resume under this
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockedIdentity {
    pub(crate) file_id: FileId,
    pub(crate) advisory_path: AdvisoryPath,
    pub(crate) reason: IdentityBlockReason,
    stored_version: u16,
    stored_digest: [u8; 32],
    configured_version: u16,
    configured_digest: [u8; 32],
}

impl BlockedIdentity {
    #[cfg(test)]
    fn into_error(self) -> IdentityError {
        match self.reason {
            IdentityBlockReason::IncompatibleProfile => IdentityError::IncompatibleProfile {
                file_id: self.file_id,
                stored_version: self.stored_version,
                stored_digest: self.stored_digest,
                configured_version: self.configured_version,
                configured_digest: self.configured_digest,
            },
            IdentityBlockReason::IncompatibleIgnoredHeaderBytes => IdentityError::InvalidEvidence {
                reason: "checkpoint ignored_header_bytes differs from configuration",
            },
        }
    }
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
    /// Matching durable state exists but its resume profile is incompatible.
    /// The record remains unchanged and no reader is attached.
    Blocked(BlockedIdentity),
}

/// One fully planned reconciliation whose checkpoint transactions can resume
/// at the exact failed transaction without regenerating file identities.
#[derive(Debug)]
pub(crate) struct IdentityResolutionPlan {
    checkpoint: Option<AtomicGroupAppendPlan>,
    resolutions: Option<Vec<IdentityResolution>>,
}

impl IdentityResolutionPlan {
    /// Persists the remaining checkpoint transactions and returns the planned
    /// resolutions once every transaction succeeds.
    pub(crate) fn persist_cancellable(
        &mut self,
        store: &mut CheckpointStore,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<Vec<IdentityResolution>>, IdentityError> {
        if let Some(checkpoint) = &mut self.checkpoint {
            match store.append_atomic_group_plan_cancellable(checkpoint, &mut cancelled)? {
                AtomicGroupAppendOutcome::Completed(_outcomes) => {
                    self.checkpoint = None;
                }
                AtomicGroupAppendOutcome::Cancelled { .. } => return Ok(None),
            }
        }
        Ok(Some(self.resolutions.take().expect(
            "a completed identity plan returns its resolutions once",
        )))
    }
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

/// Which candidates' forwarded `AdvisoryPath` is safe to persist as the new
/// authoritative distinguished binding for an `IdentityMatch::ExactLocator`
/// reconnection.
///
/// Discovery only has continuity memory for a locator it has already been
/// tracking across scans; a locator it is observing for the first time
/// since process start (including immediately after a restart) forwards
/// whatever alias happened to be visited, which must not silently replace
/// an already-durable binding. Direct, non-admission resolution (used
/// throughout this module's own tests) has no such distinction and treats
/// every candidate's path as confirmed, matching this module's prior
/// behavior (`docs/filelog-receiver-phase1-spec.md`, "Discovery and
/// matching").
enum ConfirmedPathBindings<'a> {
    #[cfg(test)]
    All,
    Only(&'a HashSet<Locator>),
}

impl ConfirmedPathBindings<'_> {
    fn contains(&self, locator: Locator) -> bool {
        match self {
            #[cfg(test)]
            ConfirmedPathBindings::All => true,
            ConfirmedPathBindings::Only(set) => set.contains(&locator),
        }
    }
}

/// Resolves a bounded candidate batch and persists every registration or
/// matching-evidence update before returning it to a reader.
#[cfg(test)]
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
///
/// `recognized_replacements` names locators discovery has recognized this
/// pass as a validated move/create replacement for a rebounding
/// distinguished matched-path binding (see [`IdentityMatch::RecognizedReplacement`]).
/// `confirmed_path_bindings` names locators for which discovery's forwarded
/// candidate path is a validated distinguished-binding decision, safe to
/// persist over an existing durable record's `advisory_path`.
#[cfg(test)]
pub(crate) fn resolve_and_persist_with_admission(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    recognized_replacements: &HashSet<Locator>,
    confirmed_path_bindings: &HashSet<Locator>,
) -> Result<Vec<IdentityResolution>, IdentityError> {
    resolve_with_source_mode(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        &mut RandomFileIdSource,
        true,
        &mut || false,
        recognized_replacements,
        ConfirmedPathBindings::Only(confirmed_path_bindings),
    )
    .map(|resolved| resolved.expect("non-cancellable identity resolution cannot be cancelled"))
}

/// Resolves one complete reconciliation but abandons all planned operations
/// when cancellation becomes visible immediately before persistence. See
/// [`resolve_and_persist_with_admission`] for `recognized_replacements` and
/// `confirmed_path_bindings`.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_and_persist_with_admission_cancellable(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    recognized_replacements: &HashSet<Locator>,
    confirmed_path_bindings: &HashSet<Locator>,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<Vec<IdentityResolution>>, IdentityError> {
    resolve_with_source_mode(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        &mut RandomFileIdSource,
        true,
        &mut cancelled,
        recognized_replacements,
        ConfirmedPathBindings::Only(confirmed_path_bindings),
    )
}

/// Plans one capacity-aware reconciliation without beginning checkpoint I/O.
///
/// The caller can retain the returned plan across bounded store retries, so
/// randomly assigned file IDs and transaction boundaries remain exact.
pub(crate) fn plan_with_admission(
    store: &CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    recognized_replacements: &HashSet<Locator>,
    confirmed_path_bindings: &HashSet<Locator>,
) -> Result<IdentityResolutionPlan, IdentityError> {
    plan_with_source_mode(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        &mut RandomFileIdSource,
        true,
        recognized_replacements,
        ConfirmedPathBindings::Only(confirmed_path_bindings),
    )
}

#[cfg(test)]
pub(super) fn resolve_and_persist_with_source(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    file_ids: &mut impl FileIdSource,
) -> Result<Vec<ResolvedIdentity>, IdentityError> {
    let empty_replacements = HashSet::new();
    let resolutions = resolve_with_source_mode(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        file_ids,
        false,
        &mut || false,
        &empty_replacements,
        ConfirmedPathBindings::All,
    )?;
    let resolutions = resolutions.expect("non-cancellable identity resolution cannot be cancelled");
    resolutions
        .into_iter()
        .map(|resolution| match resolution {
            IdentityResolution::Resolved(resolved) => Ok(resolved),
            IdentityResolution::Deferred => Err(IdentityError::InvalidEvidence {
                reason: "non-admission identity resolution unexpectedly deferred a candidate",
            }),
            IdentityResolution::Blocked(blocked) => Err(blocked.into_error()),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn resolve_with_source_mode(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    file_ids: &mut impl FileIdSource,
    defer_new_at_capacity: bool,
    cancelled: &mut impl FnMut() -> bool,
    recognized_replacements: &HashSet<Locator>,
    confirmed_path_bindings: ConfirmedPathBindings<'_>,
) -> Result<Option<Vec<IdentityResolution>>, IdentityError> {
    let mut plan = plan_with_source_mode(
        store,
        candidates,
        inventory,
        settings,
        now_unix_nano,
        file_ids,
        defer_new_at_capacity,
        recognized_replacements,
        confirmed_path_bindings,
    )?;
    if !defer_new_at_capacity
        && let Some(blocked) = plan.resolutions.as_ref().and_then(|resolutions| {
            resolutions.iter().find_map(|resolution| match resolution {
                IdentityResolution::Blocked(blocked) => Some(blocked),
                IdentityResolution::Resolved(_) | IdentityResolution::Deferred => None,
            })
        })
    {
        return Err(blocked.clone().into_error());
    }
    if cancelled() {
        return Ok(None);
    }
    plan.persist_cancellable(store, cancelled)
}

#[allow(clippy::too_many_arguments)]
fn plan_with_source_mode(
    store: &CheckpointStore,
    candidates: &[CandidateEvidence],
    inventory: &CandidateInventory,
    settings: &IdentitySettings,
    now_unix_nano: u64,
    file_ids: &mut impl FileIdSource,
    defer_new_at_capacity: bool,
    recognized_replacements: &HashSet<Locator>,
    confirmed_path_bindings: ConfirmedPathBindings<'_>,
) -> Result<IdentityResolutionPlan, IdentityError> {
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
    let mut records_by_path: HashMap<&[u8; 32], Vec<&SnapshotRecord>> = HashMap::new();
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
            .entry(record.advisory_path.full_path_digest())
            .or_default()
            .push(record);
    }

    let mut plans: Vec<Option<PlannedIdentity>> = std::iter::repeat_with(|| None)
        .take(candidates.len())
        .collect();
    let mut blocked: Vec<Option<BlockedIdentity>> = std::iter::repeat_with(|| None)
        .take(candidates.len())
        .collect();
    let mut recovery_evidence = vec![false; candidates.len()];
    // Active records found at a candidate's exact locator that cannot be
    // resumed are retired atomically with the replacement registration.
    // RotatedFinalized history has no live locator claim and remains eligible
    // for ordinary filtered-compaction retention.
    let mut stale_locator_records: Vec<Vec<(FileId, u32, LifecycleState)>> =
        vec![Vec::new(); candidates.len()];

    // Exact locator is the only durable recovery match; each candidate has a
    // distinct locator (checked above) and each locator maps to at most one
    // non-ambiguous record, so no cross-candidate reservation is needed.
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(group) = records_by_locator.get(&candidate.locator) else {
            continue;
        };
        let mut live_records = group.iter().copied().filter(|record| {
            matches!(
                record.lifecycle_state,
                LifecycleState::Active | LifecycleState::Quarantined
            )
        });
        let Some(record) = live_records.next() else {
            continue;
        };
        if live_records.next().is_some() {
            return Err(IdentityError::InvalidEvidence {
                reason: "checkpoint table contains duplicate live locator claims",
            });
        }
        if let Some(incompatible) = profile_incompatibility(record, settings) {
            blocked[index] = Some(incompatible);
            continue;
        }
        if record.lifecycle_state == LifecycleState::Quarantined {
            plans[index] = Some(plan_existing(
                candidate,
                record,
                IdentityMatch::ExactLocator,
                now_unix_nano,
                confirmed_path_bindings.contains(candidate.locator),
            ));
            continue;
        }
        recovery_evidence[index] = true;
        if !candidate.fingerprint.starts_with(&record.fingerprint)
            || record.committed_offset > candidate.size
        {
            stale_locator_records[index] =
                vec![(record.file_id, record.file_epoch, record.lifecycle_state)];
            continue;
        }
        plans[index] = Some(plan_existing(
            candidate,
            record,
            IdentityMatch::ExactLocator,
            now_unix_nano,
            confirmed_path_bindings.contains(candidate.locator),
        ));
    }

    for (index, candidate) in candidates.iter().enumerate() {
        if plans[index].is_some() || blocked[index].is_some() {
            continue;
        }

        let is_recognized_replacement = recognized_replacements.contains(&candidate.locator);
        if !recovery_evidence[index] && !is_recognized_replacement {
            // A recognized move/create replacement never inherits progress
            // through fingerprint- or path-based association: only a
            // same-locator conflict detected above (genuine locator reuse)
            // still forces `on_recovery_mismatch` for a replacement.
            recovery_evidence[index] = has_unavailable_recovery_evidence(
                candidate,
                &records_by_fingerprint,
                &records_by_path,
                &inventory.live_locators,
            );
        }
        let mismatch = recovery_evidence[index];
        let is_clean_replacement = is_recognized_replacement && !mismatch;
        let (committed_offset, quarantine) = if is_clean_replacement {
            (0, false)
        } else {
            initial_state(candidate.size, mismatch, settings)
        };
        let file_id = generate_unique_file_id(&mut known_file_ids, file_ids)?;
        plans[index] = Some(plan_new(
            candidate,
            file_id,
            committed_offset,
            if is_clean_replacement {
                IdentityMatch::RecognizedReplacement
            } else if mismatch {
                IdentityMatch::RecoveryMismatch
            } else {
                IdentityMatch::NewDiscovery
            },
            quarantine,
            settings,
            now_unix_nano,
            &stale_locator_records[index],
        ));
    }

    let mut operation_groups = Vec::new();
    let mut resolutions = Vec::with_capacity(plans.len());
    let mut remaining_capacity = settings
        .max_tracked_files
        .checked_sub(store.table().len())
        .ok_or(IdentityError::InvalidEvidence {
            reason: "checkpoint table exceeds configured tracked-file capacity",
        })?;
    for (plan, blocked) in plans.into_iter().zip(blocked) {
        if let Some(blocked) = blocked {
            resolutions.push(IdentityResolution::Blocked(blocked));
            continue;
        }
        let plan = plan.expect("every unblocked candidate receives an identity plan");
        let creates_identity = matches!(
            plan.resolved.matched_by,
            IdentityMatch::NewDiscovery
                | IdentityMatch::RecoveryMismatch
                | IdentityMatch::RecognizedReplacement
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
    let checkpoint = if operation_groups.is_empty() {
        None
    } else {
        Some(store.prepare_atomic_group_append(operation_groups)?)
    };
    Ok(IdentityResolutionPlan {
        checkpoint,
        resolutions: Some(resolutions),
    })
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

fn profile_incompatibility(
    record: &SnapshotRecord,
    settings: &IdentitySettings,
) -> Option<BlockedIdentity> {
    if record.framing_profile_version != settings.framing_profile_version
        || record.framing_profile_digest != settings.framing_profile_digest
    {
        return Some(BlockedIdentity {
            file_id: record.file_id,
            advisory_path: record.advisory_path.clone(),
            reason: IdentityBlockReason::IncompatibleProfile,
            stored_version: record.framing_profile_version,
            stored_digest: record.framing_profile_digest,
            configured_version: settings.framing_profile_version,
            configured_digest: settings.framing_profile_digest,
        });
    }
    if record.ignored_header_bytes != settings.ignored_header_bytes {
        return Some(BlockedIdentity {
            file_id: record.file_id,
            advisory_path: record.advisory_path.clone(),
            reason: IdentityBlockReason::IncompatibleIgnoredHeaderBytes,
            stored_version: record.framing_profile_version,
            stored_digest: record.framing_profile_digest,
            configured_version: settings.framing_profile_version,
            configured_digest: settings.framing_profile_digest,
        });
    }
    None
}

fn has_unavailable_recovery_evidence(
    candidate: &CandidateEvidence,
    records_by_fingerprint: &HashMap<&[u8], Vec<&SnapshotRecord>>,
    records_by_path: &HashMap<&[u8; 32], Vec<&SnapshotRecord>>,
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
        .get(candidate.advisory_path.full_path_digest())
        .is_some_and(|records| {
            records.iter().any(|record| {
                record.advisory_path == candidate.advisory_path
                    && record.lifecycle_state == LifecycleState::Active
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
    path_binding_confirmed: bool,
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
    // Only a confirmed, validated distinguished-binding reselection may
    // replace the durable `advisory_path`: an unconfirmed candidate (a
    // locator discovery has no continuity memory for yet, including
    // immediately after a restart) forwards whatever alias it happened to
    // observe first, which must never silently overwrite an
    // already-durable binding (`docs/filelog-receiver-phase1-spec.md`,
    // "Discovery and matching").
    let apply_path_update =
        path_binding_confirmed && record.advisory_path != candidate.advisory_path;
    if record.lifecycle_state != LifecycleState::RotatedFinalized
        && (record.last_seen_time_unix_nano != now_unix_nano || apply_path_update)
    {
        operations.push(Operation::UpdateMetadata(UpdateMetadata {
            file_id: record.file_id,
            expected_prior_state: record.lifecycle_state,
            expected_file_epoch: record.file_epoch,
            last_seen_time_unix_nano: now_unix_nano,
            advisory_path: apply_path_update.then(|| candidate.advisory_path.clone()),
        }));
    }
    let advisory_path = if apply_path_update {
        candidate.advisory_path.clone()
    } else {
        record.advisory_path.clone()
    };
    PlannedIdentity {
        resolved: ResolvedIdentity {
            file_id: record.file_id,
            file_epoch: record.file_epoch,
            committed_offset: record.committed_offset,
            framing_resume: record.framing_resume,
            lifecycle_state: record.lifecycle_state,
            matched_by,
            committed_frontier_guard: record.committed_frontier_guard,
            advisory_path,
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
    stale_locator_records: &[(FileId, u32, LifecycleState)],
) -> PlannedIdentity {
    // `committed_offset` is always `0` (`start_at: beginning`, or a
    // recovery mismatch under `on_recovery_mismatch: beginning`/`fail`) or
    // exactly `candidate.size` (`start_at: end`, or a recovery mismatch
    // under `on_recovery_mismatch: skip_to_end`); see `initial_state`. The
    // real committed-frontier window is exact empty evidence for the first
    // case, and the exact trailing window already read from the same
    // validated handle for the second -- never a fabricated placeholder.
    let committed_frontier_guard = if committed_offset == 0 {
        CommittedFrontierGuard::empty()
    } else {
        debug_assert_eq!(committed_offset, candidate.size);
        debug_assert_eq!(
            candidate.committed_frontier_window.end_offset(),
            committed_offset
        );
        candidate
            .committed_frontier_window
            .guard()
            .expect("candidate window was already validated against candidate.size")
    };
    let register = RegisterFile {
        file_id,
        file_epoch: 1,
        committed_offset,
        committed_frontier_guard,
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
    // Retire every stale non-quarantined record this candidate's exact
    // locator can no longer resume, atomically with its own new
    // registration: a locator is claimed by exactly one real object at a
    // time, so once this new identity is durable, the old record's
    // `locator` value must never again match a live object. Removing it
    // here (rather than leaving it for time-based retention, which is not
    // itself locator-aware) is what keeps a later exact-locator lookup for
    // this same locator unambiguous.
    for &(stale_file_id, stale_file_epoch, stale_prior_state) in stale_locator_records {
        operations.push(Operation::RemoveFile(RemoveFile {
            file_id: stale_file_id,
            expected_file_epoch: stale_file_epoch,
            expected_prior_state: stale_prior_state,
            removal_reason: REMOVAL_REASON_LOCATOR_SUPERSEDED,
            removal_time_unix_nano: now_unix_nano,
            administrative: false,
            namespace_id: None,
            audit_reason: None,
        }));
    }
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
            committed_frontier_guard,
            advisory_path: candidate.advisory_path.clone(),
        },
        operations,
    }
}
