// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Codec for the version-1 append-only WAL format: the WAL header, the
//! eight logical operations, transaction framing, and the torn-tail-versus-
//! corruption scanning algorithm.
//!
//! See `docs/filelog-checkpoint-format.md`, "WAL file format" and
//! "Torn-tail versus corruption", for the exact byte layout and scanning
//! rules this module implements.

use super::error::{DecodeError, EncodeError};
use super::primitives::{
    ADVISORY_PATH_MAX_BYTES, AUDIT_REASON_MAX_BYTES, ByteReader, ByteWriter, FINGERPRINT_MAX_BYTES,
    FORMAT_VERSION, FileId, FramingResume, LifecycleState, Locator, NAMESPACE_ID_MAX_BYTES,
    WAL_MAGIC, WAL_MAX_OPS_PER_TX, crc32c,
};

/// Fixed width of the WAL header, in bytes.
pub const WAL_HEADER_LEN: usize = 24;

/// `register_file` (`op_code = 0x01`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterFile {
    /// The new durable file identity.
    pub file_id: FileId,
    /// Initial file epoch; MUST be `1` (validated at apply time).
    pub file_epoch: u32,
    /// Initial committed offset.
    pub committed_offset: u64,
    /// Initial fingerprint matching evidence.
    pub fingerprint: Vec<u8>,
    /// Number of header bytes ignored for fingerprinting.
    pub ignored_header_bytes: u32,
    /// Initial runtime locator.
    pub locator: Locator,
    /// Framing-profile recipe version at registration time.
    pub framing_profile_version: u16,
    /// Framing-profile digest at registration time.
    pub framing_profile_digest: [u8; 32],
    /// Initial framing-resume state; MUST be `Clean` (validated at apply
    /// time).
    pub framing_resume: FramingResume,
    /// Initial last-seen timestamp, in Unix nanoseconds.
    pub last_seen_time_unix_nano: u64,
    /// Initial advisory path.
    pub advisory_path: Vec<u8>,
}

/// `update_progress` (`op_code = 0x02`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateProgress {
    /// The file identity this update applies to.
    pub file_id: FileId,
    /// The offset the caller expects is currently committed.
    pub expected_committed_offset: u64,
    /// The epoch the caller expects is currently active.
    pub expected_file_epoch: u32,
    /// The new committed offset (monotonic, non-decreasing).
    pub new_committed_offset: u64,
    /// The new framing-resume state, applied atomically with the offset.
    pub new_framing_resume: FramingResume,
    /// The new last-seen timestamp, in Unix nanoseconds.
    pub new_last_seen_time_unix_nano: u64,
    /// If set, additionally transitions the record to `RotatedFinalized`.
    pub finalize: bool,
}

/// `reset_after_truncate` (`op_code = 0x03`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetAfterTruncate {
    /// The file identity this reset applies to.
    pub file_id: FileId,
    /// The epoch the caller expects is currently active.
    pub expected_active_epoch: u32,
    /// Observed truncated size (informational evidence).
    pub observed_truncated_size: u64,
    /// The resulting epoch; MUST equal `expected_active_epoch + 1`
    /// (validated at apply time).
    pub resulting_epoch: u32,
    /// The resulting committed offset; MUST be `0` in this version
    /// (validated at apply time).
    pub new_committed_offset: u64,
    /// The resulting framing-resume state; MUST be `Clean` (validated at
    /// apply time).
    pub new_framing_resume: FramingResume,
    /// Reset timestamp, in Unix nanoseconds.
    pub reset_time_unix_nano: u64,
    /// Opaque reason code; MUST equal `TRUNCATE_RESET_REASON_READ_NEW`
    /// (validated at apply time, not decode time).
    pub reason_code: u16,
}

/// `update_fingerprint` (`op_code = 0x04`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFingerprint {
    /// The file identity this update applies to.
    pub file_id: FileId,
    /// The epoch the caller expects is currently active.
    pub expected_file_epoch: u32,
    /// The fingerprint bytes the caller expects are currently stored.
    pub expected_fingerprint: Vec<u8>,
    /// The replacement fingerprint bytes.
    pub new_fingerprint: Vec<u8>,
}

/// `update_metadata` (`op_code = 0x05`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMetadata {
    /// The file identity this update applies to.
    pub file_id: FileId,
    /// The replacement runtime locator, if `LOCATOR_PRESENT` is set.
    pub locator: Option<Locator>,
    /// The new last-seen timestamp, in Unix nanoseconds (always applied to
    /// an `Active` record; applied to a `Quarantined` record as well).
    pub last_seen_time_unix_nano: u64,
    /// The replacement advisory path, if `PATH_PRESENT` is set.
    pub advisory_path: Option<Vec<u8>>,
}

/// `quarantine_file` (`op_code = 0x06`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineFile {
    /// The file identity this quarantine applies to.
    pub file_id: FileId,
    /// The epoch the caller expects is currently active.
    pub expected_file_epoch: u32,
    /// Opaque diagnostic reason code.
    pub reason_code: u16,
    /// The immutable quarantine locator.
    pub locator: Locator,
    /// Observed file size at the moment of quarantine.
    pub observed_size: u64,
    /// The epoch value in effect at the moment of quarantine; MUST equal
    /// `expected_file_epoch` (validated at apply time).
    pub quarantine_epoch: u32,
    /// Quarantine timestamp, in Unix nanoseconds.
    pub quarantine_time_unix_nano: u64,
}

/// `reset_quarantined_file`'s `action` discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetQuarantineAction {
    /// Return the record to `Active` at offset `0`.
    ResetToBeginning,
    /// Return the record to `Active` at the given, externally supplied
    /// offset.
    ResetToEnd,
    /// Leave the record `Quarantined`; a durable, audited no-op decision.
    KeepFailed,
}

impl ResetQuarantineAction {
    fn to_wire(self) -> u8 {
        match self {
            ResetQuarantineAction::ResetToBeginning => 0x01,
            ResetQuarantineAction::ResetToEnd => 0x02,
            ResetQuarantineAction::KeepFailed => 0x03,
        }
    }

    fn from_wire(value: u8) -> Result<Self, DecodeError> {
        match value {
            0x01 => Ok(ResetQuarantineAction::ResetToBeginning),
            0x02 => Ok(ResetQuarantineAction::ResetToEnd),
            0x03 => Ok(ResetQuarantineAction::KeepFailed),
            other => Err(DecodeError::UnknownDiscriminant {
                field: "reset_quarantined_file.action",
                value: other as u32,
            }),
        }
    }
}

/// `reset_quarantined_file` (`op_code = 0x07`). Unconditionally
/// administrative: `audit_reason` is always mandatory and non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetQuarantinedFile {
    /// The file identity this reset applies to.
    pub file_id: FileId,
    /// The quarantine epoch the caller expects is currently stored.
    pub expected_quarantine_epoch: u32,
    /// The requested action.
    pub action: ResetQuarantineAction,
    /// The resulting epoch.
    pub resulting_epoch: u32,
    /// The resulting committed offset.
    pub resulting_offset: u64,
    /// The resulting framing-resume state; MUST be `Clean` for either reset
    /// action (validated at apply time).
    pub new_framing_resume: FramingResume,
    /// Reset timestamp, in Unix nanoseconds.
    pub reset_time_unix_nano: u64,
    /// Mandatory, non-empty operator audit reason.
    pub audit_reason: String,
}

/// `remove_file` (`op_code = 0x08`). Conditionally administrative,
/// distinguished by `administrative`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveFile {
    /// The file identity to remove.
    pub file_id: FileId,
    /// The epoch (or, for a quarantined record, the quarantine epoch) the
    /// caller expects is currently stored.
    pub expected_file_epoch: u32,
    /// The lifecycle state the caller expects the record is currently in.
    pub expected_prior_state: LifecycleState,
    /// Opaque diagnostic removal reason.
    pub removal_reason: u16,
    /// Removal timestamp, in Unix nanoseconds.
    pub removal_time_unix_nano: u64,
    /// Whether this is an administrative (operator-authorized) removal.
    /// MUST be `true` to remove a `Quarantined` record.
    pub administrative: bool,
    /// The exact checkpoint namespace targeted; present iff
    /// `administrative`.
    pub namespace_id: Option<String>,
    /// Mandatory operator audit reason; present iff `administrative`.
    pub audit_reason: Option<String>,
}

/// One decoded, self-delimiting WAL operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// See [`RegisterFile`].
    RegisterFile(RegisterFile),
    /// See [`UpdateProgress`].
    UpdateProgress(UpdateProgress),
    /// See [`ResetAfterTruncate`].
    ResetAfterTruncate(ResetAfterTruncate),
    /// See [`UpdateFingerprint`].
    UpdateFingerprint(UpdateFingerprint),
    /// See [`UpdateMetadata`].
    UpdateMetadata(UpdateMetadata),
    /// See [`QuarantineFile`].
    QuarantineFile(QuarantineFile),
    /// See [`ResetQuarantinedFile`].
    ResetQuarantinedFile(ResetQuarantinedFile),
    /// See [`RemoveFile`].
    RemoveFile(RemoveFile),
}

const OP_REGISTER_FILE: u8 = 0x01;
const OP_UPDATE_PROGRESS: u8 = 0x02;
const OP_RESET_AFTER_TRUNCATE: u8 = 0x03;
const OP_UPDATE_FINGERPRINT: u8 = 0x04;
const OP_UPDATE_METADATA: u8 = 0x05;
const OP_QUARANTINE_FILE: u8 = 0x06;
const OP_RESET_QUARANTINED_FILE: u8 = 0x07;
const OP_REMOVE_FILE: u8 = 0x08;

const METADATA_LOCATOR_PRESENT: u8 = super::primitives::METADATA_LOCATOR_PRESENT;
const METADATA_PATH_PRESENT: u8 = super::primitives::METADATA_PATH_PRESENT;
const METADATA_PRESENCE_RESERVED_MASK: u8 = super::primitives::METADATA_PRESENCE_RESERVED_MASK;

impl Operation {
    /// The `file_id` every operation payload carries.
    #[must_use]
    pub fn file_id(&self) -> FileId {
        match self {
            Operation::RegisterFile(op) => op.file_id,
            Operation::UpdateProgress(op) => op.file_id,
            Operation::ResetAfterTruncate(op) => op.file_id,
            Operation::UpdateFingerprint(op) => op.file_id,
            Operation::UpdateMetadata(op) => op.file_id,
            Operation::QuarantineFile(op) => op.file_id,
            Operation::ResetQuarantinedFile(op) => op.file_id,
            Operation::RemoveFile(op) => op.file_id,
        }
    }

    fn encode_payload(&self) -> Result<Vec<u8>, EncodeError> {
        let mut out = ByteWriter::new();
        match self {
            Operation::RegisterFile(op) => {
                out.write_u8(OP_REGISTER_FILE);
                out.write_bytes(&op.file_id.0);
                out.write_u32(op.file_epoch);
                out.write_u64(op.committed_offset);
                out.write_var_bytes(
                    "register_file.fingerprint",
                    &op.fingerprint,
                    FINGERPRINT_MAX_BYTES,
                )?;
                out.write_u32(op.ignored_header_bytes);
                op.locator.write(&mut out);
                out.write_u16(op.framing_profile_version);
                out.write_bytes(&op.framing_profile_digest);
                op.framing_resume.write(&mut out);
                out.write_u64(op.last_seen_time_unix_nano);
                out.write_var_bytes(
                    "register_file.advisory_path",
                    &op.advisory_path,
                    ADVISORY_PATH_MAX_BYTES,
                )?;
            }
            Operation::UpdateProgress(op) => {
                out.write_u8(OP_UPDATE_PROGRESS);
                out.write_bytes(&op.file_id.0);
                out.write_u64(op.expected_committed_offset);
                out.write_u32(op.expected_file_epoch);
                out.write_u64(op.new_committed_offset);
                op.new_framing_resume.write(&mut out);
                out.write_u64(op.new_last_seen_time_unix_nano);
                out.write_u8(u8::from(op.finalize));
            }
            Operation::ResetAfterTruncate(op) => {
                out.write_u8(OP_RESET_AFTER_TRUNCATE);
                out.write_bytes(&op.file_id.0);
                out.write_u32(op.expected_active_epoch);
                out.write_u64(op.observed_truncated_size);
                out.write_u32(op.resulting_epoch);
                out.write_u64(op.new_committed_offset);
                op.new_framing_resume.write(&mut out);
                out.write_u64(op.reset_time_unix_nano);
                out.write_u16(op.reason_code);
            }
            Operation::UpdateFingerprint(op) => {
                out.write_u8(OP_UPDATE_FINGERPRINT);
                out.write_bytes(&op.file_id.0);
                out.write_u32(op.expected_file_epoch);
                out.write_var_bytes(
                    "update_fingerprint.expected_fingerprint",
                    &op.expected_fingerprint,
                    FINGERPRINT_MAX_BYTES,
                )?;
                out.write_var_bytes(
                    "update_fingerprint.new_fingerprint",
                    &op.new_fingerprint,
                    FINGERPRINT_MAX_BYTES,
                )?;
            }
            Operation::UpdateMetadata(op) => {
                out.write_u8(OP_UPDATE_METADATA);
                out.write_bytes(&op.file_id.0);
                let mut presence = 0u8;
                if op.locator.is_some() {
                    presence |= METADATA_LOCATOR_PRESENT;
                }
                if op.advisory_path.is_some() {
                    presence |= METADATA_PATH_PRESENT;
                }
                out.write_u8(presence);
                if let Some(locator) = &op.locator {
                    locator.write(&mut out);
                }
                out.write_u64(op.last_seen_time_unix_nano);
                if let Some(path) = &op.advisory_path {
                    out.write_var_bytes(
                        "update_metadata.advisory_path",
                        path,
                        ADVISORY_PATH_MAX_BYTES,
                    )?;
                }
            }
            Operation::QuarantineFile(op) => {
                out.write_u8(OP_QUARANTINE_FILE);
                out.write_bytes(&op.file_id.0);
                out.write_u32(op.expected_file_epoch);
                out.write_u16(op.reason_code);
                op.locator.write(&mut out);
                out.write_u64(op.observed_size);
                out.write_u32(op.quarantine_epoch);
                out.write_u64(op.quarantine_time_unix_nano);
            }
            Operation::ResetQuarantinedFile(op) => {
                if op.audit_reason.is_empty() {
                    return Err(EncodeError::RequiredFieldEmpty {
                        field: "reset_quarantined_file.audit_reason",
                    });
                }
                out.write_u8(OP_RESET_QUARANTINED_FILE);
                out.write_bytes(&op.file_id.0);
                out.write_u32(op.expected_quarantine_epoch);
                out.write_u8(op.action.to_wire());
                out.write_u32(op.resulting_epoch);
                out.write_u64(op.resulting_offset);
                op.new_framing_resume.write(&mut out);
                out.write_u64(op.reset_time_unix_nano);
                out.write_var_bytes(
                    "reset_quarantined_file.audit_reason",
                    op.audit_reason.as_bytes(),
                    AUDIT_REASON_MAX_BYTES,
                )?;
            }
            Operation::RemoveFile(op) => {
                out.write_u8(OP_REMOVE_FILE);
                out.write_bytes(&op.file_id.0);
                out.write_u32(op.expected_file_epoch);
                out.write_u8(op.expected_prior_state.to_wire());
                out.write_u16(op.removal_reason);
                out.write_u64(op.removal_time_unix_nano);
                out.write_u8(u8::from(op.administrative));
                match (op.administrative, &op.namespace_id, &op.audit_reason) {
                    (true, Some(namespace_id), Some(audit_reason)) => {
                        if namespace_id.is_empty() {
                            return Err(EncodeError::RequiredFieldEmpty {
                                field: "remove_file.namespace_id",
                            });
                        }
                        if audit_reason.is_empty() {
                            return Err(EncodeError::RequiredFieldEmpty {
                                field: "remove_file.audit_reason",
                            });
                        }
                        out.write_var_bytes(
                            "remove_file.namespace_id",
                            namespace_id.as_bytes(),
                            NAMESPACE_ID_MAX_BYTES,
                        )?;
                        out.write_var_bytes(
                            "remove_file.audit_reason",
                            audit_reason.as_bytes(),
                            AUDIT_REASON_MAX_BYTES,
                        )?;
                    }
                    (false, None, None) => {
                        out.write_u16(0);
                        out.write_u16(0);
                    }
                    _ => {
                        // Constructing this combination is a caller bug: the
                        // administrative flag and the namespace/audit-reason
                        // presence must agree. This codec refuses to guess
                        // which side is authoritative.
                        return Err(EncodeError::RequiredFieldEmpty {
                            field: "remove_file.administrative",
                        });
                    }
                }
            }
        }
        Ok(out.into_bytes())
    }

    /// Encodes this operation as a self-delimiting `op_len || op_payload ||
    /// op_crc32c` frame.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let payload = self.encode_payload()?;
        let mut out = ByteWriter::new();
        out.write_u32(payload.len() as u32);
        out.write_bytes(&payload);
        let crc = crc32c(out.as_bytes());
        out.write_u32(crc);
        Ok(out.into_bytes())
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = ByteReader::new(bytes);
        let op_code = input.read_u8()?;
        let mut file_id_bytes = [0u8; 16];
        file_id_bytes.copy_from_slice(input.read_exact(16)?);
        let file_id = FileId(file_id_bytes);
        let operation = match op_code {
            OP_REGISTER_FILE => {
                let file_epoch = input.read_u32()?;
                let committed_offset = input.read_u64()?;
                let fingerprint = input
                    .read_var_bytes("register_file.fingerprint", FINGERPRINT_MAX_BYTES)?
                    .to_vec();
                let ignored_header_bytes = input.read_u32()?;
                let locator = Locator::read(&mut input)?;
                let framing_profile_version = input.read_u16()?;
                let mut framing_profile_digest = [0u8; 32];
                framing_profile_digest.copy_from_slice(input.read_exact(32)?);
                let framing_resume = FramingResume::read(&mut input)?;
                let last_seen_time_unix_nano = input.read_u64()?;
                let advisory_path = input
                    .read_var_bytes("register_file.advisory_path", ADVISORY_PATH_MAX_BYTES)?
                    .to_vec();
                Operation::RegisterFile(RegisterFile {
                    file_id,
                    file_epoch,
                    committed_offset,
                    fingerprint,
                    ignored_header_bytes,
                    locator,
                    framing_profile_version,
                    framing_profile_digest,
                    framing_resume,
                    last_seen_time_unix_nano,
                    advisory_path,
                })
            }
            OP_UPDATE_PROGRESS => {
                let expected_committed_offset = input.read_u64()?;
                let expected_file_epoch = input.read_u32()?;
                let new_committed_offset = input.read_u64()?;
                let new_framing_resume = FramingResume::read(&mut input)?;
                let new_last_seen_time_unix_nano = input.read_u64()?;
                let finalize = read_bool(&mut input, "update_progress.finalize")?;
                Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset,
                    expected_file_epoch,
                    new_committed_offset,
                    new_framing_resume,
                    new_last_seen_time_unix_nano,
                    finalize,
                })
            }
            OP_RESET_AFTER_TRUNCATE => {
                let expected_active_epoch = input.read_u32()?;
                let observed_truncated_size = input.read_u64()?;
                let resulting_epoch = input.read_u32()?;
                let new_committed_offset = input.read_u64()?;
                let new_framing_resume = FramingResume::read(&mut input)?;
                let reset_time_unix_nano = input.read_u64()?;
                let reason_code = input.read_u16()?;
                Operation::ResetAfterTruncate(ResetAfterTruncate {
                    file_id,
                    expected_active_epoch,
                    observed_truncated_size,
                    resulting_epoch,
                    new_committed_offset,
                    new_framing_resume,
                    reset_time_unix_nano,
                    reason_code,
                })
            }
            OP_UPDATE_FINGERPRINT => {
                let expected_file_epoch = input.read_u32()?;
                let expected_fingerprint = input
                    .read_var_bytes(
                        "update_fingerprint.expected_fingerprint",
                        FINGERPRINT_MAX_BYTES,
                    )?
                    .to_vec();
                let new_fingerprint = input
                    .read_var_bytes("update_fingerprint.new_fingerprint", FINGERPRINT_MAX_BYTES)?
                    .to_vec();
                Operation::UpdateFingerprint(UpdateFingerprint {
                    file_id,
                    expected_file_epoch,
                    expected_fingerprint,
                    new_fingerprint,
                })
            }
            OP_UPDATE_METADATA => {
                let presence = input.read_u8()?;
                if presence & METADATA_PRESENCE_RESERVED_MASK != 0 {
                    return Err(DecodeError::ReservedFieldNonZero {
                        field: "update_metadata.presence_flags",
                        value: presence as u64,
                    });
                }
                let locator = if presence & METADATA_LOCATOR_PRESENT != 0 {
                    Some(Locator::read(&mut input)?)
                } else {
                    None
                };
                let last_seen_time_unix_nano = input.read_u64()?;
                let advisory_path = if presence & METADATA_PATH_PRESENT != 0 {
                    Some(
                        input
                            .read_var_bytes(
                                "update_metadata.advisory_path",
                                ADVISORY_PATH_MAX_BYTES,
                            )?
                            .to_vec(),
                    )
                } else {
                    None
                };
                Operation::UpdateMetadata(UpdateMetadata {
                    file_id,
                    locator,
                    last_seen_time_unix_nano,
                    advisory_path,
                })
            }
            OP_QUARANTINE_FILE => {
                let expected_file_epoch = input.read_u32()?;
                let reason_code = input.read_u16()?;
                let locator = Locator::read(&mut input)?;
                let observed_size = input.read_u64()?;
                let quarantine_epoch = input.read_u32()?;
                let quarantine_time_unix_nano = input.read_u64()?;
                Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch,
                    reason_code,
                    locator,
                    observed_size,
                    quarantine_epoch,
                    quarantine_time_unix_nano,
                })
            }
            OP_RESET_QUARANTINED_FILE => {
                let expected_quarantine_epoch = input.read_u32()?;
                let action = ResetQuarantineAction::from_wire(input.read_u8()?)?;
                let resulting_epoch = input.read_u32()?;
                let resulting_offset = input.read_u64()?;
                let new_framing_resume = FramingResume::read(&mut input)?;
                let reset_time_unix_nano = input.read_u64()?;
                let audit_reason = input.read_var_string(
                    "reset_quarantined_file.audit_reason",
                    AUDIT_REASON_MAX_BYTES,
                )?;
                if audit_reason.is_empty() {
                    return Err(DecodeError::EmptyRequiredField {
                        field: "reset_quarantined_file.audit_reason",
                    });
                }
                Operation::ResetQuarantinedFile(ResetQuarantinedFile {
                    file_id,
                    expected_quarantine_epoch,
                    action,
                    resulting_epoch,
                    resulting_offset,
                    new_framing_resume,
                    reset_time_unix_nano,
                    audit_reason: audit_reason.to_owned(),
                })
            }
            OP_REMOVE_FILE => {
                let expected_file_epoch = input.read_u32()?;
                let expected_prior_state = LifecycleState::from_wire(
                    input.read_u8()?,
                    "remove_file.expected_prior_state",
                )?;
                let removal_reason = input.read_u16()?;
                let removal_time_unix_nano = input.read_u64()?;
                let administrative = read_bool(&mut input, "remove_file.administrative")?;
                let namespace_id_raw =
                    input.read_var_string("remove_file.namespace_id", NAMESPACE_ID_MAX_BYTES)?;
                let audit_reason_raw =
                    input.read_var_string("remove_file.audit_reason", AUDIT_REASON_MAX_BYTES)?;
                let (namespace_id, audit_reason) = if administrative {
                    if namespace_id_raw.is_empty() {
                        return Err(DecodeError::EmptyRequiredField {
                            field: "remove_file.namespace_id",
                        });
                    }
                    if audit_reason_raw.is_empty() {
                        return Err(DecodeError::EmptyRequiredField {
                            field: "remove_file.audit_reason",
                        });
                    }
                    (
                        Some(namespace_id_raw.to_owned()),
                        Some(audit_reason_raw.to_owned()),
                    )
                } else {
                    if !namespace_id_raw.is_empty() {
                        return Err(DecodeError::UnexpectedPresentField {
                            field: "remove_file.namespace_id",
                        });
                    }
                    if !audit_reason_raw.is_empty() {
                        return Err(DecodeError::UnexpectedPresentField {
                            field: "remove_file.audit_reason",
                        });
                    }
                    (None, None)
                };
                Operation::RemoveFile(RemoveFile {
                    file_id,
                    expected_file_epoch,
                    expected_prior_state,
                    removal_reason,
                    removal_time_unix_nano,
                    administrative,
                    namespace_id,
                    audit_reason,
                })
            }
            other => {
                return Err(DecodeError::UnknownDiscriminant {
                    field: "operation.op_code",
                    value: other as u32,
                });
            }
        };
        if input.remaining() != 0 {
            return Err(DecodeError::UnconsumedBytes {
                context: "operation",
                declared: bytes.len(),
                consumed: bytes.len() - input.remaining(),
            });
        }
        Ok(operation)
    }

    /// Decodes one self-delimiting operation frame from the front of
    /// `input`, returning the operation and the number of bytes consumed.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let mut reader = ByteReader::new(input);
        let op_len = reader.read_u32()? as usize;
        let payload = reader.read_exact(op_len)?;
        let stored_crc = reader.read_u32()?;
        let consumed = reader.position();
        let computed_crc = crc32c(&input[0..consumed - 4]);
        if stored_crc != computed_crc {
            return Err(DecodeError::ChecksumMismatch {
                context: "wal_operation",
                expected: stored_crc,
                computed: computed_crc,
            });
        }
        let operation = Self::decode_payload(payload)?;
        Ok((operation, consumed))
    }
}

fn read_bool(input: &mut ByteReader<'_>, field: &'static str) -> Result<bool, DecodeError> {
    match input.read_u8()? {
        0x00 => Ok(false),
        0x01 => Ok(true),
        other => Err(DecodeError::UnknownDiscriminant {
            field,
            value: other as u32,
        }),
    }
}

/// One decoded, validated WAL transaction: a strictly sequenced, atomic
/// batch of one or more operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Strictly increasing transaction sequence number (starts at `1`).
    pub sequence: u64,
    /// The transaction's operations, in on-disk order.
    pub operations: Vec<Operation>,
}

impl Transaction {
    fn encode_body(&self) -> Result<Vec<u8>, EncodeError> {
        // Reject an empty or over-long operation list before writing
        // anything, rather than silently truncating `operations.len()`
        // through a lossy `as u16` cast (which would otherwise let a
        // caller-supplied count greater than `u16::MAX` wrap around to a
        // smaller, wrong `op_count` on the wire).
        if self.operations.is_empty() {
            return Err(EncodeError::EmptyTransaction {
                sequence: self.sequence,
            });
        }
        if self.operations.len() > WAL_MAX_OPS_PER_TX as usize {
            return Err(EncodeError::TooManyOperations {
                sequence: self.sequence,
                op_count: self.operations.len(),
                max: WAL_MAX_OPS_PER_TX,
            });
        }
        let mut out = ByteWriter::new();
        out.write_u64(self.sequence);
        // Checked exact: bounded above by `WAL_MAX_OPS_PER_TX` (a `u16`)
        // just above.
        out.write_u16(self.operations.len() as u16);
        for operation in &self.operations {
            out.write_bytes(&operation.encode()?);
        }
        Ok(out.into_bytes())
    }

    /// Encodes this transaction as a self-delimiting `tx_len || tx_body ||
    /// tx_crc32c` frame.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let body = self.encode_body()?;
        let mut out = ByteWriter::new();
        out.write_u32(body.len() as u32);
        out.write_bytes(&body);
        let crc = crc32c(out.as_bytes());
        out.write_u32(crc);
        Ok(out.into_bytes())
    }

    fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = ByteReader::new(body);
        let sequence = reader.read_u64()?;
        let op_count = reader.read_u16()?;
        if op_count == 0 {
            return Err(DecodeError::EmptyTransaction { sequence });
        }
        if op_count > WAL_MAX_OPS_PER_TX {
            return Err(DecodeError::TooManyOperations {
                sequence,
                op_count,
                max: WAL_MAX_OPS_PER_TX,
            });
        }
        let mut operations = Vec::with_capacity(op_count as usize);
        let mut cursor = reader.position();
        for _ in 0..op_count {
            let remaining = body.get(cursor..).ok_or(DecodeError::Truncated {
                needed: 1,
                available: 0,
            })?;
            let (operation, consumed) = Operation::decode(remaining)?;
            cursor += consumed;
            operations.push(operation);
        }
        if cursor != body.len() {
            return Err(DecodeError::UnconsumedBytes {
                context: "wal_transaction",
                declared: body.len(),
                consumed: cursor,
            });
        }
        Ok(Transaction {
            sequence,
            operations,
        })
    }
}

/// The result of scanning exactly one transaction attempt from the current
/// WAL cursor position.
enum TransactionScan {
    /// A structurally complete, checksum-valid transaction.
    Complete(Transaction, usize),
    /// A structurally incomplete final transaction (the torn-tail case):
    /// `usize` is the number of trailing bytes to discard.
    TornTail(usize),
}

/// Scans exactly one transaction attempt from the front of `bytes`,
/// distinguishing a torn tail (discarded, not an error) from corruption (a
/// structurally complete frame with an invalid checksum, or invalid
/// contents once validated), per
/// `docs/filelog-checkpoint-format.md#torn-tail-versus-corruption`.
fn scan_one_transaction(bytes: &[u8]) -> Result<TransactionScan, DecodeError> {
    let remaining = bytes.len();
    if remaining < 4 {
        return Ok(TransactionScan::TornTail(remaining));
    }
    let mut reader = ByteReader::new(bytes);
    let tx_len = reader.read_u32()? as usize;
    let after_len = remaining - 4;
    let needed = tx_len
        .checked_add(4)
        .ok_or(DecodeError::ArithmeticOverflow {
            context: "tx_len + trailing crc",
        })?;
    if after_len < needed {
        return Ok(TransactionScan::TornTail(remaining));
    }
    let body = reader.read_exact(tx_len)?;
    let stored_crc = reader.read_u32()?;
    let consumed = reader.position();
    let computed_crc = crc32c(&bytes[0..consumed - 4]);
    if stored_crc != computed_crc {
        return Err(DecodeError::ChecksumMismatch {
            context: "wal_transaction",
            expected: stored_crc,
            computed: computed_crc,
        });
    }
    let transaction = Transaction::decode_body(body)?;
    Ok(TransactionScan::Complete(transaction, consumed))
}

/// The fully decoded contents of one WAL generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalContents {
    /// The generation number recorded in the WAL header.
    pub generation: u64,
    /// Every complete, validated transaction, in strictly increasing
    /// sequence order.
    pub transactions: Vec<Transaction>,
    /// The number of trailing bytes discarded as a torn tail; `0` if the
    /// WAL ended cleanly.
    pub torn_tail_bytes: usize,
}

/// Encodes a complete WAL file (header plus every transaction) for the
/// given `generation`.
pub fn encode_wal(generation: u64, transactions: &[Transaction]) -> Result<Vec<u8>, EncodeError> {
    let mut out = ByteWriter::new();
    out.write_bytes(WAL_MAGIC);
    out.write_u16(FORMAT_VERSION);
    out.write_u16(0); // flags, reserved
    out.write_u64(generation);
    let header_crc = crc32c(out.as_bytes());
    out.write_u32(header_crc);
    for transaction in transactions {
        out.write_bytes(&transaction.encode()?);
    }
    Ok(out.into_bytes())
}

/// Decodes and validates a complete WAL file: the header (no torn-tail
/// leniency), then every transaction in strictly increasing sequence order,
/// applying the torn-tail-versus-corruption distinction to the final
/// transaction only.
pub fn decode_wal(bytes: &[u8]) -> Result<WalContents, DecodeError> {
    if bytes.len() < WAL_HEADER_LEN {
        return Err(DecodeError::Truncated {
            needed: WAL_HEADER_LEN,
            available: bytes.len(),
        });
    }
    let mut reader = ByteReader::new(bytes);
    let magic = reader.read_exact(8)?;
    if magic != WAL_MAGIC {
        return Err(DecodeError::BadMagic {
            context: "WAL header",
        });
    }
    let format_version = reader.read_u16()?;
    if format_version != FORMAT_VERSION {
        return Err(DecodeError::UnsupportedFormatVersion {
            context: "WAL header",
            found: format_version,
        });
    }
    let flags = reader.read_u16()?;
    if flags != 0 {
        return Err(DecodeError::ReservedFieldNonZero {
            field: "wal_header.flags",
            value: flags as u64,
        });
    }
    let generation = reader.read_u64()?;
    let stored_header_crc = reader.read_u32()?;
    let computed_header_crc = crc32c(&bytes[0..20]);
    if stored_header_crc != computed_header_crc {
        return Err(DecodeError::ChecksumMismatch {
            context: "wal_header",
            expected: stored_header_crc,
            computed: computed_header_crc,
        });
    }

    let mut cursor = WAL_HEADER_LEN;
    let mut transactions = Vec::new();
    let mut expected_sequence: u64 = 1;
    let mut torn_tail_bytes = 0usize;
    while cursor < bytes.len() {
        match scan_one_transaction(&bytes[cursor..])? {
            TransactionScan::TornTail(n) => {
                torn_tail_bytes = n;
                break;
            }
            TransactionScan::Complete(transaction, consumed) => {
                if transaction.sequence != expected_sequence {
                    return Err(DecodeError::SequenceOutOfOrder {
                        expected: expected_sequence,
                        found: transaction.sequence,
                    });
                }
                expected_sequence =
                    expected_sequence
                        .checked_add(1)
                        .ok_or(DecodeError::ArithmeticOverflow {
                            context: "wal transaction sequence",
                        })?;
                cursor += consumed;
                transactions.push(transaction);
            }
        }
    }

    Ok(WalContents {
        generation,
        transactions,
        torn_tail_bytes,
    })
}
