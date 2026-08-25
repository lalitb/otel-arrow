// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Replay of decoded WAL operations against an in-memory checkpoint table.
//!
//! This module implements the exact preconditions, idempotency rules, and
//! effects for all eight logical WAL operations from
//! `docs/filelog-checkpoint-format.md`, "Replay preconditions, idempotency,
//! and exact transition restrictions". It operates purely on decoded values
//! already produced by [`super::wal`]; it performs no filesystem I/O, no
//! locking, and no OS-specific locator lookups.

use std::collections::HashMap;

use super::error::{ApplyError, DecodeError};
use super::primitives::{FileId, FramingResume, LifecycleState};
use super::snapshot::{QuarantineEvidence, SnapshotRecord};
use super::wal::{Operation, ResetQuarantineAction, Transaction};

/// One in-memory checkpoint record. Identical in shape to
/// [`SnapshotRecord`]: applying every durable operation for a `file_id`
/// produces exactly the record a snapshot would persist for it, so the two
/// share one type rather than risking field drift between two parallel
/// definitions.
pub type TableRecord = SnapshotRecord;

/// The in-memory, replayed checkpoint state for one namespace: a table of
/// [`TableRecord`] keyed by [`FileId`].
#[derive(Debug, Clone, Default)]
pub struct CheckpointTable {
    records: HashMap<FileId, TableRecord>,
    quarantined_records: usize,
}

/// Bounded transaction scratch state validated against an unchanged table.
pub(crate) struct StagedOperations {
    touched: HashMap<FileId, Option<TableRecord>>,
}

impl CheckpointTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds a table from a decoded snapshot's records (the recovery base).
    ///
    /// Fails closed with [`DecodeError::DuplicateFileId`] if two records
    /// share a `file_id`: `file_id` is the record key and must uniquely
    /// identify a record, whether the records came from this codec's own
    /// `decode_snapshot` (which already rejects this at the byte level) or
    /// were constructed directly by a caller bypassing the byte codec
    /// entirely.
    pub fn from_snapshot_records(records: Vec<SnapshotRecord>) -> Result<Self, DecodeError> {
        let mut table = Self::new();
        for record in records {
            let file_id = record.file_id;
            if table.records.insert(file_id, record).is_some() {
                return Err(DecodeError::DuplicateFileId {
                    file_id,
                    context: "from_snapshot_records",
                });
            }
        }
        table.quarantined_records = table
            .records
            .values()
            .filter(|record| record.lifecycle_state == LifecycleState::Quarantined)
            .count();
        Ok(table)
    }

    /// Looks up the current record for `file_id`, if any.
    #[must_use]
    pub fn get(&self, file_id: &FileId) -> Option<&TableRecord> {
        self.records.get(file_id)
    }

    /// Number of tracked records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Number of durable quarantined records.
    #[must_use]
    pub const fn quarantined_len(&self) -> usize {
        self.quarantined_records
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Iterates every record and its key, in unspecified order.
    ///
    /// Borrowing avoids the whole-table clone
    /// [`Self::snapshot_records`] performs, which matters for read-only
    /// scans such as the durable store's retention pass.
    pub fn iter(&self) -> impl Iterator<Item = (&FileId, &TableRecord)> {
        self.records.iter()
    }

    /// Returns every record, ordered by `file_id`, suitable for deterministic
    /// snapshot encoding.
    #[must_use]
    pub fn snapshot_records(&self) -> Vec<SnapshotRecord> {
        let mut records: Vec<SnapshotRecord> = self.records.values().cloned().collect();
        records.sort_by_key(|record| record.file_id);
        records
    }

    /// Validates and stages `operations` in a bounded scratch map without
    /// changing the table.
    pub(crate) fn stage_operations(
        &self,
        operations: &[Operation],
        namespace_id: &str,
    ) -> Result<StagedOperations, ApplyError> {
        let mut touched: HashMap<FileId, Option<TableRecord>> = HashMap::new();
        for operation in operations {
            let file_id = operation.file_id();
            let _ = touched
                .entry(file_id)
                .or_insert_with(|| self.records.get(&file_id).cloned());
        }
        for operation in operations {
            Self::apply_operation(&mut touched, operation, namespace_id)?;
        }
        Ok(StagedOperations { touched })
    }

    /// Commits scratch state already validated against this unchanged table.
    pub(crate) fn commit_staged(&mut self, staged: StagedOperations) {
        for (file_id, record) in staged.touched {
            let old_quarantined = self
                .records
                .get(&file_id)
                .is_some_and(|record| record.lifecycle_state == LifecycleState::Quarantined);
            let new_quarantined = record
                .as_ref()
                .is_some_and(|record| record.lifecycle_state == LifecycleState::Quarantined);
            match (old_quarantined, new_quarantined) {
                (false, true) => {
                    self.quarantined_records = self
                        .quarantined_records
                        .checked_add(1)
                        .expect("staged quarantine count cannot exceed the table");
                }
                (true, false) => {
                    self.quarantined_records = self
                        .quarantined_records
                        .checked_sub(1)
                        .expect("staged quarantine count matches the table");
                }
                (false, false) | (true, true) => {}
            }
            match record {
                Some(record) => {
                    let _ = self.records.insert(file_id, record);
                }
                None => {
                    let _ = self.records.remove(&file_id);
                }
            }
        }
    }

    /// Validates every supplied operation without changing the table.
    ///
    /// The durable store uses this to preflight a caller batch that will be
    /// split across multiple on-disk transactions. Deterministic failures
    /// are therefore reported before the first chunk becomes durable.
    pub(crate) fn validate_operations(
        &self,
        operations: &[Operation],
        namespace_id: &str,
    ) -> Result<(), ApplyError> {
        let _staged = self.stage_operations(operations, namespace_id)?;
        Ok(())
    }

    /// Applies every operation in `transaction` atomically: either every
    /// operation succeeds and is committed, or the table is left completely
    /// unchanged.
    ///
    /// `namespace_id` is the exact `checkpoint.id` this table (and the WAL
    /// generation `transaction` came from) belongs to; it is consulted only
    /// by an administrative `remove_file`.
    ///
    /// Atomicity is achieved with a bounded scratch map holding at most one
    /// entry per distinct `file_id` this transaction's operations touch
    /// (never a clone of the whole table): every operation is validated and
    /// applied against that scratch map first, and only once every
    /// operation in the transaction has succeeded are the touched entries
    /// written back into `self`. If any operation fails, `self` is
    /// returned untouched because it was never written to during the
    /// trial. This keeps `apply_transaction`'s cost proportional to the
    /// number of operations in the transaction, not to the size of the
    /// table.
    pub fn apply_transaction(
        &mut self,
        transaction: &Transaction,
        namespace_id: &str,
    ) -> Result<(), ApplyError> {
        let staged = self.stage_operations(&transaction.operations, namespace_id)?;
        self.commit_staged(staged);
        Ok(())
    }

    /// Replays every transaction in `transactions`, in order, via
    /// [`Self::apply_transaction`].
    pub fn replay(
        &mut self,
        transactions: &[Transaction],
        namespace_id: &str,
    ) -> Result<(), ApplyError> {
        for transaction in transactions {
            self.apply_transaction(transaction, namespace_id)?;
        }
        Ok(())
    }

    /// Returns the touched-map slot for `file_id`.
    ///
    /// Panics only if called for a `file_id` that `apply_transaction` did
    /// not first populate for every operation in the transaction being
    /// applied; that population always happens immediately before this
    /// function is ever called, so this is an internal control-flow
    /// invariant of this module, never a condition an external WAL byte
    /// stream or a caller-supplied record can trigger.
    fn slot(
        table: &mut HashMap<FileId, Option<TableRecord>>,
        file_id: FileId,
    ) -> &mut Option<TableRecord> {
        table
            .get_mut(&file_id)
            .expect("apply_transaction pre-populates a slot for every operation's file_id")
    }

    fn apply_operation(
        table: &mut HashMap<FileId, Option<TableRecord>>,
        operation: &Operation,
        namespace_id: &str,
    ) -> Result<(), ApplyError> {
        match operation {
            Operation::RegisterFile(op) => {
                // These two preconditions are validated unconditionally,
                // before branching on whether a record already exists,
                // so a stale/invalid `file_epoch` or `framing_resume` can
                // never be waved through as a "benign identical replay"
                // just because an equally invalid prior record happens to
                // match it field-for-field.
                if op.file_epoch != 1 {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "register_file",
                        file_id: op.file_id,
                        reason: "file_epoch must be 1 at registration",
                    });
                }
                if op.framing_resume != FramingResume::Clean {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "register_file",
                        file_id: op.file_id,
                        reason: "framing_resume must be Clean at registration",
                    });
                }
                let slot = Self::slot(table, op.file_id);
                match slot {
                    None => {
                        *slot = Some(SnapshotRecord {
                            file_id: op.file_id,
                            file_epoch: op.file_epoch,
                            committed_offset: op.committed_offset,
                            fingerprint: op.fingerprint.clone(),
                            ignored_header_bytes: op.ignored_header_bytes,
                            locator: op.locator,
                            framing_profile_version: op.framing_profile_version,
                            framing_profile_digest: op.framing_profile_digest,
                            framing_resume: op.framing_resume,
                            lifecycle_state: LifecycleState::Active,
                            quarantine_evidence: None,
                            last_seen_time_unix_nano: op.last_seen_time_unix_nano,
                            advisory_path: op.advisory_path.clone(),
                        });
                    }
                    Some(existing) => {
                        let identical = existing.file_epoch == op.file_epoch
                            && existing.committed_offset == op.committed_offset
                            && existing.fingerprint == op.fingerprint
                            && existing.ignored_header_bytes == op.ignored_header_bytes
                            && existing.locator == op.locator
                            && existing.framing_profile_version == op.framing_profile_version
                            && existing.framing_profile_digest == op.framing_profile_digest
                            && existing.framing_resume == op.framing_resume
                            && existing.lifecycle_state == LifecycleState::Active
                            && existing.last_seen_time_unix_nano == op.last_seen_time_unix_nano
                            && existing.advisory_path == op.advisory_path;
                        if !identical {
                            return Err(ApplyError::ConflictingRegistration {
                                file_id: op.file_id,
                            });
                        }
                        // Benign replay of an already-durable registration: no-op.
                    }
                }
            }
            Operation::UpdateProgress(op) => {
                let record = Self::slot(table, op.file_id).as_mut().ok_or(
                    ApplyError::ImpossibleTransition {
                        operation: "update_progress",
                        file_id: op.file_id,
                        reason: "no record for file_id",
                    },
                )?;
                if record.lifecycle_state != LifecycleState::Active {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "update_progress",
                        file_id: op.file_id,
                        reason: "record is not Active",
                    });
                }
                if record.committed_offset != op.expected_committed_offset
                    || record.file_epoch != op.expected_file_epoch
                {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "update_progress",
                        file_id: op.file_id,
                        reason: "expected committed_offset or file_epoch mismatch",
                    });
                }
                if op.new_committed_offset < op.expected_committed_offset {
                    return Err(ApplyError::OffsetRegression {
                        file_id: op.file_id,
                        current: record.committed_offset,
                        attempted: op.new_committed_offset,
                    });
                }
                record.committed_offset = op.new_committed_offset;
                record.framing_resume = op.new_framing_resume;
                record.last_seen_time_unix_nano = op.new_last_seen_time_unix_nano;
                if op.finalize {
                    record.lifecycle_state = LifecycleState::RotatedFinalized;
                }
            }
            Operation::ResetAfterTruncate(op) => {
                let record = Self::slot(table, op.file_id).as_mut().ok_or(
                    ApplyError::ImpossibleTransition {
                        operation: "reset_after_truncate",
                        file_id: op.file_id,
                        reason: "no record for file_id",
                    },
                )?;
                if record.lifecycle_state != LifecycleState::Active {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "reset_after_truncate",
                        file_id: op.file_id,
                        reason: "record is not Active",
                    });
                }
                if record.file_epoch != op.expected_active_epoch {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "reset_after_truncate",
                        file_id: op.file_id,
                        reason: "expected_active_epoch mismatch",
                    });
                }
                let resulting_epoch =
                    op.expected_active_epoch
                        .checked_add(1)
                        .ok_or(ApplyError::EpochOverflow {
                            operation: "reset_after_truncate",
                            file_id: op.file_id,
                        })?;
                if op.resulting_epoch != resulting_epoch {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "reset_after_truncate",
                        file_id: op.file_id,
                        reason: "resulting_epoch must equal expected_active_epoch + 1",
                    });
                }
                if op.new_committed_offset != 0 {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "reset_after_truncate",
                        file_id: op.file_id,
                        reason: "new_committed_offset must be 0 in this version",
                    });
                }
                if op.new_framing_resume != FramingResume::Clean {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "reset_after_truncate",
                        file_id: op.file_id,
                        reason: "new_framing_resume must be Clean",
                    });
                }
                if op.reason_code != super::primitives::TRUNCATE_RESET_REASON_READ_NEW {
                    return Err(ApplyError::InvalidTruncateReason {
                        file_id: op.file_id,
                        reason_code: op.reason_code,
                    });
                }
                record.file_epoch = resulting_epoch;
                record.committed_offset = 0;
                record.framing_resume = FramingResume::Clean;
                record.last_seen_time_unix_nano = op.reset_time_unix_nano;
            }
            Operation::UpdateFingerprint(op) => {
                let record = Self::slot(table, op.file_id).as_mut().ok_or(
                    ApplyError::ImpossibleTransition {
                        operation: "update_fingerprint",
                        file_id: op.file_id,
                        reason: "no record for file_id",
                    },
                )?;
                if record.lifecycle_state != LifecycleState::Active {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "update_fingerprint",
                        file_id: op.file_id,
                        reason: "record is not Active",
                    });
                }
                if record.file_epoch != op.expected_file_epoch {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "update_fingerprint",
                        file_id: op.file_id,
                        reason: "expected_file_epoch mismatch",
                    });
                }
                if record.fingerprint != op.expected_fingerprint {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "update_fingerprint",
                        file_id: op.file_id,
                        reason: "expected_fingerprint does not match stored fingerprint",
                    });
                }
                record.fingerprint = op.new_fingerprint.clone();
            }
            Operation::UpdateMetadata(op) => {
                let record = Self::slot(table, op.file_id).as_mut().ok_or(
                    ApplyError::ImpossibleTransition {
                        operation: "update_metadata",
                        file_id: op.file_id,
                        reason: "no record for file_id",
                    },
                )?;
                match record.lifecycle_state {
                    LifecycleState::Active => {
                        if let Some(locator) = op.locator {
                            record.locator = locator;
                        }
                        if let Some(path) = &op.advisory_path {
                            record.advisory_path = path.clone();
                        }
                        record.last_seen_time_unix_nano = op.last_seen_time_unix_nano;
                    }
                    LifecycleState::Quarantined => {
                        // The immutable quarantine locator, lifecycle state,
                        // and failure evidence are never touched here, even
                        // when `op.locator` is `Some`.
                        if let Some(path) = &op.advisory_path {
                            record.advisory_path = path.clone();
                        }
                        record.last_seen_time_unix_nano = op.last_seen_time_unix_nano;
                    }
                    LifecycleState::RotatedFinalized => {
                        return Err(ApplyError::ImpossibleTransition {
                            operation: "update_metadata",
                            file_id: op.file_id,
                            reason: "a RotatedFinalized record's metadata is immutable",
                        });
                    }
                }
            }
            Operation::QuarantineFile(op) => {
                let record_slot = Self::slot(table, op.file_id);
                {
                    let existing =
                        record_slot
                            .as_ref()
                            .ok_or(ApplyError::ImpossibleTransition {
                                operation: "quarantine_file",
                                file_id: op.file_id,
                                reason: "no record for file_id",
                            })?;
                    match existing.lifecycle_state {
                        LifecycleState::Active => {
                            if existing.file_epoch != op.expected_file_epoch {
                                return Err(ApplyError::ImpossibleTransition {
                                    operation: "quarantine_file",
                                    file_id: op.file_id,
                                    reason: "expected_file_epoch mismatch",
                                });
                            }
                            if op.quarantine_epoch != op.expected_file_epoch {
                                return Err(ApplyError::ImpossibleTransition {
                                    operation: "quarantine_file",
                                    file_id: op.file_id,
                                    reason: "quarantine_epoch must equal expected_file_epoch",
                                });
                            }
                        }
                        LifecycleState::Quarantined => {
                            let evidence = existing.quarantine_evidence.as_ref().ok_or(
                                ApplyError::MissingQuarantineEvidence {
                                    operation: "quarantine_file",
                                    file_id: op.file_id,
                                },
                            )?;
                            let identical = existing.locator == op.locator
                                && evidence.quarantine_epoch == op.quarantine_epoch
                                && evidence.reason_code == op.reason_code
                                && evidence.observed_size == op.observed_size
                                && evidence.quarantine_time_unix_nano
                                    == op.quarantine_time_unix_nano;
                            if !identical {
                                return Err(ApplyError::ConflictingQuarantine {
                                    file_id: op.file_id,
                                });
                            }
                            return Ok(()); // Benign replay of an identical quarantine: no-op.
                        }
                        LifecycleState::RotatedFinalized => {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "quarantine_file",
                                file_id: op.file_id,
                                reason: "a RotatedFinalized record cannot be quarantined",
                            });
                        }
                    }
                }
                let record = record_slot
                    .as_mut()
                    .expect("presence already checked above");
                record.lifecycle_state = LifecycleState::Quarantined;
                record.locator = op.locator;
                record.quarantine_evidence = Some(QuarantineEvidence {
                    reason_code: op.reason_code,
                    observed_size: op.observed_size,
                    quarantine_epoch: op.quarantine_epoch,
                    quarantine_time_unix_nano: op.quarantine_time_unix_nano,
                });
            }
            Operation::ResetQuarantinedFile(op) => {
                let record_slot = Self::slot(table, op.file_id);
                let committed_offset;
                {
                    let existing =
                        record_slot
                            .as_ref()
                            .ok_or(ApplyError::ImpossibleTransition {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                                reason: "no record for file_id",
                            })?;
                    if existing.lifecycle_state != LifecycleState::Quarantined {
                        return Err(ApplyError::ImpossibleTransition {
                            operation: "reset_quarantined_file",
                            file_id: op.file_id,
                            reason: "record is not Quarantined",
                        });
                    }
                    let evidence = existing.quarantine_evidence.as_ref().ok_or(
                        ApplyError::MissingQuarantineEvidence {
                            operation: "reset_quarantined_file",
                            file_id: op.file_id,
                        },
                    )?;
                    if evidence.quarantine_epoch != op.expected_quarantine_epoch {
                        return Err(ApplyError::ImpossibleTransition {
                            operation: "reset_quarantined_file",
                            file_id: op.file_id,
                            reason: "expected_quarantine_epoch mismatch",
                        });
                    }
                    committed_offset = existing.committed_offset;
                }
                match op.action {
                    ResetQuarantineAction::KeepFailed => {
                        if op.resulting_epoch != op.expected_quarantine_epoch {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                                reason: "keep_failed must not change the quarantine epoch",
                            });
                        }
                        if op.resulting_offset != committed_offset {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                                reason: "keep_failed must not change the committed offset",
                            });
                        }
                        return Ok(()); // Stays Quarantined; audit-only.
                    }
                    ResetQuarantineAction::ResetToBeginning => {
                        let resulting_epoch = op.expected_quarantine_epoch.checked_add(1).ok_or(
                            ApplyError::EpochOverflow {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                            },
                        )?;
                        if op.resulting_epoch != resulting_epoch {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                                reason: "resulting_epoch must equal expected_quarantine_epoch + 1",
                            });
                        }
                        if op.resulting_offset != 0 {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                                reason: "reset_to_beginning requires resulting_offset == 0",
                            });
                        }
                    }
                    ResetQuarantineAction::ResetToEnd => {
                        let resulting_epoch = op.expected_quarantine_epoch.checked_add(1).ok_or(
                            ApplyError::EpochOverflow {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                            },
                        )?;
                        if op.resulting_epoch != resulting_epoch {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "reset_quarantined_file",
                                file_id: op.file_id,
                                reason: "resulting_epoch must equal expected_quarantine_epoch + 1",
                            });
                        }
                        // `resulting_offset` is accepted as given.
                    }
                }
                if op.new_framing_resume != FramingResume::Clean {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "reset_quarantined_file",
                        file_id: op.file_id,
                        reason: "new_framing_resume must be Clean for a reset action",
                    });
                }
                let record = record_slot
                    .as_mut()
                    .expect("presence already checked above");
                record.lifecycle_state = LifecycleState::Active;
                record.file_epoch = op.resulting_epoch;
                record.committed_offset = op.resulting_offset;
                record.framing_resume = FramingResume::Clean;
                record.last_seen_time_unix_nano = op.reset_time_unix_nano;
                record.quarantine_evidence = None;
            }
            Operation::RemoveFile(op) => {
                let record_slot = Self::slot(table, op.file_id);
                let existing = match record_slot.as_ref() {
                    None => return Ok(()), // Idempotent: already absent.
                    Some(existing) => existing,
                };
                if existing.lifecycle_state != op.expected_prior_state {
                    return Err(ApplyError::ImpossibleTransition {
                        operation: "remove_file",
                        file_id: op.file_id,
                        reason: "expected_prior_state mismatch",
                    });
                }
                match existing.lifecycle_state {
                    LifecycleState::Active | LifecycleState::RotatedFinalized => {
                        if existing.file_epoch != op.expected_file_epoch {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "remove_file",
                                file_id: op.file_id,
                                reason: "expected_file_epoch mismatch",
                            });
                        }
                    }
                    LifecycleState::Quarantined => {
                        let evidence = existing.quarantine_evidence.as_ref().ok_or(
                            ApplyError::MissingQuarantineEvidence {
                                operation: "remove_file",
                                file_id: op.file_id,
                            },
                        )?;
                        if evidence.quarantine_epoch != op.expected_file_epoch {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "remove_file",
                                file_id: op.file_id,
                                reason: "expected_file_epoch (quarantine_epoch) mismatch",
                            });
                        }
                        if !op.administrative {
                            return Err(ApplyError::ImpossibleTransition {
                                operation: "remove_file",
                                file_id: op.file_id,
                                reason: "ordinary retention cannot remove quarantined state",
                            });
                        }
                    }
                }
                // Namespace validation applies whenever this removal is
                // administrative, regardless of the record's lifecycle
                // state -- not only when removing a Quarantined record --
                // so an administrative removal recorded against the wrong
                // namespace can never silently succeed against an Active or
                // RotatedFinalized record either.
                if op.administrative {
                    let named_namespace = op.namespace_id.as_deref().unwrap_or_default();
                    if named_namespace != namespace_id {
                        return Err(ApplyError::NamespaceMismatch {
                            file_id: op.file_id,
                            named: named_namespace.to_owned(),
                            actual: namespace_id.to_owned(),
                        });
                    }
                }
                *record_slot = None;
            }
        }
        Ok(())
    }
}
