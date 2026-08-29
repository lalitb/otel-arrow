// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared constants, normalized value types, and checked byte-level codec
//! helpers for the version-1 filelog checkpoint format.
//!
//! Every constant and field width here mirrors
//! `docs/filelog-checkpoint-format.md` exactly; that document is the
//! authoritative source and this module must be kept in lockstep with it.
//!
//! All multi-byte integers are big-endian. All variable-length fields are
//! read and written through [`ByteReader`] / [`ByteWriter`], which perform
//! checked-arithmetic bounds validation before any slicing or allocation, so
//! a corrupted or adversarial length prefix can never cause an out-of-bounds
//! slice, an integer overflow, or an unbounded allocation.

use crc::{CRC_32_ISCSI, Crc};
use sha2::{Digest, Sha256};

use super::error::{DecodeError, EncodeError};

/// On-disk format version for the `CURRENT` marker, snapshot, and WAL
/// headers. Independent of [`FRAMING_PROFILE_VERSION`].
pub const FORMAT_VERSION: u16 = 1;

/// Fixed transaction-envelope version within [`FORMAT_VERSION`] `1`.
pub const TX_ENVELOPE_VERSION: u16 = 1;

/// Canonical-serialization/digest recipe version for the framing profile.
///
/// This first version already binds the decode-error policy in addition to
/// the identity and framing inputs, so a receiver cannot resume durable
/// state under different emitted-body or decoding-failure semantics.
pub const FRAMING_PROFILE_VERSION: u16 = 1;
/// Current raw-prefix fingerprint algorithm/profile.
pub const FINGERPRINT_PROFILE_VERSION: u16 = 1;

/// Magic bytes for the `CURRENT` marker file.
pub const CURRENT_MAGIC: &[u8; 8] = b"FLOGCUR\0";
/// Magic bytes for a snapshot file header.
pub const SNAPSHOT_MAGIC: &[u8; 8] = b"FLOGSNP\0";
/// Magic bytes for a snapshot file footer.
pub const SNAPSHOT_FOOTER_MAGIC: &[u8; 8] = b"FLOGSFT\0";
/// Magic bytes for a WAL file header.
pub const WAL_MAGIC: &[u8; 8] = b"FLOGWAL\0";
/// Magic bytes for a WAL transaction's fixed 36-byte envelope header.
pub const TX_MAGIC: &[u8; 8] = b"FLOGTXN\0";

/// Domain separator for the namespace digest (see
/// `docs/filelog-checkpoint-format.md`, "Namespace digest").
const NAMESPACE_DIGEST_DOMAIN: &[u8] = b"otel-arrow-filelog-checkpoint-namespace-v1\0";
/// Domain separator for the committed-frontier guard digest (see
/// `docs/filelog-checkpoint-format.md`, "Committed-frontier guard").
const FRONTIER_GUARD_DOMAIN: &[u8] = b"otel-arrow-filelog-frontier-guard-v1\0";
/// Domain separator for the `AdvisoryPath` digest (see
/// `docs/filelog-checkpoint-format.md`, "`AdvisoryPath` encoding").
const ADVISORY_PATH_DIGEST_DOMAIN: &[u8] = b"otel-arrow-filelog-advisory-path-v1\0";

/// Maximum stored fingerprint length, in bytes (`u16::MAX`).
pub const FINGERPRINT_MAX_BYTES: usize = u16::MAX as usize;
/// Maximum stored advisory-path length, in bytes (`AdvisoryPath.stored_path_bytes`).
pub const ADVISORY_PATH_STORED_MAX_BYTES: usize = 4096;
/// Fixed encoded overhead of an [`AdvisoryPath`] value, excluding
/// `stored_path_bytes`: `path_kind` (1) + `path_flags` (1) +
/// `full_path_len` (8) + `stored_path_len` (2) + `full_path_digest` (32).
pub const ADVISORY_PATH_FIXED_BYTES: usize = 1 + 1 + 8 + 2 + 32;
/// `AdvisoryPath.path_flags` bit: the stored bytes are the final
/// `ADVISORY_PATH_STORED_MAX_BYTES` bytes of a longer complete path.
pub const ADVISORY_PATH_TRUNCATED: u8 = 0x01;
/// All `AdvisoryPath.path_flags` bits not yet assigned in v1; MUST be zero.
pub const ADVISORY_PATH_FLAGS_RESERVED_MASK: u8 = !ADVISORY_PATH_TRUNCATED;
/// Maximum stored audit-reason length, in bytes.
pub const AUDIT_REASON_MAX_BYTES: usize = 1024;
/// Maximum stored administrative namespace-id length, in bytes.
pub const NAMESPACE_ID_MAX_BYTES: usize = 255;
/// Maximum stored framing-profile regex pattern length, in bytes.
pub const FRAMING_PATTERN_MAX_BYTES: usize = 4096;
/// Maximum number of operations a progress-only WAL transaction may
/// contain (every operation is `update_progress`).
pub const WAL_MAX_OPS_PER_TX: u16 = 4096;
/// Maximum number of operations a non-progress WAL transaction (no
/// operation is `update_progress`) may contain.
pub const WAL_MAX_NON_PROGRESS_OPS_PER_TX: u16 = 256;
/// Hard cap on the encoded body of any single WAL transaction, of either
/// class, enforced before allocation or write.
pub const WAL_MAX_TX_BODY_BYTES: u64 = 16 * 1024 * 1024;
/// Fixed width of the WAL transaction envelope header, in bytes.
pub const TX_HEADER_BYTES: usize = 36;
/// Trailing frame-CRC width appended after a transaction's body.
pub const TX_FRAME_CRC_BYTES: usize = 4;
/// Fixed width of the raw window a [`CommittedFrontierGuard`] covers.
pub const COMMITTED_FRONTIER_GUARD_WINDOW_BYTES: u16 = 64;
/// Fixed encoded width of a [`CommittedFrontierGuard`]: `window_len: u16`
/// plus a 32-byte digest.
pub const COMMITTED_FRONTIER_GUARD_LEN: usize = 2 + 32;

// The constants below are the exact worst-case/minimum-case encoded sizes
// from `docs/filelog-checkpoint-format.md`, "Maximum encoded lengths
// (summary)". Each is independent of receiver configuration (they use
// `FINGERPRINT_MAX_BYTES` and `ADVISORY_PATH_STORED_MAX_BYTES`, the
// format's own absolute field maximums, not a configured
// `identity.fingerprint_bytes`); [`super::store::limits`] derives the
// configuration-dependent bounds a running store actually enforces, and its
// tests assert those formulas agree with the fixed values here at the
// format's own worst case.

/// Maximum encoded `payload` of one snapshot record: the widest
/// `Quarantined` record with a maximum committed-frontier guard, locator,
/// `Continuation` framing resume, maximum fingerprint, and maximum
/// (truncated) advisory path.
pub const SNAPSHOT_MAX_RECORD_PAYLOAD_BYTES: u64 = 69854;
/// [`SNAPSHOT_MAX_RECORD_PAYLOAD_BYTES`] plus its `record_len` prefix and
/// `record_crc32c` suffix.
pub const SNAPSHOT_MAX_RECORD_FRAME_BYTES: u64 = 69862;
/// Minimum encoded transaction body: one minimal `update_metadata`
/// operation frame (`op_code`, `file_id`, `presence_flags` clear,
/// `last_seen_time_unix_nano`, no advisory path) with its `op_len` prefix
/// and `op_crc32c` suffix.
pub const TX_MIN_BODY_BYTES: u32 = 34;
/// [`TX_MIN_BODY_BYTES`] plus the fixed transaction envelope header and
/// trailing frame CRC.
pub const TX_MIN_FRAME_BYTES: u32 =
    TX_HEADER_BYTES as u32 + TX_MIN_BODY_BYTES + TX_FRAME_CRC_BYTES as u32;
/// Maximum encoded payload of any single WAL operation: `update_fingerprint`
/// carrying two maximum fingerprints.
pub const MAX_OPERATION_PAYLOAD_BYTES: u64 = 131095;
/// [`MAX_OPERATION_PAYLOAD_BYTES`] plus its `op_len` prefix and `op_crc32c`
/// suffix.
pub const MAX_OPERATION_FRAME_BYTES: u64 = 131103;
/// Maximum encoded payload of a `register_file` operation: maximum
/// committed-frontier guard, locator, maximum fingerprint, the required
/// `Clean` framing resume, and maximum (truncated) advisory path.
pub const REGISTER_FILE_MAX_OP_PAYLOAD_BYTES: u64 = 69812;
/// [`WAL_MAX_TX_BODY_BYTES`] plus the fixed transaction envelope header and
/// trailing frame CRC: the hard ceiling on any single encoded transaction.
pub const WAL_MAX_TX_FRAME_BYTES: u64 =
    TX_HEADER_BYTES as u64 + WAL_MAX_TX_BODY_BYTES + TX_FRAME_CRC_BYTES as u64;
/// Maximum encoded payload of an `update_progress` operation: maximum
/// committed-frontier guard and the widest (`Continuation`) framing resume.
/// `update_progress` never carries an advisory path.
pub const UPDATE_PROGRESS_MAX_OP_PAYLOAD_BYTES: u64 = 101;
/// [`UPDATE_PROGRESS_MAX_OP_PAYLOAD_BYTES`] plus its `op_len` prefix and
/// `op_crc32c` suffix.
pub const UPDATE_PROGRESS_MAX_OP_FRAME_BYTES: u64 = 109;
/// Maximum encoded body of a progress-only transaction:
/// `WAL_MAX_OPS_PER_TX` maximum `update_progress` operation frames.
pub const MAX_PROGRESS_TX_BODY_BYTES: u64 = 446464;
/// [`MAX_PROGRESS_TX_BODY_BYTES`] plus the fixed transaction envelope
/// header and trailing frame CRC.
pub const MAX_PROGRESS_TX_FRAME_BYTES: u64 = 446504;

/// The sole valid `reset_after_truncate` reason in this version: an explicit
/// `on_truncate: read_new` policy decision.
pub const TRUNCATE_RESET_REASON_READ_NEW: u16 = 0x0001;

/// Reserved reason-code value that an encoder MUST NOT produce.
pub const REASON_CODE_RESERVED: u16 = 0x0000;
/// Version-1 quarantine reason reserved by the format.
pub(crate) const QUARANTINE_REASON_RESERVED_V1: u16 = 0x0004;
/// Quarantine reason: malformed input was rejected by the decode `fail`
/// policy.
pub const QUARANTINE_REASON_DECODE: u16 = 0x0001;
/// Quarantine reason: recovery evidence existed but could not be inherited
/// unambiguously and the configured policy was `fail`.
pub const QUARANTINE_REASON_RECOVERY_MISMATCH: u16 = 0x0003;
/// Quarantine reason: observable truncation was detected under the `fail`
/// policy.
pub const QUARANTINE_REASON_TRUNCATE: u16 = 0x0002;

/// Whether a quarantine reason is reserved from version-1 encoder output.
#[must_use]
pub(crate) const fn quarantine_reason_is_reserved(reason_code: u16) -> bool {
    reason_code == REASON_CODE_RESERVED || reason_code == QUARANTINE_REASON_RESERVED_V1
}

/// Removal reason: a stale non-quarantined record (`Active` or
/// `RotatedFinalized`) was superseded by a new identity created for the
/// same runtime locator under recovery-mismatch handling. `removal_reason`
/// is opaque per the format; this named value only aids diagnostics and
/// never gates apply-time validation.
pub const REMOVAL_REASON_LOCATOR_SUPERSEDED: u16 = 0x0001;

/// `update_metadata` presence bit: an advisory path value is present.
///
/// `update_metadata` never carries a locator: the locator is immutable for
/// a given `file_id` in this version (see
/// `docs/filelog-checkpoint-format.md`, `update_metadata`).
pub const METADATA_PATH_PRESENT: u8 = 0x01;
/// All presence bits not yet assigned in v1; MUST be zero.
pub const METADATA_PRESENCE_RESERVED_MASK: u8 = !METADATA_PATH_PRESENT;

/// Opaque 128-bit durable file identity. Never a UUID-format value by
/// contract; simply 16 bytes of OS randomness assigned once at registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub [u8; 16]);

impl FileId {
    /// Builds a `FileId` from raw bytes (test/tooling helper).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

/// Durable lifecycle state discriminant (`lifecycle_state`,
/// `expected_prior_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// The file is actively tracked and its offset advances on Ack.
    Active,
    /// The file was finalized after a move/create rotation.
    RotatedFinalized,
    /// The file is durably quarantined pending operator action.
    Quarantined,
}

impl LifecycleState {
    pub(super) fn to_wire(self) -> u8 {
        match self {
            LifecycleState::Active => 0x01,
            LifecycleState::RotatedFinalized => 0x02,
            LifecycleState::Quarantined => 0x03,
        }
    }

    pub(super) fn from_wire(value: u8, field: &'static str) -> Result<Self, DecodeError> {
        match value {
            0x01 => Ok(LifecycleState::Active),
            0x02 => Ok(LifecycleState::RotatedFinalized),
            0x03 => Ok(LifecycleState::Quarantined),
            other => Err(DecodeError::UnknownDiscriminant {
                field,
                value: other as u32,
            }),
        }
    }
}

/// Normalized, platform-neutral runtime locator. Never a native `stat` or
/// `FILE_ID_INFO` structure; only the specific integer/byte-array values
/// needed for equality comparison, copied out explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Locator {
    /// No runtime locator recorded (for example, Windows identity
    /// unavailable).
    Unspecified,
    /// Normalized POSIX `(st_dev, st_ino)`, both widened to `u64`.
    PosixDevIno {
        /// Device identifier.
        dev: u64,
        /// Inode number.
        ino: u64,
    },
    /// Normalized Windows `(volume_serial, FILE_ID_INFO.FileId)`.
    WindowsVolumeFileId {
        /// 64-bit `FILE_ID_INFO.VolumeSerialNumber`.
        volume_serial: u64,
        /// 128-bit file identifier, stored verbatim.
        file_id: [u8; 16],
    },
}

impl Locator {
    pub(super) fn write(&self, out: &mut ByteWriter) {
        match *self {
            Locator::Unspecified => out.write_u8(0x00),
            Locator::PosixDevIno { dev, ino } => {
                out.write_u8(0x01);
                out.write_u64(dev);
                out.write_u64(ino);
            }
            Locator::WindowsVolumeFileId {
                volume_serial,
                file_id,
            } => {
                out.write_u8(0x02);
                out.write_u64(volume_serial);
                out.write_bytes(&file_id);
            }
        }
    }

    pub(super) fn read(input: &mut ByteReader<'_>) -> Result<Self, DecodeError> {
        let kind = input.read_u8()?;
        match kind {
            0x00 => Ok(Locator::Unspecified),
            0x01 => {
                let dev = input.read_u64()?;
                let ino = input.read_u64()?;
                Ok(Locator::PosixDevIno { dev, ino })
            }
            0x02 => {
                let volume_serial = input.read_u64()?;
                let mut file_id = [0u8; 16];
                file_id.copy_from_slice(input.read_exact(16)?);
                Ok(Locator::WindowsVolumeFileId {
                    volume_serial,
                    file_id,
                })
            }
            other => Err(DecodeError::UnknownDiscriminant {
                field: "locator.kind",
                value: other as u32,
            }),
        }
    }
}

/// Framing-resume state (INV-FR1): `Clean` or `Continuation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingResume {
    /// The next complete source unit starts a new logical record.
    Clean,
    /// A split logical record is in progress.
    Continuation {
        /// Source-byte offset where the original record started.
        record_start_offset: u64,
        /// The original record's known termination offset, or `0` for the
        /// scan-to-next-physical-LF termination mode (see
        /// `docs/filelog-checkpoint-format.md`, `FramingResume` encoding).
        record_end_offset: u64,
        /// Index of the next fragment to emit.
        next_fragment_index: u32,
    },
}

impl FramingResume {
    pub(super) fn write(&self, out: &mut ByteWriter) {
        match *self {
            FramingResume::Clean => out.write_u8(0x00),
            FramingResume::Continuation {
                record_start_offset,
                record_end_offset,
                next_fragment_index,
            } => {
                out.write_u8(0x01);
                out.write_u64(record_start_offset);
                out.write_u64(record_end_offset);
                out.write_u32(next_fragment_index);
            }
        }
    }

    pub(super) fn read(input: &mut ByteReader<'_>) -> Result<Self, DecodeError> {
        let kind = input.read_u8()?;
        match kind {
            0x00 => Ok(FramingResume::Clean),
            0x01 => {
                let record_start_offset = input.read_u64()?;
                let record_end_offset = input.read_u64()?;
                let next_fragment_index = input.read_u32()?;
                Ok(FramingResume::Continuation {
                    record_start_offset,
                    record_end_offset,
                    next_fragment_index,
                })
            }
            other => Err(DecodeError::UnknownDiscriminant {
                field: "framing_resume.kind",
                value: other as u32,
            }),
        }
    }
}

/// `AdvisoryPath.path_kind` discriminant (see
/// `docs/filelog-checkpoint-format.md`, "`AdvisoryPath` encoding").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AdvisoryPathKind {
    /// No advisory path is available; the complete native-byte
    /// representation is empty.
    Unavailable,
    /// Native Unix path bytes, with no UTF-8 requirement.
    UnixBytes,
    /// Native UTF-16 code units, serialized individually as little-endian
    /// `u16` values.
    WindowsUtf16Le,
}

impl AdvisoryPathKind {
    /// The wire-encoded `path_kind` discriminant.
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            AdvisoryPathKind::Unavailable => 0x00,
            AdvisoryPathKind::UnixBytes => 0x01,
            AdvisoryPathKind::WindowsUtf16Le => 0x02,
        }
    }

    fn from_wire(value: u8) -> Result<Self, DecodeError> {
        match value {
            0x00 => Ok(AdvisoryPathKind::Unavailable),
            0x01 => Ok(AdvisoryPathKind::UnixBytes),
            0x02 => Ok(AdvisoryPathKind::WindowsUtf16Le),
            other => Err(DecodeError::UnknownDiscriminant {
                field: "advisory_path.path_kind",
                value: other as u32,
            }),
        }
    }
}

/// Durable, bounded advisory-path evidence (see
/// `docs/filelog-checkpoint-format.md`, "`AdvisoryPath` encoding").
///
/// Bounded diagnostics only: never identity or progress evidence. Equality,
/// hashing, and ordering compare every field -- `path_kind`, the truncation
/// flag, `full_path_len`, `stored_path_bytes`, and `full_path_digest` -- so a
/// decoded value is never matched by its stored suffix alone.
///
/// A complete path (`full_path_len <= ADVISORY_PATH_STORED_MAX_BYTES`)
/// stores its entire complete native representation and a digest a decoder
/// can and does recompute and verify. A truncated path
/// (`full_path_len > ADVISORY_PATH_STORED_MAX_BYTES`) stores only the final
/// `ADVISORY_PATH_STORED_MAX_BYTES` bytes of the complete representation;
/// its digest is retained as opaque comparison/diagnostic evidence and
/// cannot authenticate the omitted prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdvisoryPath {
    kind: AdvisoryPathKind,
    truncated: bool,
    full_path_len: u64,
    stored_path_bytes: Vec<u8>,
    full_path_digest: [u8; 32],
}

impl AdvisoryPath {
    /// The explicit "no advisory path available" value: `path_kind ==
    /// Unavailable`, all lengths and flags zero, no stored bytes, and the
    /// digest recomputed over the empty complete-bytes representation.
    #[must_use]
    pub fn unavailable() -> Self {
        let full_path_digest = advisory_path_digest(AdvisoryPathKind::Unavailable, 0, &[]);
        Self {
            kind: AdvisoryPathKind::Unavailable,
            truncated: false,
            full_path_len: 0,
            stored_path_bytes: Vec::new(),
            full_path_digest,
        }
    }

    /// Builds a durable advisory path from the complete native Unix path
    /// bytes: arbitrary bytes, with no UTF-8 requirement.
    ///
    /// Computes the full length, digest, and stored suffix directly from
    /// `bytes`: hashing reads `bytes` in place and the stored suffix is a
    /// bounded (at most `ADVISORY_PATH_STORED_MAX_BYTES`) copy of its tail,
    /// so this never allocates a second buffer proportional to
    /// `bytes.len()`.
    ///
    /// Fails only if `bytes` is empty: a present advisory path always has a
    /// nonzero complete length. An oversized (`> 4096` byte) path is not an
    /// error in this version; it is durably truncated evidence instead.
    pub fn from_unix_bytes(bytes: &[u8]) -> Result<Self, EncodeError> {
        Self::from_native_bytes(AdvisoryPathKind::UnixBytes, bytes)
    }

    /// Builds a durable advisory path from the complete native Windows path
    /// as UTF-16 code units (serialized little-endian on the wire).
    ///
    /// Platform-independent: `units` may come from any source (a real
    /// `OsStr::encode_wide()` iterator collected by the caller, or a
    /// hand-built test vector), so this constructor is usable from
    /// cross-platform conformance tests as well as the real `cfg(windows)`
    /// runtime path.
    ///
    /// The digest is computed by hashing each code unit's little-endian
    /// bytes as it is visited, and the stored suffix is built only from the
    /// final `ADVISORY_PATH_STORED_MAX_BYTES / 2` code units: this never
    /// allocates a full-length little-endian byte buffer proportional to
    /// `units.len()`. Because every code unit contributes exactly two bytes
    /// and the stored maximum is even, the suffix boundary always falls on
    /// a code-unit boundary; a code unit is never split.
    ///
    /// Fails if `units` is empty (a present advisory path always has a
    /// nonzero complete length) or if `units.len() * 2` would overflow
    /// `u64`.
    pub fn from_windows_utf16_units(units: &[u16]) -> Result<Self, EncodeError> {
        if units.is_empty() {
            return Err(EncodeError::InvalidAdvisoryPath {
                reason: "a present advisory path's code units must be nonempty",
            });
        }
        let full_path_len = u64::try_from(units.len())
            .ok()
            .and_then(|len| len.checked_mul(2))
            .ok_or(EncodeError::InvalidAdvisoryPath {
                reason: "advisory path length overflows u64",
            })?;

        let mut hasher = Sha256::new();
        hasher.update(ADVISORY_PATH_DIGEST_DOMAIN);
        hasher.update([AdvisoryPathKind::WindowsUtf16Le.to_wire()]);
        hasher.update(full_path_len.to_be_bytes());
        for unit in units {
            hasher.update(unit.to_le_bytes());
        }
        let full_path_digest: [u8; 32] = hasher.finalize().into();

        let stored_units = (ADVISORY_PATH_STORED_MAX_BYTES / 2).min(units.len());
        let truncated = stored_units < units.len();
        let mut stored_path_bytes = Vec::with_capacity(stored_units * 2);
        for unit in &units[units.len() - stored_units..] {
            stored_path_bytes.extend_from_slice(&unit.to_le_bytes());
        }

        Ok(Self {
            kind: AdvisoryPathKind::WindowsUtf16Le,
            truncated,
            full_path_len,
            stored_path_bytes,
            full_path_digest,
        })
    }

    fn from_native_bytes(kind: AdvisoryPathKind, bytes: &[u8]) -> Result<Self, EncodeError> {
        if bytes.is_empty() {
            return Err(EncodeError::InvalidAdvisoryPath {
                reason: "a present advisory path's bytes must be nonempty",
            });
        }
        // Real paths never approach `u64::MAX`; the conversion is exact on
        // every supported target (`usize` is at most 64 bits wide).
        let full_path_len = bytes.len() as u64;
        let full_path_digest = advisory_path_digest(kind, full_path_len, bytes);
        let stored_start = bytes.len().saturating_sub(ADVISORY_PATH_STORED_MAX_BYTES);
        Ok(Self {
            kind,
            truncated: stored_start > 0,
            full_path_len,
            stored_path_bytes: bytes[stored_start..].to_vec(),
            full_path_digest,
        })
    }

    /// The `path_kind` discriminant.
    #[must_use]
    pub const fn kind(&self) -> AdvisoryPathKind {
        self.kind
    }

    /// Whether `stored_path_bytes` is the final
    /// `ADVISORY_PATH_STORED_MAX_BYTES` bytes of a longer complete path
    /// rather than the complete path itself.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The complete native-byte representation's length. Always `0` for
    /// `Unavailable`.
    #[must_use]
    pub const fn full_path_len(&self) -> u64 {
        self.full_path_len
    }

    /// The stored bytes: the complete path when not truncated, otherwise
    /// its final `ADVISORY_PATH_STORED_MAX_BYTES` bytes. Always empty for
    /// `Unavailable`.
    #[must_use]
    pub fn stored_path_bytes(&self) -> &[u8] {
        &self.stored_path_bytes
    }

    /// The domain-separated digest over the complete native-byte
    /// representation. Recomputable/verified when not truncated; opaque
    /// comparison evidence otherwise.
    #[must_use]
    pub const fn full_path_digest(&self) -> [u8; 32] {
        self.full_path_digest
    }

    /// Encoded size of this value in bytes: `ADVISORY_PATH_FIXED_BYTES +
    /// stored_path_bytes.len()`.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        ADVISORY_PATH_FIXED_BYTES + self.stored_path_bytes.len()
    }

    /// Deterministic total order used to select one distinguished
    /// matched-path binding among simultaneously eligible aliases for the
    /// same runtime locator (see `docs/filelog-receiver-phase1-spec.md`,
    /// "Discovery and matching").
    ///
    /// `path_kind` orders first. When neither side is truncated, the
    /// complete native path bytes are both available, so the remaining
    /// comparison is a literal lexicographic order over those bytes --
    /// exactly `(path_kind, complete native path bytes)`. When either side
    /// is truncated, the complete bytes are not both available, so the
    /// comparison instead uses the full length, digest, and stored suffix
    /// together, in that order -- never the stored suffix alone.
    #[must_use]
    pub(crate) fn distinguished_binding_order(&self, other: &Self) -> std::cmp::Ordering {
        self.kind.cmp(&other.kind).then_with(|| {
            if !self.truncated && !other.truncated {
                self.stored_path_bytes.cmp(&other.stored_path_bytes)
            } else {
                self.full_path_len
                    .cmp(&other.full_path_len)
                    .then_with(|| self.full_path_digest.cmp(&other.full_path_digest))
                    .then_with(|| self.stored_path_bytes.cmp(&other.stored_path_bytes))
            }
        })
    }

    pub(super) fn write(&self, out: &mut ByteWriter) {
        out.write_u8(self.kind.to_wire());
        out.write_u8(if self.truncated {
            ADVISORY_PATH_TRUNCATED
        } else {
            0
        });
        out.write_u64(self.full_path_len);
        // `stored_path_bytes.len() <= ADVISORY_PATH_STORED_MAX_BYTES`
        // (4096), always representable as `u16`.
        out.write_u16(self.stored_path_bytes.len() as u16);
        out.write_bytes(&self.stored_path_bytes);
        out.write_bytes(&self.full_path_digest);
    }

    pub(super) fn read(input: &mut ByteReader<'_>) -> Result<Self, DecodeError> {
        let kind = AdvisoryPathKind::from_wire(input.read_u8()?)?;
        let flags = input.read_u8()?;
        if flags & ADVISORY_PATH_FLAGS_RESERVED_MASK != 0 {
            return Err(DecodeError::ReservedFieldNonZero {
                field: "advisory_path.path_flags",
                value: u64::from(flags),
            });
        }
        let truncated = flags & ADVISORY_PATH_TRUNCATED != 0;
        let full_path_len = input.read_u64()?;
        let stored_path_len = input.read_u16()? as usize;
        if stored_path_len > ADVISORY_PATH_STORED_MAX_BYTES {
            return Err(DecodeError::LengthExceedsMaximum {
                field: "advisory_path.stored_path_bytes",
                declared: stored_path_len,
                max: ADVISORY_PATH_STORED_MAX_BYTES,
            });
        }
        let stored_path_bytes = input.read_exact(stored_path_len)?.to_vec();
        let mut full_path_digest = [0u8; 32];
        full_path_digest.copy_from_slice(input.read_exact(32)?);

        match kind {
            AdvisoryPathKind::Unavailable => {
                if truncated || full_path_len != 0 || stored_path_len != 0 {
                    return Err(DecodeError::InvalidAdvisoryPath {
                        field: "advisory_path",
                        reason: "Unavailable requires zero flags, length, and stored bytes",
                    });
                }
                let expected = advisory_path_digest(kind, 0, &[]);
                if full_path_digest != expected {
                    return Err(DecodeError::InvalidAdvisoryPath {
                        field: "advisory_path.full_path_digest",
                        reason: "Unavailable digest does not match the recomputed value",
                    });
                }
            }
            AdvisoryPathKind::UnixBytes | AdvisoryPathKind::WindowsUtf16Le => {
                if full_path_len == 0 {
                    return Err(DecodeError::InvalidAdvisoryPath {
                        field: "advisory_path.full_path_len",
                        reason: "a present advisory path must have a nonzero full length",
                    });
                }
                if kind == AdvisoryPathKind::WindowsUtf16Le
                    && (!full_path_len.is_multiple_of(2) || !stored_path_len.is_multiple_of(2))
                {
                    return Err(DecodeError::InvalidAdvisoryPath {
                        field: "advisory_path",
                        reason: "a Windows advisory path's lengths must be even",
                    });
                }
                let stored_max = ADVISORY_PATH_STORED_MAX_BYTES as u64;
                if full_path_len <= stored_max {
                    if truncated {
                        return Err(DecodeError::InvalidAdvisoryPath {
                            field: "advisory_path.path_flags",
                            reason: "a complete advisory path must not set TRUNCATED",
                        });
                    }
                    if stored_path_len as u64 != full_path_len {
                        return Err(DecodeError::InvalidAdvisoryPath {
                            field: "advisory_path.stored_path_len",
                            reason: "a complete advisory path's stored length must equal its full length",
                        });
                    }
                    let expected = advisory_path_digest(kind, full_path_len, &stored_path_bytes);
                    if full_path_digest != expected {
                        return Err(DecodeError::InvalidAdvisoryPath {
                            field: "advisory_path.full_path_digest",
                            reason: "digest does not match the recomputed value for a complete path",
                        });
                    }
                } else {
                    if !truncated {
                        return Err(DecodeError::InvalidAdvisoryPath {
                            field: "advisory_path.path_flags",
                            reason: "a path over the stored maximum must set TRUNCATED",
                        });
                    }
                    if stored_path_len as u64 != stored_max {
                        return Err(DecodeError::InvalidAdvisoryPath {
                            field: "advisory_path.stored_path_len",
                            reason: "a truncated advisory path's stored length must equal the stored maximum",
                        });
                    }
                    // The complete bytes are unavailable to a decoder for a
                    // truncated path; the digest is retained as opaque
                    // comparison/diagnostic evidence, never recomputed.
                }
            }
        }

        Ok(Self {
            kind,
            truncated,
            full_path_len,
            stored_path_bytes,
            full_path_digest,
        })
    }
}

/// Computes `full_path_digest = SHA-256(domain || path_kind: u8 ||
/// full_path_len: u64 BE || complete_native_path_bytes)`.
fn advisory_path_digest(
    kind: AdvisoryPathKind,
    full_path_len: u64,
    complete_bytes: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ADVISORY_PATH_DIGEST_DOMAIN);
    hasher.update([kind.to_wire()]);
    hasher.update(full_path_len.to_be_bytes());
    hasher.update(complete_bytes);
    hasher.finalize().into()
}

/// Fixed-width committed-frontier continuity evidence: a domain-separated
/// SHA-256 digest over the raw source bytes immediately preceding
/// `committed_offset`.
///
/// See `docs/filelog-checkpoint-format.md`, "Committed-frontier guard". The
/// digest is matching evidence, not authentication: it detects an
/// inherited-progress mismatch against the actual source bytes: it does not
/// protect against a hostile replacement that reproduces the checked
/// locator, prefix, size bound, and frontier window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedFrontierGuard {
    /// `min(committed_offset, COMMITTED_FRONTIER_GUARD_WINDOW_BYTES)`.
    pub window_len: u16,
    /// `SHA-256(domain || window_len: u16 BE || window_bytes)`.
    pub digest: [u8; 32],
}

impl CommittedFrontierGuard {
    /// Computes the guard for `committed_offset` given the exact raw source
    /// bytes `window_bytes` = `source[committed_offset - window_len,
    /// committed_offset)`, where `window_len = min(committed_offset, 64)`.
    ///
    /// Fails if `window_bytes.len()` does not equal the required window
    /// length for `committed_offset`; a caller must supply exactly that
    /// many bytes, never fewer (truncated) or more (unbounded).
    pub fn compute(committed_offset: u64, window_bytes: &[u8]) -> Result<Self, EncodeError> {
        let required_len =
            committed_offset.min(u64::from(COMMITTED_FRONTIER_GUARD_WINDOW_BYTES)) as usize;
        if window_bytes.len() != required_len {
            return Err(EncodeError::FieldTooLong {
                field: "committed_frontier_guard.window_bytes",
                len: window_bytes.len(),
                max: required_len,
            });
        }
        // `required_len <= 64`, always representable as `u16`.
        let window_len = required_len as u16;
        let mut hasher = Sha256::new();
        hasher.update(FRONTIER_GUARD_DOMAIN);
        hasher.update(window_len.to_be_bytes());
        hasher.update(window_bytes);
        let digest: [u8; 32] = hasher.finalize().into();
        Ok(Self { window_len, digest })
    }

    /// The guard for `committed_offset == 0`: an empty window.
    #[must_use]
    pub fn empty() -> Self {
        Self::compute(0, &[]).expect("offset 0 always has a valid empty window")
    }

    pub(super) fn write(&self, out: &mut ByteWriter) {
        out.write_u16(self.window_len);
        out.write_bytes(&self.digest);
    }

    pub(super) fn read(input: &mut ByteReader<'_>) -> Result<Self, DecodeError> {
        let window_len = input.read_u16()?;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(input.read_exact(32)?);
        Ok(Self { window_len, digest })
    }
}

/// A bounded runtime buffer of the exact raw source bytes ending at
/// [`Self::end_offset`], used to compute a real
/// [`CommittedFrontierGuard`] instead of fabricating one.
///
/// This is deliberately a distinct runtime type from the serialized
/// [`CommittedFrontierGuard`] digest: the guard is durable, one-way
/// (digest-only) evidence written to disk, while this type is the
/// in-memory raw bytes the reader/framer pipeline actually owns and never
/// persists directly. `bytes.len()` always equals
/// `min(end_offset, COMMITTED_FRONTIER_GUARD_WINDOW_BYTES)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedFrontierWindow {
    end_offset: u64,
    bytes: Vec<u8>,
}

impl CommittedFrontierWindow {
    /// Builds a window from `bytes`, the exact raw source bytes
    /// `source[end_offset - bytes.len(), end_offset)`.
    ///
    /// Fails if `bytes.len()` does not equal
    /// `min(end_offset, COMMITTED_FRONTIER_GUARD_WINDOW_BYTES)`: a caller
    /// must supply exactly that many real bytes, never a fabricated or
    /// truncated substitute.
    pub fn new(end_offset: u64, bytes: Vec<u8>) -> Result<Self, EncodeError> {
        let required_len =
            end_offset.min(u64::from(COMMITTED_FRONTIER_GUARD_WINDOW_BYTES)) as usize;
        if bytes.len() != required_len {
            return Err(EncodeError::FieldTooLong {
                field: "committed_frontier_window.bytes",
                len: bytes.len(),
                max: required_len,
            });
        }
        Ok(Self { end_offset, bytes })
    }

    /// The empty window at source offset `0`.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            end_offset: 0,
            bytes: Vec::new(),
        }
    }

    /// The source offset this window's bytes end at.
    #[must_use]
    pub const fn end_offset(&self) -> u64 {
        self.end_offset
    }

    /// The exact raw bytes this window retains.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Computes the real [`CommittedFrontierGuard`] for this window.
    pub fn guard(&self) -> Result<CommittedFrontierGuard, EncodeError> {
        CommittedFrontierGuard::compute(self.end_offset, &self.bytes)
    }
}

/// Computes the namespace digest binding an artifact to `checkpoint_id`.
///
/// See `docs/filelog-checkpoint-format.md`, "Namespace digest".
/// `checkpoint_id`'s raw bytes are hashed verbatim: no Unicode
/// normalization, case folding, or path escaping occurs.
#[must_use]
pub fn namespace_digest(checkpoint_id: &str) -> [u8; 32] {
    let id_bytes = checkpoint_id.as_bytes();
    // The exact `checkpoint.id` length is validated as `1..=255` bytes by
    // the durable store before this is ever called; the cast is exact for
    // any value that reaches this function.
    let len = id_bytes.len() as u16;
    let mut hasher = Sha256::new();
    hasher.update(NAMESPACE_DIGEST_DOMAIN);
    hasher.update(len.to_be_bytes());
    hasher.update(id_bytes);
    hasher.finalize().into()
}

/// Computes CRC-32C (Castagnoli), matching the iSCSI CRC-32C parametrization
/// used throughout this format. This is deliberately **not**
/// `crc32fast`'s default IEEE 802.3 polynomial.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
    CASTAGNOLI.checksum(bytes)
}

/// A cursor over an input byte slice that performs checked-arithmetic bounds
/// validation on every read. No method ever panics on adversarial input; all
/// failures return [`DecodeError`].
pub struct ByteReader<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    /// Wraps `input` for checked, position-tracked reads from the start.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    /// Number of bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    /// Current absolute read position, in bytes from the start of `input`.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Reads exactly `len` bytes, checking `len` against the number of bytes
    /// actually remaining before slicing.
    pub fn read_exact(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(DecodeError::ArithmeticOverflow {
                context: "read_exact offset",
            })?;
        if end > self.input.len() {
            return Err(DecodeError::Truncated {
                needed: len,
                available: self.remaining(),
            });
        }
        let slice = &self.input[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Reads one `u8`.
    pub fn read_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.read_exact(1)?[0])
    }

    /// Reads one big-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes(
            bytes.try_into().expect("checked length"),
        ))
    }

    /// Reads one big-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().expect("checked length"),
        ))
    }

    /// Reads one big-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().expect("checked length"),
        ))
    }

    /// Reads a `u16`-length-prefixed byte field, checking the declared
    /// length against both `max_len` and the bytes actually remaining
    /// before allocating or slicing.
    pub fn read_var_bytes(
        &mut self,
        field: &'static str,
        max_len: usize,
    ) -> Result<&'a [u8], DecodeError> {
        let len = self.read_u16()? as usize;
        if len > max_len {
            return Err(DecodeError::LengthExceedsMaximum {
                field,
                declared: len,
                max: max_len,
            });
        }
        self.read_exact(len)
    }

    /// Reads a `u16`-length-prefixed UTF-8 string field.
    pub fn read_var_string(
        &mut self,
        field: &'static str,
        max_len: usize,
    ) -> Result<&'a str, DecodeError> {
        let bytes = self.read_var_bytes(field, max_len)?;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8 { field })
    }
}

/// An append-only byte buffer used to encode checkpoint records,
/// transactions, and operations.
#[derive(Debug, Default)]
pub struct ByteWriter {
    buf: Vec<u8>,
}

impl ByteWriter {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Returns the accumulated bytes, consuming the writer.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Borrows the accumulated bytes so far.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Writes raw bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes one `u8`.
    pub fn write_u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    /// Writes one big-endian `u16`.
    pub fn write_u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes one big-endian `u32`.
    pub fn write_u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes one big-endian `u64`.
    pub fn write_u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a `u16`-length-prefixed byte field, rejecting a value longer
    /// than `max_len` before writing anything.
    pub fn write_var_bytes(
        &mut self,
        field: &'static str,
        bytes: &[u8],
        max_len: usize,
    ) -> Result<(), EncodeError> {
        if bytes.len() > max_len {
            return Err(EncodeError::FieldTooLong {
                field,
                len: bytes.len(),
                max: max_len,
            });
        }
        // `max_len` values in this format all fit in u16, so this cast is exact.
        self.write_u16(bytes.len() as u16);
        self.write_bytes(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: computing CRC-32C over the standard check string.
    /// Guarantees: the codec uses the Castagnoli parametrization (matching
    /// the published `CRC-32C("123456789") = 0xE3069283` reference vector),
    /// not `crc32fast`'s default IEEE 802.3 polynomial.
    #[test]
    fn crc32c_matches_published_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    /// Scenario: reading a `u32` when fewer than 4 bytes remain.
    /// Guarantees: the reader reports a structured `Truncated` error instead
    /// of panicking or silently zero-filling missing bytes.
    #[test]
    fn byte_reader_reports_truncation_without_panicking() {
        let mut reader = ByteReader::new(&[0x00, 0x01]);
        let err = reader.read_u32().expect_err("only two bytes available");
        assert!(matches!(
            err,
            DecodeError::Truncated {
                needed: 4,
                available: 2
            }
        ));
    }

    /// Scenario: a variable-length field declares a length larger than the
    /// field's documented maximum, even though the bytes are available.
    /// Guarantees: the declared-length check runs before any allocation or
    /// slicing driven by the untrusted length value.
    #[test]
    fn byte_reader_rejects_length_over_field_maximum() {
        let mut bytes = vec![0x00, 0x05];
        bytes.extend_from_slice(&[0u8; 5]);
        let mut reader = ByteReader::new(&bytes);
        let err = reader
            .read_var_bytes("test.field", 3)
            .expect_err("declared length 5 exceeds max 3");
        assert!(matches!(
            err,
            DecodeError::LengthExceedsMaximum {
                field: "test.field",
                declared: 5,
                max: 3,
            }
        ));
    }

    /// Scenario: computing the namespace digest for the authoritative
    /// conformance `checkpoint.id` value `app-logs`.
    /// Guarantees: the digest matches the published, independently
    /// generated vector exactly, byte-for-byte.
    #[test]
    fn namespace_digest_matches_conformance_vector() {
        let digest = namespace_digest("app-logs");
        assert_eq!(
            hex_digest(&digest),
            "400aa7032f9128c39cc7e1403b8745dcccf6c9a5acfc665e908f15e798ac9531"
        );
    }

    /// Scenario: computing the committed-frontier guard at offset zero
    /// (empty window) and at offset four with raw window bytes `abc\n`.
    /// Guarantees: both digests match the published, independently
    /// generated conformance vectors exactly, and `window_len` matches
    /// `min(committed_offset, 64)` in each case.
    #[test]
    fn committed_frontier_guard_matches_conformance_vectors() {
        let empty = CommittedFrontierGuard::compute(0, &[]).unwrap();
        assert_eq!(empty.window_len, 0);
        assert_eq!(
            hex_digest(&empty.digest),
            "be47d023a06e82fd6da2daa0631547d6eca297b7ac532cba6471ab90829ec5b9"
        );
        assert_eq!(empty, CommittedFrontierGuard::empty());

        let non_empty = CommittedFrontierGuard::compute(4, b"abc\n").unwrap();
        assert_eq!(non_empty.window_len, 4);
        assert_eq!(
            hex_digest(&non_empty.digest),
            "23321df310e76dad74d895ad8e8e99d64f331fa350d4117f1f818a755d0a306a"
        );
    }

    /// Scenario: computing a committed-frontier guard with a window length
    /// that does not match `min(committed_offset, 64)`.
    /// Guarantees: the codec refuses to encode mismatched evidence rather
    /// than silently hashing the wrong number of bytes.
    #[test]
    fn committed_frontier_guard_rejects_wrong_window_length() {
        let err = CommittedFrontierGuard::compute(4, b"ab").unwrap_err();
        assert!(matches!(
            err,
            EncodeError::FieldTooLong {
                field: "committed_frontier_guard.window_bytes",
                len: 2,
                max: 4,
            }
        ));
    }

    fn hex_digest(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Scenario: `AdvisoryPath::unavailable()` and a round trip through
    /// `write`/`read`.
    /// Guarantees: the encoded value has zero flags, length, and stored
    /// bytes, and its digest matches the conformance vector recomputed over
    /// `path_kind = 0`, `full_path_len = 0`, and empty complete bytes.
    #[test]
    fn advisory_path_unavailable_matches_conformance_vector_and_round_trips() {
        let value = AdvisoryPath::unavailable();
        assert_eq!(value.kind(), AdvisoryPathKind::Unavailable);
        assert!(!value.is_truncated());
        assert_eq!(value.full_path_len(), 0);
        assert!(value.stored_path_bytes().is_empty());
        assert_eq!(
            hex_digest(&value.full_path_digest()),
            "56f3da573bf6a2f1e65787aa7a70acba735cc73f423e988692d5d40fd39c6232"
        );
        assert_eq!(value.encoded_len(), ADVISORY_PATH_FIXED_BYTES);

        let mut out = ByteWriter::new();
        value.write(&mut out);
        let bytes = out.into_bytes();
        assert_eq!(bytes.len(), ADVISORY_PATH_FIXED_BYTES);
        let mut reader = ByteReader::new(&bytes);
        let decoded = AdvisoryPath::read(&mut reader).unwrap();
        assert_eq!(decoded, value);
    }

    /// Scenario: a complete Unix advisory path built from non-UTF-8 native
    /// bytes for `/var/log/app.log`.
    /// Guarantees: the digest matches the conformance vector, the value is
    /// not truncated, the stored bytes equal the complete input, and the
    /// value round-trips through the wire format exactly.
    #[test]
    fn advisory_path_unix_complete_non_utf8_matches_conformance_vector() {
        let mut bytes = b"/var/log/app.log".to_vec();
        // A non-UTF-8 native Unix path byte, appended so the fixture also
        // exercises "no UTF-8 requirement"; kept out of the digest vector
        // fixture used below by testing this shape separately.
        let value = AdvisoryPath::from_unix_bytes(b"/var/log/app.log").unwrap();
        assert_eq!(value.kind(), AdvisoryPathKind::UnixBytes);
        assert!(!value.is_truncated());
        assert_eq!(value.full_path_len(), 16);
        assert_eq!(value.stored_path_bytes(), b"/var/log/app.log");
        assert_eq!(
            hex_digest(&value.full_path_digest()),
            "337a8fdfc197d2f02179162dccb0e86c430452449e51368104bbd5cc98fca49b"
        );

        // No UTF-8 requirement: an arbitrary non-UTF-8 byte round-trips
        // byte-for-byte.
        bytes.push(0xFF);
        let non_utf8 = AdvisoryPath::from_unix_bytes(&bytes).unwrap();
        assert_eq!(non_utf8.stored_path_bytes(), bytes.as_slice());

        let mut out = ByteWriter::new();
        value.write(&mut out);
        let encoded = out.into_bytes();
        assert_eq!(encoded.len(), value.encoded_len());
        let mut reader = ByteReader::new(&encoded);
        let decoded = AdvisoryPath::read(&mut reader).unwrap();
        assert_eq!(decoded, value);
    }

    /// Scenario: a complete Windows advisory path built from UTF-16LE code
    /// units (via the platform-independent constructor) for `C:\logs\app.log`.
    /// Guarantees: the digest matches the conformance vector, lengths are
    /// even, and the value round-trips through the wire format exactly.
    #[test]
    fn advisory_path_windows_utf16le_matches_conformance_vector() {
        let units: Vec<u16> = "C:\\logs\\app.log".encode_utf16().collect();
        let value = AdvisoryPath::from_windows_utf16_units(&units).unwrap();
        assert_eq!(value.kind(), AdvisoryPathKind::WindowsUtf16Le);
        assert!(!value.is_truncated());
        assert_eq!(value.full_path_len(), 30);
        assert_eq!(
            hex_digest(&value.full_path_digest()),
            "eaf5f1242c984fbf5c1ec523bd5dd9d1fa8c21b7f09a9394ee7b74b3ecdb8357"
        );
        let mut expected_bytes = Vec::new();
        for unit in &units {
            expected_bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(value.stored_path_bytes(), expected_bytes.as_slice());

        let mut out = ByteWriter::new();
        value.write(&mut out);
        let encoded = out.into_bytes();
        let mut reader = ByteReader::new(&encoded);
        let decoded = AdvisoryPath::read(&mut reader).unwrap();
        assert_eq!(decoded, value);
    }

    /// Scenario: a Unix advisory path of exactly
    /// `ADVISORY_PATH_STORED_MAX_BYTES` (4096) bytes.
    /// Guarantees: the boundary is complete, not truncated: `stored_len ==
    /// full_len == 4096` and `TRUNCATED` is clear.
    #[test]
    fn advisory_path_exact_boundary_is_not_truncated() {
        let bytes = vec![b'x'; ADVISORY_PATH_STORED_MAX_BYTES];
        let value = AdvisoryPath::from_unix_bytes(&bytes).unwrap();
        assert!(!value.is_truncated());
        assert_eq!(value.full_path_len(), ADVISORY_PATH_STORED_MAX_BYTES as u64);
        assert_eq!(
            value.stored_path_bytes().len(),
            ADVISORY_PATH_STORED_MAX_BYTES
        );
        assert_eq!(value.stored_path_bytes(), bytes.as_slice());
    }

    /// Scenario: Unix advisory paths one byte over the stored maximum
    /// (4097) and comfortably over it (5000, matching the published
    /// conformance vector for 5,000 bytes of `0x78`).
    /// Guarantees: both are truncated, `stored_len == 4096` in both cases,
    /// the stored bytes are exactly the final 4,096-byte suffix, and the
    /// 5,000-byte digest matches the conformance vector even though the
    /// stored bytes only cover the tail.
    #[test]
    fn advisory_path_over_boundary_is_truncated_to_final_suffix() {
        let bytes_4097 = vec![b'y'; ADVISORY_PATH_STORED_MAX_BYTES + 1];
        let value_4097 = AdvisoryPath::from_unix_bytes(&bytes_4097).unwrap();
        assert!(value_4097.is_truncated());
        assert_eq!(
            value_4097.full_path_len(),
            (ADVISORY_PATH_STORED_MAX_BYTES + 1) as u64
        );
        assert_eq!(
            value_4097.stored_path_bytes().len(),
            ADVISORY_PATH_STORED_MAX_BYTES
        );
        assert_eq!(
            value_4097.stored_path_bytes(),
            &bytes_4097[bytes_4097.len() - ADVISORY_PATH_STORED_MAX_BYTES..]
        );

        let bytes_5000 = vec![0x78u8; 5000];
        let value_5000 = AdvisoryPath::from_unix_bytes(&bytes_5000).unwrap();
        assert!(value_5000.is_truncated());
        assert_eq!(value_5000.full_path_len(), 5000);
        assert_eq!(
            value_5000.stored_path_bytes().len(),
            ADVISORY_PATH_STORED_MAX_BYTES
        );
        assert_eq!(
            hex_digest(&value_5000.full_path_digest()),
            "4edffb8c0486f5658b188d349af1b47270dc02bc0459b60dbfd3c314d9ecffa2"
        );

        let mut out = ByteWriter::new();
        value_5000.write(&mut out);
        let encoded = out.into_bytes();
        assert_eq!(
            encoded.len(),
            ADVISORY_PATH_FIXED_BYTES + ADVISORY_PATH_STORED_MAX_BYTES
        );
        let mut reader = ByteReader::new(&encoded);
        let decoded = AdvisoryPath::read(&mut reader).unwrap();
        assert_eq!(decoded, value_5000);
    }

    /// Scenario: decoding a complete (untruncated) advisory path whose
    /// stored `full_path_digest` does not match the recomputed digest over
    /// its stored bytes.
    /// Guarantees: the decoder recomputes and validates the digest for a
    /// complete path and fails closed on a mismatch.
    #[test]
    fn advisory_path_decode_rejects_corrupt_complete_digest() {
        let value = AdvisoryPath::from_unix_bytes(b"/var/log/app.log").unwrap();
        let mut out = ByteWriter::new();
        value.write(&mut out);
        let mut bytes = out.into_bytes();
        // Flip a bit in the last byte of the trailing digest.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let mut reader = ByteReader::new(&bytes);
        let err = AdvisoryPath::read(&mut reader).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidAdvisoryPath {
                field: "advisory_path.full_path_digest",
                ..
            }
        ));
    }

    /// Scenario: decoding an `AdvisoryPath` with a reserved `path_kind`
    /// (`0x03`..=`0xFF`).
    /// Guarantees: decode fails closed as an unknown discriminant rather
    /// than silently accepting an unrecognized kind.
    #[test]
    fn advisory_path_decode_rejects_reserved_kind() {
        let mut bytes = vec![0x03u8, 0x00];
        bytes.extend_from_slice(&0u64.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 32]);
        let mut reader = ByteReader::new(&bytes);
        let err = AdvisoryPath::read(&mut reader).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::UnknownDiscriminant {
                field: "advisory_path.path_kind",
                value: 0x03,
            }
        ));
    }

    /// Scenario: decoding an `AdvisoryPath` with a reserved `path_flags`
    /// bit set (any bit other than `TRUNCATED`).
    /// Guarantees: decode fails closed rather than silently ignoring an
    /// unrecognized flag.
    #[test]
    fn advisory_path_decode_rejects_reserved_flags() {
        let mut bytes = vec![0x00u8, 0x02];
        bytes.extend_from_slice(&0u64.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 32]);
        let mut reader = ByteReader::new(&bytes);
        let err = AdvisoryPath::read(&mut reader).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::ReservedFieldNonZero {
                field: "advisory_path.path_flags",
                value: 0x02,
            }
        ));
    }

    /// Scenario: decoding advisory paths with internally inconsistent
    /// lengths: a complete path whose `stored_path_len` does not equal
    /// `full_path_len`, and a `Windows` path with an odd `full_path_len`.
    /// Guarantees: both fail closed as structurally invalid rather than
    /// being silently accepted or truncated.
    #[test]
    fn advisory_path_decode_rejects_inconsistent_lengths() {
        // Complete path (full_path_len <= stored max) but a stored length
        // that does not match full_path_len.
        let mut bytes = vec![0x01u8, 0x00];
        bytes.extend_from_slice(&4u64.to_be_bytes());
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.extend_from_slice(&[0u8; 32]);
        let mut reader = ByteReader::new(&bytes);
        let err = AdvisoryPath::read(&mut reader).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidAdvisoryPath {
                field: "advisory_path.stored_path_len",
                ..
            }
        ));

        // Windows path with an odd full_path_len.
        let mut bytes = vec![0x02u8, 0x00];
        bytes.extend_from_slice(&3u64.to_be_bytes());
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.extend_from_slice(&[0u8; 32]);
        let mut reader = ByteReader::new(&bytes);
        let err = AdvisoryPath::read(&mut reader).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidAdvisoryPath {
                field: "advisory_path",
                ..
            }
        ));
    }

    /// Scenario: constructing a Windows advisory path from an odd number
    /// of bytes via a hand-truncated stored field is out of scope for the
    /// constructor (it always emits even lengths); this test instead
    /// exercises the decoder rejecting a stored `Windows` frame whose
    /// `stored_path_len` is odd while `full_path_len` is even.
    /// Guarantees: an odd `stored_path_len` fails closed even when
    /// `full_path_len` alone would otherwise look consistent.
    #[test]
    fn advisory_path_decode_rejects_odd_windows_stored_length() {
        let mut bytes = vec![0x02u8, 0x00];
        bytes.extend_from_slice(&4u64.to_be_bytes());
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.extend_from_slice(&[0u8; 32]);
        let mut reader = ByteReader::new(&bytes);
        let err = AdvisoryPath::read(&mut reader).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidAdvisoryPath {
                field: "advisory_path",
                ..
            }
        ));
    }

    /// Scenario: constructing an advisory path from empty native bytes or
    /// empty Windows code units.
    /// Guarantees: encoding fails closed (`InvalidAdvisoryPath`) instead of
    /// silently succeeding as `Unavailable` -- a genuine construction error
    /// is never success-folded into the "no path" value.
    #[test]
    fn advisory_path_rejects_empty_native_input() {
        assert!(matches!(
            AdvisoryPath::from_unix_bytes(&[]),
            Err(EncodeError::InvalidAdvisoryPath { .. })
        ));
        assert!(matches!(
            AdvisoryPath::from_windows_utf16_units(&[]),
            Err(EncodeError::InvalidAdvisoryPath { .. })
        ));
    }

    /// Scenario: two complete (untruncated) advisory paths of different
    /// native byte content are compared for the distinguished-binding
    /// selection order.
    /// Guarantees: the order is a literal lexicographic comparison of the
    /// complete native path bytes -- `b"aa"` sorts before `b"b"` even though
    /// it is longer, so the order never degenerates into a length-first
    /// comparison.
    #[test]
    fn distinguished_binding_order_is_lexicographic_not_length_first() {
        let shorter = AdvisoryPath::from_unix_bytes(b"b").unwrap();
        let longer_but_lexicographically_smaller = AdvisoryPath::from_unix_bytes(b"aa").unwrap();
        assert_eq!(
            longer_but_lexicographically_smaller.distinguished_binding_order(&shorter),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            shorter.distinguished_binding_order(&longer_but_lexicographically_smaller),
            std::cmp::Ordering::Greater
        );
    }

    /// Scenario: two structurally equal advisory paths (same kind,
    /// truncation, length, digest, and stored suffix) are compared.
    /// Guarantees: the order is reflexively `Equal`, matching `AdvisoryPath`
    /// equality.
    #[test]
    fn distinguished_binding_order_is_equal_for_equal_paths() {
        let a = AdvisoryPath::from_unix_bytes(b"same.log").unwrap();
        let b = AdvisoryPath::from_unix_bytes(b"same.log").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.distinguished_binding_order(&b), std::cmp::Ordering::Equal);
    }

    /// Scenario: two paths exceed the stored bound and are both truncated,
    /// with different complete lengths but suffixes that happen to collide
    /// in their trailing bytes.
    /// Guarantees: comparison never uses the stored suffix alone -- the
    /// differing complete lengths (part of the order key) still produce a
    /// deterministic, non-`Equal` order.
    #[test]
    fn distinguished_binding_order_for_truncated_paths_never_uses_suffix_alone() {
        let common_suffix = vec![b'x'; ADVISORY_PATH_STORED_MAX_BYTES];
        let mut shorter_prefixed = b"a/".to_vec();
        shorter_prefixed.extend_from_slice(&common_suffix);
        let mut longer_prefixed = b"aaaaaa/".to_vec();
        longer_prefixed.extend_from_slice(&common_suffix);

        let shorter = AdvisoryPath::from_unix_bytes(&shorter_prefixed).unwrap();
        let longer = AdvisoryPath::from_unix_bytes(&longer_prefixed).unwrap();
        assert!(shorter.is_truncated());
        assert!(longer.is_truncated());
        // Both encodings retain only the shared trailing suffix; the stored
        // bytes alone cannot distinguish them.
        assert_eq!(shorter.stored_path_bytes(), longer.stored_path_bytes());
        assert_ne!(shorter.full_path_len(), longer.full_path_len());
        assert_ne!(
            shorter.distinguished_binding_order(&longer),
            std::cmp::Ordering::Equal
        );
        // The order is a strict total order: swapping operands reverses it.
        assert_eq!(
            shorter.distinguished_binding_order(&longer),
            longer.distinguished_binding_order(&shorter).reverse()
        );
    }
}
