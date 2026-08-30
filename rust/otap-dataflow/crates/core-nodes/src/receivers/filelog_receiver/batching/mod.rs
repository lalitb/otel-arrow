// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Exact OTAP projection and bounded Phase 1 batch construction.
//!
//! A refused record is deliberately returned unchanged. Stage 11 must drop
//! that speculative frame together with every decoder/framer state derived
//! after the retained batch frontier, rewind readers, and reconstruct after
//! Ack. Holding the refused record across the one-batch in-flight window
//! would violate Phase 1's strict no-read-ahead contract.

use std::collections::{HashMap, TryReserveError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::error::ArrowError;
use otap_df_pdata::{
    encode::record::{
        attributes::StrKeysAttributesRecordBatchBuilder, logs::LogsRecordBatchBuilder,
    },
    otap::{Logs, OtapArrowRecords},
    proto::opentelemetry::arrow::v1::ArrowPayloadType,
};
use thiserror::Error;

use super::checkpoint::primitives::WAL_MAX_OPS_PER_TX;
use super::checkpoint::wal::UpdateProgress;
use super::checkpoint::{CommittedFrontierGuard, CommittedFrontierWindow, FileId, FramingResume};
use super::config::{
    ATTR_KEY_DECODE_ERROR_COUNT, ATTR_KEY_DECODE_ERROR_POLICY, ATTR_KEY_FLUSH_REASON,
    ATTR_KEY_FRAGMENT_ID, ATTR_KEY_FRAGMENT_INDEX, ATTR_KEY_FRAGMENT_LAST,
    ATTR_KEY_FRAGMENT_SOURCE_END, ATTR_KEY_FRAGMENT_SOURCE_START, ATTR_KEY_LOG_FILE_NAME,
    ATTR_KEY_LOG_FILE_PATH, ATTR_KEY_PATH_ENCODING, ATTR_KEY_PATH_RESOLVED, ATTR_KEY_RECORD_NUMBER,
    ATTR_KEY_RECORD_OFFSET, ATTR_KEY_RECORD_TRUNCATED, ATTR_KEY_TERMINAL_UNTERMINATED,
    ENCODED_PATH_DISCRIMINATOR, ENCODED_PATH_PREFIX, LogicalAttributeSize, LogicalSizeError,
    MetadataConfig, RuntimeConfig, checked_logical_record_size, logical_bool_value_len,
    logical_int_value_len, logical_string_value_len,
};
use super::framing::{
    DecodeOutcome, FlushReason, FragmentMetadata, FramedBody, FramedRecord, fragment_id,
};
use super::identity::IdentityError;
use super::identity::platform::native_path_bytes;
use super::{MaxLogSizeBehavior, OnDecodeError};

#[cfg(test)]
use super::config::BatchConfig;

const SCOPE_NAME: &[u8] = b"otap-df-core-nodes/filelog";
const MAX_DISTINCT_DELTAS: usize = WAL_MAX_OPS_PER_TX as usize;
const MAX_PREPARED_ATTRIBUTES: usize = 15;

/// The durable progress state from which one speculative framing pass began.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProgressBase {
    /// Durable file epoch.
    pub(crate) file_epoch: u32,
    /// Ack-gated source offset.
    pub(crate) committed_offset: u64,
    /// Durable framing state paired atomically with the offset.
    pub(crate) framing_resume: FramingResume,
    /// Latest last-seen metadata already persisted for this identity.
    pub(crate) last_seen_time_unix_nano: u64,
    /// Durable committed-frontier guard paired atomically with the offset.
    /// Reused verbatim by a zero-delta or finalize-only update instead of
    /// being recomputed, since the source offset does not change.
    pub(crate) committed_frontier_guard: CommittedFrontierGuard,
}

/// One current source frontier, which may still be provisional.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProgressFrontier {
    /// File epoch owning the frontier.
    pub(crate) file_epoch: u32,
    /// Source offset at the frontier.
    pub(crate) offset: u64,
    /// Framing state paired with the offset.
    pub(crate) framing_resume: FramingResume,
}

/// One owned frame plus all projection and progress evidence.
///
/// Finalization is intentionally not a required record field. Lifecycle-only
/// finalization uses [`OpenBatch::finalize_file`] and never synthesizes a log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecordInput {
    /// Framed body and source ranges.
    pub(crate) framed: FramedRecord,
    /// Opaque durable identity, used only for progress.
    pub(crate) file_id: FileId,
    /// Explicit durable epoch/offset/resume/metadata state used to construct
    /// this frame.
    pub(crate) progress_base: ProgressBase,
    /// Operator-visible path matched by discovery.
    pub(crate) matched_path: PathBuf,
    /// Canonical target path opened by discovery.
    pub(crate) resolved_path: PathBuf,
    /// Per-record observed timestamp; negative Unix timestamps are rejected.
    pub(crate) observed_time_unix_nano: i64,
    /// Independent monotonic metadata timestamp used only by checkpointing.
    pub(crate) last_seen_time_unix_nano: u64,
    /// Monotonic worker clock instant at which the frame became ready.
    pub(crate) ready_at: Instant,
    /// Optional zero-based worker-local number supplied by
    /// [`RecordNumberTable`].
    pub(crate) record_number: Option<u64>,
}

/// The evidence source for one delta's resulting committed-frontier guard.
#[derive(Clone, Debug, Eq, PartialEq)]
enum DeltaGuardSource {
    /// The source offset does not change (a recordless finalize, or a
    /// zero-delta update): the durable guard is reused verbatim rather than
    /// recomputed.
    Unchanged,
    /// Real progress happened: the exact real committed-frontier window
    /// ending at the delta's `final_offset`, already owned by the
    /// reader/framer pipeline that produced it.
    Window(CommittedFrontierWindow),
}

/// One file's complete Ack transaction contribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProgressDelta {
    file_id: FileId,
    expected_file_epoch: u32,
    expected_committed_offset: u64,
    expected_framing_resume: FramingResume,
    final_offset: u64,
    final_framing_resume: FramingResume,
    final_guard_source: DeltaGuardSource,
    last_seen_time_unix_nano: u64,
    finalize: bool,
}

impl ProgressDelta {
    /// Builds recordless clean progress used only to finalize framing-only
    /// source bytes, such as a stripped BOM at permanent EOF.
    pub(crate) fn terminal_empty_finalization(
        file_id: FileId,
        base: ProgressBase,
        final_offset: u64,
        final_window: CommittedFrontierWindow,
        last_seen_time_unix_nano: u64,
    ) -> Result<Self, BatchError> {
        if final_offset <= base.committed_offset
            || base.framing_resume != FramingResume::Clean
            || final_window.end_offset() != final_offset
        {
            return Err(BatchError::InvalidProgress {
                file_id,
                reason: "terminal empty progress requires an advancing clean frontier window",
            });
        }
        Ok(Self {
            file_id,
            expected_file_epoch: base.file_epoch,
            expected_committed_offset: base.committed_offset,
            expected_framing_resume: base.framing_resume,
            final_offset,
            final_framing_resume: FramingResume::Clean,
            final_guard_source: DeltaGuardSource::Window(final_window),
            last_seen_time_unix_nano,
            finalize: true,
        })
    }

    /// Durable identity updated by this delta.
    pub(crate) const fn file_id(&self) -> FileId {
        self.file_id
    }

    /// Epoch that must still be active when Ack is committed.
    pub(crate) const fn expected_file_epoch(&self) -> u32 {
        self.expected_file_epoch
    }

    /// Durable offset expected when Ack is committed.
    pub(crate) const fn expected_committed_offset(&self) -> u64 {
        self.expected_committed_offset
    }

    /// Durable framing state expected when Ack is committed.
    pub(crate) const fn expected_framing_resume(&self) -> FramingResume {
        self.expected_framing_resume
    }

    /// Resulting source offset.
    pub(crate) const fn final_offset(&self) -> u64 {
        self.final_offset
    }

    /// Resulting durable framing state.
    pub(crate) const fn final_framing_resume(&self) -> FramingResume {
        self.final_framing_resume
    }

    /// The exact real committed-frontier window resulting from this delta,
    /// once its checkpoint operation has been durably applied.
    ///
    /// `None` means the source offset did not change (a zero-delta or
    /// finalize-only update): the reader's already-retained window remains
    /// correct bit-for-bit and must not be replaced. `Some` carries the
    /// exact window the framing pipeline already owns for `final_offset`,
    /// never a fabricated or reread substitute.
    pub(crate) fn final_window(&self) -> Option<&CommittedFrontierWindow> {
        match &self.final_guard_source {
            DeltaGuardSource::Unchanged => None,
            DeltaGuardSource::Window(window) => Some(window),
        }
    }

    /// Greatest last-seen timestamp represented by merged records.
    pub(crate) const fn last_seen_time_unix_nano(&self) -> u64 {
        self.last_seen_time_unix_nano
    }

    /// Whether Ack also finalizes this rotated identity.
    pub(crate) const fn finalize(&self) -> bool {
        self.finalize
    }

    /// Converts this validated delta to the checkpoint operation committed by
    /// Stage 11.
    pub(crate) fn to_update_progress(
        &self,
        current: ProgressBase,
    ) -> Result<UpdateProgress, BatchError> {
        if current.file_epoch != self.expected_file_epoch
            || current.committed_offset != self.expected_committed_offset
            || current.framing_resume != self.expected_framing_resume
        {
            return Err(BatchError::InvalidProgress {
                file_id: self.file_id,
                reason: "current durable progress does not match the delta base",
            });
        }
        if self.final_offset < self.expected_committed_offset {
            return Err(BatchError::InvalidProgress {
                file_id: self.file_id,
                reason: "final offset regresses below the durable base",
            });
        }
        let new_committed_frontier_guard = match &self.final_guard_source {
            // The offset does not change: the durable guard is reused
            // verbatim, never recomputed, matching the format's zero-delta
            // invariant that the guard is repeated bit-for-bit.
            DeltaGuardSource::Unchanged => current.committed_frontier_guard,
            // Real progress: the exact real window the reader/framer
            // pipeline already owns for this new offset.
            DeltaGuardSource::Window(window) => {
                if window.end_offset() != self.final_offset {
                    return Err(BatchError::InvalidProgress {
                        file_id: self.file_id,
                        reason: "retained committed-frontier window does not end at final_offset",
                    });
                }
                window.guard().map_err(|_| BatchError::InvalidProgress {
                    file_id: self.file_id,
                    reason: "retained committed-frontier window is not a valid guard",
                })?
            }
        };
        Ok(UpdateProgress {
            file_id: self.file_id,
            expected_committed_offset: self.expected_committed_offset,
            expected_file_epoch: self.expected_file_epoch,
            new_committed_offset: self.final_offset,
            new_committed_frontier_guard,
            new_framing_resume: self.final_framing_resume,
            new_last_seen_time_unix_nano: self
                .last_seen_time_unix_nano
                .max(current.last_seen_time_unix_nano),
            finalize: self.finalize,
        })
    }
}

/// Why an open batch must seal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealReason {
    /// Appending would exceed, or exactly reached, `batch.max_records`.
    RecordCount,
    /// Appending would exceed, or exactly reached, `batch.max_bytes`.
    LogicalBytes,
    /// Appending would exceed, or exactly reached, one WAL transaction's
    /// 4,096 distinct file operations.
    DistinctFiles,
    /// The first-record deadline was reached before this record became ready.
    Deadline,
}

/// Result of an otherwise valid append attempt.
//
// Keeping the refused record inline avoids a new fallible allocation at the
// exact backpressure boundary where ownership must be returned unchanged.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum BatchAppendOutcome {
    /// The frame was projected. A reason requests immediate sealing after the
    /// append; `None` permits more records.
    Appended { seal: Option<SealReason> },
    /// The nonempty batch must seal first. The owned record is byte-for-byte
    /// unchanged and must not be retained across the in-flight window.
    SealBefore {
        /// Refused input.
        record: RecordInput,
        /// Bound or deadline requiring the seal.
        reason: SealReason,
    },
}

/// Result of recordless rotation finalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FinalizationOutcome {
    /// Finalization was merged into the existing file delta.
    Merged,
    /// No record for the file exists in this batch. Stage 11 can commit this
    /// same-frontier delta directly without emitting an empty OTAP batch.
    Direct(ProgressDelta),
}

/// Structured projection, batching, and numbering failures.
#[derive(Debug, Error)]
pub(crate) enum BatchError {
    /// Independently constructed batch settings violate validated config.
    #[error("invalid filelog batch setting: {reason}")]
    InvalidSettings {
        /// Exact rejected invariant.
        reason: &'static str,
    },
    /// Preallocating one bounded collection failed.
    #[error("could not allocate bounded filelog batch resource '{resource}'")]
    AllocationFailed {
        /// Collection being reserved.
        resource: &'static str,
        /// Allocation failure.
        #[source]
        source: TryReserveError,
    },
    /// A path-shaped field could not satisfy the reversible bounded path
    /// contract.
    #[error("could not project filelog {field} path {path}: {source}")]
    Path {
        /// Stable field description.
        field: &'static str,
        /// Original path.
        path: PathBuf,
        /// Bounded native encoding failure.
        #[source]
        source: Box<IdentityError>,
    },
    /// The matched path has no terminal file-name component.
    #[error("filelog matched path has no file name: {path}")]
    MissingFileName {
        /// Rejected matched path.
        path: PathBuf,
    },
    /// Percent-output capacity arithmetic overflowed.
    #[error("filelog path percent encoding overflows addressable capacity")]
    PathEncodingOverflow,
    /// Shared logical-size arithmetic overflowed.
    #[error(transparent)]
    LogicalSize(#[from] LogicalSizeError),
    /// One valid projected record is larger than the whole batch budget.
    #[error("filelog record logical size {logical_bytes} exceeds batch.max_bytes {max_bytes}")]
    RecordTooLarge {
        /// Actual logical size.
        logical_bytes: u64,
        /// Configured batch maximum.
        max_bytes: u64,
    },
    /// A frame or its policy evidence is internally inconsistent.
    #[error("invalid filelog framed record for {file_id:?}: {reason}")]
    InvalidRecord {
        /// Durable identity supplying the frame.
        file_id: FileId,
        /// Exact invariant violation.
        reason: &'static str,
    },
    /// Progress cannot form one contiguous Ack delta.
    #[error("invalid filelog batch progress for {file_id:?}: {reason}")]
    InvalidProgress {
        /// Durable identity whose progress was rejected.
        file_id: FileId,
        /// Exact invariant violation.
        reason: &'static str,
    },
    /// Ready instants moved backward within one batch.
    #[error("filelog record ready_at is earlier than the previous batch record")]
    NonMonotonicReadyAt,
    /// First-record deadline cannot be represented by `Instant`.
    #[error("filelog batch deadline overflows Instant")]
    DeadlineOverflow,
    /// A bounded counter or byte sum exhausted its representation.
    #[error("filelog batch counter '{counter}' overflowed")]
    CounterOverflow {
        /// Counter that could not advance.
        counter: &'static str,
    },
    /// The current record count cannot be represented by the OTAP `u16` ID.
    #[error("filelog log ID for record count {record_count} exceeds the u16 batch contract")]
    InvalidLogId {
        /// Zero-based ID candidate.
        record_count: u32,
    },
    /// `finish` was requested before any record was appended.
    #[error("filelog cannot finish an empty logical batch")]
    EmptyBatch,
    /// A bounded record-number table has no slot for another identity.
    #[error("filelog record-number table is full at {max_files} files")]
    RecordNumberCapacityExhausted {
        /// Configured table maximum.
        max_files: usize,
    },
    /// A record-number reservation no longer matches table state.
    #[error("stale filelog record-number reservation for {file_id:?}")]
    StaleRecordNumberReservation {
        /// Identity named by the stale reservation.
        file_id: FileId,
    },
    /// A file's worker-local record number exhausted `u64`.
    #[error("filelog record number overflowed for {file_id:?}")]
    RecordNumberOverflow {
        /// Identity whose next number cannot advance.
        file_id: FileId,
    },
    /// Canonical Arrow construction failed. Finishing is terminal.
    #[error("could not finish filelog Arrow projection: {0}")]
    Arrow(#[from] ArrowError),
    /// OTAP schema validation failed. Finishing is terminal.
    #[error("could not validate filelog OTAP projection: {0}")]
    Otap(#[from] otap_df_pdata::error::Error),
    /// Internal bounded indexes no longer agree.
    #[error("filelog open-batch integrity check failed: {reason}")]
    Inconsistent {
        /// Exact failed invariant.
        reason: &'static str,
    },
}

/// Runtime settings copied from validated receiver configuration.
#[derive(Clone, Debug)]
pub(crate) struct BatchSettings {
    max_records: u32,
    max_bytes: u64,
    max_flush_period: Duration,
    metadata: MetadataConfig,
    oversize_behavior: MaxLogSizeBehavior,
    decode_policy: OnDecodeError,
}

impl BatchSettings {
    fn from_runtime(config: &RuntimeConfig) -> Self {
        Self {
            max_records: config.batch.max_records,
            max_bytes: config.batch.max_bytes,
            max_flush_period: config.batch.max_flush_period,
            metadata: config.metadata,
            oversize_behavior: config.framing.max_log_size_behavior,
            decode_policy: config.on_decode_error,
        }
    }

    #[cfg(test)]
    fn for_test(
        batch: BatchConfig,
        metadata: MetadataConfig,
        oversize_behavior: MaxLogSizeBehavior,
        decode_policy: OnDecodeError,
    ) -> Self {
        Self {
            max_records: batch.max_records,
            max_bytes: batch.max_bytes,
            max_flush_period: batch.max_flush_period,
            metadata,
            oversize_behavior,
            decode_policy,
        }
    }
}

#[derive(Debug)]
enum PreparedValue {
    String(String),
    StaticString(&'static str),
    Int(i64),
    Bool(bool),
}

impl PreparedValue {
    fn logical_len(&self) -> Result<u64, LogicalSizeError> {
        match self {
            Self::String(value) => logical_string_value_len(value),
            Self::StaticString(value) => logical_string_value_len(value),
            Self::Int(value) => Ok(logical_int_value_len(*value)),
            Self::Bool(value) => Ok(logical_bool_value_len(*value)),
        }
    }

    fn append_to(&self, attributes: &mut StrKeysAttributesRecordBatchBuilder<u16>) {
        match self {
            Self::String(value) => attributes.any_values_builder.append_str(value.as_bytes()),
            Self::StaticString(value) => attributes.any_values_builder.append_str(value.as_bytes()),
            Self::Int(value) => attributes.any_values_builder.append_int(*value),
            Self::Bool(value) => attributes.any_values_builder.append_bool(*value),
        }
    }
}

#[derive(Debug)]
struct PreparedAttribute {
    key: &'static str,
    value: PreparedValue,
    logical_size: LogicalAttributeSize,
}

impl PreparedAttribute {
    fn new(key: &'static str, value: PreparedValue) -> Result<Self, LogicalSizeError> {
        let value_bytes = value.logical_len()?;
        Ok(Self {
            key,
            value,
            logical_size: LogicalAttributeSize::new(key, value_bytes)?,
        })
    }
}

#[derive(Debug)]
struct PreparedPath {
    value: String,
    encoded: bool,
}

#[derive(Debug)]
enum DeltaPlan {
    New(ProgressDelta),
    Merge {
        index: usize,
        final_offset: u64,
        final_framing_resume: FramingResume,
        final_window: CommittedFrontierWindow,
        last_seen_time_unix_nano: u64,
    },
}

impl DeltaPlan {
    const fn adds_distinct_file(&self) -> bool {
        matches!(self, Self::New(_))
    }
}

/// One mutable, bounded batch under construction.
pub(crate) struct OpenBatch {
    settings: BatchSettings,
    record_count: u32,
    logical_bytes: u64,
    source_bytes: u64,
    deadline: Option<Instant>,
    last_ready_at: Option<Instant>,
    logs: LogsRecordBatchBuilder,
    attributes: StrKeysAttributesRecordBatchBuilder<u16>,
    deltas: Vec<ProgressDelta>,
    delta_index: HashMap<FileId, usize>,
    seal_after_append: Option<SealReason>,
}

impl OpenBatch {
    /// Creates a batch from validated receiver configuration.
    pub(crate) fn new(config: &RuntimeConfig) -> Result<Self, BatchError> {
        Self::from_settings(BatchSettings::from_runtime(config))
    }

    fn from_settings(settings: BatchSettings) -> Result<Self, BatchError> {
        if settings.max_records == 0 || settings.max_records > u32::from(u16::MAX) {
            return Err(BatchError::InvalidSettings {
                reason: "max_records must be in 1..=65535",
            });
        }
        if settings.max_bytes == 0 {
            return Err(BatchError::InvalidSettings {
                reason: "max_bytes must be greater than zero",
            });
        }
        if settings.max_flush_period.is_zero() {
            return Err(BatchError::InvalidSettings {
                reason: "max_flush_period must be greater than zero",
            });
        }

        let reserve = usize::try_from(settings.max_records)
            .map_err(|_| BatchError::InvalidSettings {
                reason: "max_records must fit usize",
            })?
            .min(MAX_DISTINCT_DELTAS);
        let mut deltas = Vec::new();
        deltas
            .try_reserve_exact(reserve)
            .map_err(|source| BatchError::AllocationFailed {
                resource: "progress delta vector",
                source,
            })?;
        let mut delta_index = HashMap::new();
        delta_index
            .try_reserve(reserve)
            .map_err(|source| BatchError::AllocationFailed {
                resource: "progress delta index",
                source,
            })?;

        Ok(Self {
            settings,
            record_count: 0,
            logical_bytes: 0,
            source_bytes: 0,
            deadline: None,
            last_ready_at: None,
            logs: LogsRecordBatchBuilder::new(),
            attributes: StrKeysAttributesRecordBatchBuilder::new(),
            deltas,
            delta_index,
            seal_after_append: None,
        })
    }

    /// Number of projected records, stored wider than the Arrow ID.
    pub(crate) const fn record_count(&self) -> u32 {
        self.record_count
    }

    /// Exact shared logical-size sum for projected records.
    pub(crate) const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    /// First-record deadline. It is never rearmed.
    pub(crate) const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Returns the provisional progress frontier already represented by this
    /// batch for one file.
    pub(crate) fn progress_frontier(&self, file_id: FileId) -> Option<ProgressFrontier> {
        let index = self.delta_index.get(&file_id).copied()?;
        let delta = self.deltas.get(index)?;
        Some(ProgressFrontier {
            file_epoch: delta.expected_file_epoch,
            offset: delta.final_offset,
            framing_resume: delta.final_framing_resume,
        })
    }

    /// Whether the open batch owns unresolved progress for `file_id`.
    pub(crate) fn contains_file(&self, file_id: FileId) -> bool {
        self.delta_index.contains_key(&file_id)
    }

    /// Whether a sparse nonempty batch must flush at `now`.
    pub(crate) fn is_flush_due(&self, now: Instant) -> bool {
        self.record_count != 0 && self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Validates and appends one record, or returns it unchanged when the
    /// existing nonempty batch must seal first.
    pub(crate) fn try_append(
        &mut self,
        record: RecordInput,
    ) -> Result<BatchAppendOutcome, BatchError> {
        validate_record(&record, &self.settings)?;
        let attributes = prepare_attributes(&record, &self.settings)?;
        let body_bytes = match &record.framed.body {
            FramedBody::Text(body) => {
                u64::try_from(body.len()).map_err(|_| LogicalSizeError::Overflow)?
            }
            FramedBody::Bytes(body) => {
                u64::try_from(body.len()).map_err(|_| LogicalSizeError::Overflow)?
            }
        };
        let record_logical_bytes = checked_logical_record_size(
            body_bytes,
            attributes.iter().map(|attribute| attribute.logical_size),
        )?;
        let delta_plan = self.prepare_delta(&record)?;

        if record.observed_time_unix_nano < 0 {
            return Err(BatchError::InvalidRecord {
                file_id: record.file_id,
                reason: "observed_time_unix_nano must be nonnegative",
            });
        }
        if self
            .last_ready_at
            .is_some_and(|last_ready_at| record.ready_at < last_ready_at)
        {
            return Err(BatchError::NonMonotonicReadyAt);
        }
        let deadline = match self.deadline {
            Some(deadline) => deadline,
            None => record
                .ready_at
                .checked_add(self.settings.max_flush_period)
                .ok_or(BatchError::DeadlineOverflow)?,
        };
        if record_logical_bytes > self.settings.max_bytes {
            return Err(BatchError::RecordTooLarge {
                logical_bytes: record_logical_bytes,
                max_bytes: self.settings.max_bytes,
            });
        }
        let next_count = self
            .record_count
            .checked_add(1)
            .ok_or(BatchError::CounterOverflow {
                counter: "record count",
            })?;
        let next_bytes = self.logical_bytes.checked_add(record_logical_bytes).ok_or(
            BatchError::CounterOverflow {
                counter: "logical bytes",
            },
        )?;
        let record_source_bytes = record
            .framed
            .frame_source_range
            .end
            .checked_sub(record.framed.frame_source_range.start)
            .ok_or(BatchError::InvalidRecord {
                file_id: record.file_id,
                reason: "frame source range regressed",
            })?;
        let next_source_bytes = self.source_bytes.checked_add(record_source_bytes).ok_or(
            BatchError::CounterOverflow {
                counter: "source bytes",
            },
        )?;

        if self.record_count != 0 {
            if let Some(reason) = self.seal_after_append {
                return Ok(BatchAppendOutcome::SealBefore { record, reason });
            }
            if next_count > self.settings.max_records {
                return Ok(BatchAppendOutcome::SealBefore {
                    record,
                    reason: SealReason::RecordCount,
                });
            }
            if next_bytes > self.settings.max_bytes {
                return Ok(BatchAppendOutcome::SealBefore {
                    record,
                    reason: SealReason::LogicalBytes,
                });
            }
            if delta_plan.adds_distinct_file() && self.deltas.len() >= MAX_DISTINCT_DELTAS {
                return Ok(BatchAppendOutcome::SealBefore {
                    record,
                    reason: SealReason::DistinctFiles,
                });
            }
            if record.ready_at >= deadline {
                return Ok(BatchAppendOutcome::SealBefore {
                    record,
                    reason: SealReason::Deadline,
                });
            }
        }

        let log_id = log_id_for_count(self.record_count)?;
        append_projection(
            &mut self.logs,
            &mut self.attributes,
            log_id,
            &record,
            &attributes,
        );
        self.apply_delta(delta_plan);
        self.record_count = next_count;
        self.logical_bytes = next_bytes;
        self.source_bytes = next_source_bytes;
        self.deadline = Some(deadline);
        self.last_ready_at = Some(record.ready_at);

        let delta_count = self.deltas.len();
        let seal = if next_count == self.settings.max_records {
            Some(SealReason::RecordCount)
        } else if next_bytes == self.settings.max_bytes {
            Some(SealReason::LogicalBytes)
        } else if delta_count == MAX_DISTINCT_DELTAS {
            Some(SealReason::DistinctFiles)
        } else {
            None
        };
        self.seal_after_append = seal;
        Ok(BatchAppendOutcome::Appended { seal })
    }

    fn prepare_delta(&self, record: &RecordInput) -> Result<DeltaPlan, BatchError> {
        let frame = record.framed.frame_source_range;
        if record.framed.checkpoint_end != frame.end {
            return Err(BatchError::InvalidProgress {
                file_id: record.file_id,
                reason: "checkpoint_end does not equal frame_source_range.end",
            });
        }
        if let Some(index) = self.delta_index.get(&record.file_id).copied() {
            let delta = self.deltas.get(index).ok_or(BatchError::Inconsistent {
                reason: "delta index points beyond the delta vector",
            })?;
            if delta.finalize {
                return Err(BatchError::InvalidProgress {
                    file_id: record.file_id,
                    reason: "a record cannot append after finalization",
                });
            }
            if delta.expected_file_epoch != record.progress_base.file_epoch {
                return Err(BatchError::InvalidProgress {
                    file_id: record.file_id,
                    reason: "file epoch changed inside one batch",
                });
            }
            if delta.expected_committed_offset != record.progress_base.committed_offset
                || delta.expected_framing_resume != record.progress_base.framing_resume
            {
                return Err(BatchError::InvalidProgress {
                    file_id: record.file_id,
                    reason: "durable progress base changed inside one batch",
                });
            }
            if delta.final_offset != frame.start {
                return Err(BatchError::InvalidProgress {
                    file_id: record.file_id,
                    reason: "frame does not begin at the previous final offset",
                });
            }
            if frame.end < delta.final_offset {
                return Err(BatchError::InvalidProgress {
                    file_id: record.file_id,
                    reason: "frame source progress regresses",
                });
            }
            validate_resume_transition(record, delta.final_framing_resume)?;
            return Ok(DeltaPlan::Merge {
                index,
                final_offset: frame.end,
                final_framing_resume: record.framed.resulting_resume,
                final_window: record.framed.checkpoint_window.clone(),
                last_seen_time_unix_nano: delta
                    .last_seen_time_unix_nano
                    .max(record.progress_base.last_seen_time_unix_nano)
                    .max(record.last_seen_time_unix_nano),
            });
        }

        if record.progress_base.committed_offset != frame.start {
            return Err(BatchError::InvalidProgress {
                file_id: record.file_id,
                reason: "first frame does not begin at the durable progress base",
            });
        }
        if frame.end < record.progress_base.committed_offset {
            return Err(BatchError::InvalidProgress {
                file_id: record.file_id,
                reason: "first frame source progress regresses",
            });
        }
        validate_resume_transition(record, record.progress_base.framing_resume)?;
        Ok(DeltaPlan::New(ProgressDelta {
            file_id: record.file_id,
            expected_file_epoch: record.progress_base.file_epoch,
            expected_committed_offset: record.progress_base.committed_offset,
            expected_framing_resume: record.progress_base.framing_resume,
            final_offset: frame.end,
            final_framing_resume: record.framed.resulting_resume,
            final_guard_source: DeltaGuardSource::Window(record.framed.checkpoint_window.clone()),
            last_seen_time_unix_nano: record
                .progress_base
                .last_seen_time_unix_nano
                .max(record.last_seen_time_unix_nano),
            finalize: false,
        }))
    }

    fn apply_delta(&mut self, plan: DeltaPlan) {
        match plan {
            DeltaPlan::New(delta) => {
                let index = self.deltas.len();
                let prior = self.delta_index.insert(delta.file_id, index);
                debug_assert!(
                    prior.is_none(),
                    "a prevalidated new delta cannot already have an index"
                );
                self.deltas.push(delta);
            }
            DeltaPlan::Merge {
                index,
                final_offset,
                final_framing_resume,
                final_window,
                last_seen_time_unix_nano,
            } => {
                // `prepare_delta` obtained this index from the same immutable
                // map immediately before Arrow mutation. No code between
                // preparation and this point can change either collection.
                let delta = self
                    .deltas
                    .get_mut(index)
                    .expect("prevalidated delta index must remain valid");
                delta.final_offset = final_offset;
                delta.final_framing_resume = final_framing_resume;
                delta.final_guard_source = DeltaGuardSource::Window(final_window);
                delta.last_seen_time_unix_nano = last_seen_time_unix_nano;
            }
        }
    }

    /// Adds lifecycle finalization without creating a record.
    pub(crate) fn finalize_file(
        &mut self,
        file_id: FileId,
        frontier: ProgressFrontier,
        last_seen_time_unix_nano: u64,
    ) -> Result<FinalizationOutcome, BatchError> {
        if let Some(index) = self.delta_index.get(&file_id).copied() {
            let delta = self.deltas.get_mut(index).ok_or(BatchError::Inconsistent {
                reason: "finalization index points beyond the delta vector",
            })?;
            if delta.expected_file_epoch != frontier.file_epoch {
                return Err(BatchError::InvalidProgress {
                    file_id,
                    reason: "finalization epoch does not match the batch delta",
                });
            }
            if delta.final_offset != frontier.offset
                || delta.final_framing_resume != frontier.framing_resume
            {
                return Err(BatchError::InvalidProgress {
                    file_id,
                    reason: "finalization frontier does not match the batch delta",
                });
            }
            delta.last_seen_time_unix_nano =
                delta.last_seen_time_unix_nano.max(last_seen_time_unix_nano);
            delta.finalize = true;
            return Ok(FinalizationOutcome::Merged);
        }

        // No record advanced this identity's offset in this batch: this is
        // a zero-delta, lifecycle-only finalize. The durable guard is
        // reused verbatim rather than recomputed (there is no new frame to
        // provide real evidence for), matching "zero-delta finalization
        // preserves prior window/guard".
        Ok(FinalizationOutcome::Direct(ProgressDelta {
            file_id,
            expected_file_epoch: frontier.file_epoch,
            expected_committed_offset: frontier.offset,
            expected_framing_resume: frontier.framing_resume,
            final_offset: frontier.offset,
            final_framing_resume: frontier.framing_resume,
            final_guard_source: DeltaGuardSource::Unchanged,
            last_seen_time_unix_nano,
            finalize: true,
        }))
    }

    /// Finishes the canonical Arrow builders. Any Arrow or schema error is
    /// terminal because the consumed builders cannot be retried.
    pub(crate) fn finish(mut self) -> Result<LogicalBatch, BatchError> {
        if self.record_count == 0 {
            return Err(BatchError::EmptyBatch);
        }
        let count =
            usize::try_from(self.record_count).map_err(|_| BatchError::CounterOverflow {
                counter: "record count conversion",
            })?;

        self.logs.resource.append_id_n(0, count);
        self.logs.resource.append_schema_url_n(None, count);
        self.logs
            .resource
            .append_dropped_attributes_count_n(0, count);
        self.logs.scope.append_id_n(0, count);
        self.logs.scope.append_name_n(Some(SCOPE_NAME), count);
        self.logs
            .scope
            .append_version_n(Some(env!("CARGO_PKG_VERSION").as_bytes()), count);
        self.logs.scope.append_dropped_attributes_count_n(0, count);
        self.logs.append_trace_id_n(None, count)?;
        self.logs.append_span_id_n(None, count)?;

        let logs = self.logs.finish()?;
        let attributes = self.attributes.finish()?;
        let mut records = OtapArrowRecords::Logs(Logs::default());
        records.set(ArrowPayloadType::Logs, logs)?;
        if attributes.num_rows() != 0 {
            records.set(ArrowPayloadType::LogAttrs, attributes)?;
        }

        Ok(LogicalBatch {
            records,
            deltas: Arc::from(self.deltas),
            record_count: self.record_count,
            logical_bytes: self.logical_bytes,
            source_bytes: self.source_bytes,
        })
    }
}

/// Immutable retained batch. Cloning shares Arrow arrays/buffers and the
/// progress-delta allocation.
#[derive(Clone, Debug)]
pub(crate) struct LogicalBatch {
    records: OtapArrowRecords,
    deltas: Arc<[ProgressDelta]>,
    record_count: u32,
    logical_bytes: u64,
    source_bytes: u64,
}

impl LogicalBatch {
    /// Borrow the canonical OTAP records.
    pub(crate) const fn records(&self) -> &OtapArrowRecords {
        &self.records
    }

    /// Produces the shallow outbound OTAP view used for sends and resends.
    pub(crate) fn outbound_records(&self) -> OtapArrowRecords {
        self.records.clone()
    }

    /// Borrow the Ack delta set.
    pub(crate) fn deltas(&self) -> &[ProgressDelta] {
        &self.deltas
    }

    /// Whether the retained batch owns unresolved progress for `file_id`.
    pub(crate) fn contains_file(&self, file_id: FileId) -> bool {
        self.deltas.iter().any(|delta| delta.file_id == file_id)
    }

    /// Clone the shared delta allocation for completion correlation.
    pub(crate) fn shared_deltas(&self) -> Arc<[ProgressDelta]> {
        Arc::clone(&self.deltas)
    }

    /// Number of log records.
    pub(crate) const fn record_count(&self) -> u32 {
        self.record_count
    }

    /// Exact logical-size sum.
    pub(crate) const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    /// Source bytes represented by the retained records.
    pub(crate) const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
}

fn validate_record(record: &RecordInput, settings: &BatchSettings) -> Result<(), BatchError> {
    let body = record.framed.body_source_range;
    let frame = record.framed.frame_source_range;
    if frame.start >= frame.end {
        return Err(BatchError::InvalidRecord {
            file_id: record.file_id,
            reason: "frame_source_range must own at least one source byte",
        });
    }
    if body.start > body.end || body.start < frame.start || body.end > frame.end {
        return Err(BatchError::InvalidRecord {
            file_id: record.file_id,
            reason: "body_source_range is not contained by frame_source_range",
        });
    }
    match (&record.framed.fragment, record.framed.truncated) {
        (Some(fragment), false) => {
            if settings.oversize_behavior != MaxLogSizeBehavior::Split {
                return Err(BatchError::InvalidRecord {
                    file_id: record.file_id,
                    reason: "fragment metadata requires split oversize policy",
                });
            }
            if fragment.id.len() != 64
                || !fragment
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(BatchError::InvalidRecord {
                    file_id: record.file_id,
                    reason: "fragment id must be 64 lowercase hexadecimal bytes",
                });
            }
            if record.framed.discarded_source_bytes != 0 {
                return Err(BatchError::InvalidRecord {
                    file_id: record.file_id,
                    reason: "split fragments cannot report discarded source bytes",
                });
            }
        }
        (None, true) => {
            if settings.oversize_behavior != MaxLogSizeBehavior::Truncate {
                return Err(BatchError::InvalidRecord {
                    file_id: record.file_id,
                    reason: "truncation metadata requires truncate oversize policy",
                });
            }
        }
        (Some(_), true) => {
            return Err(BatchError::InvalidRecord {
                file_id: record.file_id,
                reason: "one frame cannot be both split and truncated",
            });
        }
        (None, false) => {
            if record.framed.discarded_source_bytes != 0 {
                return Err(BatchError::InvalidRecord {
                    file_id: record.file_id,
                    reason: "discarded source bytes require truncation metadata",
                });
            }
        }
    }
    if settings.metadata.include_file_record_number
        && record.framed.fragment.is_none()
        && record.record_number.is_none()
    {
        return Err(BatchError::InvalidRecord {
            file_id: record.file_id,
            reason: "enabled record-number metadata requires a worker-generated number",
        });
    }
    match record.framed.decode_outcome {
        DecodeOutcome::Clean => {}
        DecodeOutcome::Replacements { count } => {
            if settings.decode_policy != OnDecodeError::Replace || count == 0 {
                return Err(BatchError::InvalidRecord {
                    file_id: record.file_id,
                    reason: "replacement evidence does not match decode policy",
                });
            }
        }
        DecodeOutcome::PreserveRaw { count } => {
            if settings.decode_policy != OnDecodeError::PreserveRaw || count == 0 {
                return Err(BatchError::InvalidRecord {
                    file_id: record.file_id,
                    reason: "preserve-raw evidence does not match decode policy",
                });
            }
        }
    }
    Ok(())
}

fn validate_resume_transition(
    record: &RecordInput,
    prior_resume: FramingResume,
) -> Result<(), BatchError> {
    let Some(fragment) = &record.framed.fragment else {
        if prior_resume != FramingResume::Clean
            || record.framed.resulting_resume != FramingResume::Clean
        {
            return Err(BatchError::InvalidProgress {
                file_id: record.file_id,
                reason: "an unsplit record cannot enter or produce continuation state",
            });
        }
        return Ok(());
    };

    let (record_start_offset, expected_index) = match prior_resume {
        FramingResume::Clean => (record.framed.body_source_range.start, 0),
        FramingResume::Continuation {
            record_start_offset,
            next_fragment_index,
            // Not consulted by this reconstruction check: only the
            // fragment start/index feed `fragment_id`.
            record_end_offset: _,
        } => {
            if record_start_offset >= record.framed.frame_source_range.start {
                return Err(BatchError::InvalidProgress {
                    file_id: record.file_id,
                    reason: "continued fragment record start must precede its frame",
                });
            }
            (record_start_offset, next_fragment_index)
        }
    };
    if fragment.index != expected_index {
        return Err(BatchError::InvalidProgress {
            file_id: record.file_id,
            reason: "fragment index does not match the prior framing state",
        });
    }
    if fragment.id
        != fragment_id(
            record.file_id,
            record.progress_base.file_epoch,
            record_start_offset,
        )
    {
        return Err(BatchError::InvalidRecord {
            file_id: record.file_id,
            reason: "fragment id does not match file, epoch, and record start",
        });
    }
    let expected_resume = if fragment.last {
        FramingResume::Clean
    } else {
        FramingResume::Continuation {
            record_start_offset,
            // Fragments produced by this framer always use the
            // scan-to-next-physical-LF sentinel; see `framer.rs`.
            record_end_offset: 0,
            next_fragment_index: fragment.index.checked_add(1).ok_or(
                BatchError::InvalidProgress {
                    file_id: record.file_id,
                    reason: "nonfinal fragment index cannot advance",
                },
            )?,
        }
    };
    if record.framed.resulting_resume != expected_resume {
        return Err(BatchError::InvalidProgress {
            file_id: record.file_id,
            reason: "fragment result does not match its continuation metadata",
        });
    }
    Ok(())
}

fn prepare_attributes(
    record: &RecordInput,
    settings: &BatchSettings,
) -> Result<Vec<PreparedAttribute>, BatchError> {
    let matched = prepare_path(&record.matched_path, "matched")?;
    let file_name = record
        .matched_path
        .file_name()
        .ok_or_else(|| BatchError::MissingFileName {
            path: record.matched_path.clone(),
        })?;
    let name = prepare_path(Path::new(file_name), "file name")?;
    let resolved = settings
        .metadata
        .include_file_path_resolved
        .then(|| prepare_path(&record.resolved_path, "resolved"))
        .transpose()?;
    let path_was_encoded =
        matched.encoded || name.encoded || resolved.as_ref().is_some_and(|path| path.encoded);

    let mut attributes = Vec::new();
    attributes
        .try_reserve_exact(MAX_PREPARED_ATTRIBUTES)
        .map_err(|source| BatchError::AllocationFailed {
            resource: "projected record attributes",
            source,
        })?;
    push_attribute(
        &mut attributes,
        ATTR_KEY_LOG_FILE_PATH,
        PreparedValue::String(matched.value),
    )?;
    push_attribute(
        &mut attributes,
        ATTR_KEY_LOG_FILE_NAME,
        PreparedValue::String(name.value),
    )?;
    if let Some(resolved) = resolved {
        push_attribute(
            &mut attributes,
            ATTR_KEY_PATH_RESOLVED,
            PreparedValue::String(resolved.value),
        )?;
    }
    if path_was_encoded {
        push_attribute(
            &mut attributes,
            ATTR_KEY_PATH_ENCODING,
            PreparedValue::StaticString(ENCODED_PATH_DISCRIMINATOR),
        )?;
    }
    if settings.metadata.include_file_record_offset {
        push_attribute(
            &mut attributes,
            ATTR_KEY_RECORD_OFFSET,
            PreparedValue::String(record.framed.body_source_range.start.to_string()),
        )?;
    }
    if settings.metadata.include_file_record_number && record.framed.fragment.is_none() {
        if let Some(record_number) = record.record_number {
            push_attribute(
                &mut attributes,
                ATTR_KEY_RECORD_NUMBER,
                PreparedValue::String(record_number.to_string()),
            )?;
        }
    }
    if let Some(fragment) = &record.framed.fragment {
        push_fragment_attributes(&mut attributes, fragment, record)?;
    }
    if record.framed.truncated {
        push_attribute(
            &mut attributes,
            ATTR_KEY_RECORD_TRUNCATED,
            PreparedValue::Bool(true),
        )?;
    }
    if let Some(reason) = record.framed.flush_reason {
        push_attribute(
            &mut attributes,
            ATTR_KEY_FLUSH_REASON,
            PreparedValue::StaticString(flush_reason_value(reason)),
        )?;
        if reason == FlushReason::Rotation {
            push_attribute(
                &mut attributes,
                ATTR_KEY_TERMINAL_UNTERMINATED,
                PreparedValue::Bool(true),
            )?;
        }
    }
    match record.framed.decode_outcome {
        DecodeOutcome::Clean => {}
        DecodeOutcome::Replacements { count } => {
            push_decode_attributes(&mut attributes, "replace", count)?;
        }
        DecodeOutcome::PreserveRaw { count } => {
            push_decode_attributes(&mut attributes, "preserve_raw", count)?;
        }
    }
    Ok(attributes)
}

fn push_fragment_attributes(
    attributes: &mut Vec<PreparedAttribute>,
    fragment: &FragmentMetadata,
    record: &RecordInput,
) -> Result<(), BatchError> {
    push_attribute(
        attributes,
        ATTR_KEY_FRAGMENT_ID,
        PreparedValue::String(fragment.id.clone()),
    )?;
    push_attribute(
        attributes,
        ATTR_KEY_FRAGMENT_INDEX,
        PreparedValue::Int(i64::from(fragment.index)),
    )?;
    push_attribute(
        attributes,
        ATTR_KEY_FRAGMENT_LAST,
        PreparedValue::Bool(fragment.last),
    )?;
    push_attribute(
        attributes,
        ATTR_KEY_FRAGMENT_SOURCE_START,
        PreparedValue::String(record.framed.body_source_range.start.to_string()),
    )?;
    push_attribute(
        attributes,
        ATTR_KEY_FRAGMENT_SOURCE_END,
        PreparedValue::String(record.framed.body_source_range.end.to_string()),
    )
}

fn push_decode_attributes(
    attributes: &mut Vec<PreparedAttribute>,
    policy: &'static str,
    count: u64,
) -> Result<(), BatchError> {
    push_attribute(
        attributes,
        ATTR_KEY_DECODE_ERROR_POLICY,
        PreparedValue::StaticString(policy),
    )?;
    push_attribute(
        attributes,
        ATTR_KEY_DECODE_ERROR_COUNT,
        PreparedValue::String(count.to_string()),
    )
}

fn push_attribute(
    attributes: &mut Vec<PreparedAttribute>,
    key: &'static str,
    value: PreparedValue,
) -> Result<(), BatchError> {
    attributes.push(PreparedAttribute::new(key, value)?);
    Ok(())
}

fn prepare_path(path: &Path, field: &'static str) -> Result<PreparedPath, BatchError> {
    let native = native_path_bytes(path).map_err(|source| BatchError::Path {
        field,
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    if let Some(value) = path.to_str() {
        return Ok(PreparedPath {
            value: value.to_owned(),
            encoded: false,
        });
    }

    let encoded_bytes = native
        .len()
        .checked_mul(3)
        .and_then(|bytes| bytes.checked_add(ENCODED_PATH_PREFIX.len()))
        .ok_or(BatchError::PathEncodingOverflow)?;
    let mut value = String::new();
    value
        .try_reserve_exact(encoded_bytes)
        .map_err(|source| BatchError::AllocationFailed {
            resource: "percent-encoded path",
            source,
        })?;
    value.push_str(ENCODED_PATH_PREFIX);
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in native {
        value.push('%');
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(PreparedPath {
        value,
        encoded: true,
    })
}

const fn flush_reason_value(reason: FlushReason) -> &'static str {
    match reason {
        FlushReason::MaxLines => "max_lines",
        FlushReason::Timeout => "timeout",
        FlushReason::OversizeLineBoundary => "oversize_line_boundary",
        FlushReason::Rotation => "rotation",
        FlushReason::Drain => "drain",
    }
}

fn append_projection(
    logs: &mut LogsRecordBatchBuilder,
    attributes: &mut StrKeysAttributesRecordBatchBuilder<u16>,
    log_id: u16,
    record: &RecordInput,
    prepared_attributes: &[PreparedAttribute],
) {
    logs.append_id(Some(log_id));
    logs.append_time_unix_nano(0);
    logs.append_observed_time_unix_nano(record.observed_time_unix_nano);
    logs.append_schema_url(None);
    logs.append_severity_number(None);
    logs.append_severity_text(None);
    match &record.framed.body {
        FramedBody::Text(body) => logs.body.append_str(body.as_bytes()),
        FramedBody::Bytes(body) => logs.body.append_bytes(body),
    }
    logs.append_dropped_attributes_count(0);
    logs.append_flags(None);
    logs.append_event_name(None);

    for attribute in prepared_attributes {
        attributes.append_parent_id(&log_id);
        attributes.append_key(attribute.key);
        attribute.value.append_to(attributes);
    }
}

fn log_id_for_count(record_count: u32) -> Result<u16, BatchError> {
    if record_count >= u32::from(u16::MAX) {
        return Err(BatchError::InvalidLogId { record_count });
    }
    u16::try_from(record_count).map_err(|_| BatchError::InvalidLogId { record_count })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordNumberState {
    file_epoch: u32,
    next: u64,
}

/// Transactional worker-local record-number decision.
///
/// The worker prepares this before projection, puts [`Self::record_number`]
/// into [`RecordInput`], and commits it only after `try_append` succeeds.
/// Refused speculative records therefore do not consume a number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecordNumberReservation {
    file_id: FileId,
    expected: Option<RecordNumberState>,
    resulting: RecordNumberState,
    record_number: Option<u64>,
}

impl RecordNumberReservation {
    /// Number projected for an unsplit record; fragments always return
    /// `None`.
    pub(crate) const fn record_number(&self) -> Option<u64> {
        self.record_number
    }
}

/// Bounded worker-local record numbers keyed by durable identity.
///
/// Entries survive descriptor eviction because descriptor lifecycle never
/// touches this table. A new process constructs a new table, and an epoch
/// change resets that file to zero.
pub(crate) struct RecordNumberTable {
    max_files: usize,
    states: HashMap<FileId, RecordNumberState>,
}

impl RecordNumberTable {
    /// Pre-reserves the complete configured file population.
    pub(crate) fn new(max_files: usize) -> Result<Self, BatchError> {
        if max_files == 0 {
            return Err(BatchError::InvalidSettings {
                reason: "record-number max_files must be greater than zero",
            });
        }
        let mut states = HashMap::new();
        states
            .try_reserve(max_files)
            .map_err(|source| BatchError::AllocationFailed {
                resource: "record-number table",
                source,
            })?;
        Ok(Self { max_files, states })
    }

    /// Prepares a number decision without mutating the table.
    ///
    /// `fragment_index == None` numbers and advances an unsplit record.
    /// Fragment zero advances once but emits no number; later fragments do
    /// neither. Every fragment omits the projected number.
    pub(crate) fn prepare(
        &self,
        file_id: FileId,
        file_epoch: u32,
        fragment_index: Option<u32>,
    ) -> Result<RecordNumberReservation, BatchError> {
        let expected = self.states.get(&file_id).copied();
        if expected.is_none() && self.states.len() >= self.max_files {
            return Err(BatchError::RecordNumberCapacityExhausted {
                max_files: self.max_files,
            });
        }
        let next = expected
            .filter(|state| state.file_epoch == file_epoch)
            .map_or(0, |state| state.next);
        let advance = fragment_index.is_none() || fragment_index == Some(0);
        let resulting_next = if advance {
            next.checked_add(1)
                .ok_or(BatchError::RecordNumberOverflow { file_id })?
        } else {
            next
        };
        Ok(RecordNumberReservation {
            file_id,
            expected,
            resulting: RecordNumberState {
                file_epoch,
                next: resulting_next,
            },
            record_number: fragment_index.is_none().then_some(next),
        })
    }

    /// Commits a decision after its record was accepted into the open batch.
    pub(crate) fn commit(
        &mut self,
        reservation: RecordNumberReservation,
    ) -> Result<Option<u64>, BatchError> {
        if self.states.get(&reservation.file_id).copied() != reservation.expected {
            return Err(BatchError::StaleRecordNumberReservation {
                file_id: reservation.file_id,
            });
        }
        if reservation.expected.is_none() && self.states.len() >= self.max_files {
            return Err(BatchError::RecordNumberCapacityExhausted {
                max_files: self.max_files,
            });
        }
        let _ = self
            .states
            .insert(reservation.file_id, reservation.resulting);
        Ok(reservation.record_number)
    }

    /// Releases numbering state after the identity leaves the tracked-file
    /// population. Descriptor closure alone must not call this method.
    pub(crate) fn remove(&mut self, file_id: FileId) -> bool {
        self.states.remove(&file_id).is_some()
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
