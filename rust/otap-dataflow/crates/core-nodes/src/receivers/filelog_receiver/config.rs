// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Configuration types for the filelog receiver (Phase 1).
//!
//! See [`docs/filelog-receiver.md`](../../../../../../docs/filelog-receiver.md),
//! Appendix C ("Complete Phase 1 configuration"), for the schema and
//! validation rules this module implements.
//!
//! Scope note: this module ships only the user-facing config types, their
//! defaults, semantic validation into a [`RuntimeConfig`], the shared
//! logical-record-size function, and the framing-profile canonical digest
//! integration (built on [`checkpoint::framing_profile`]). It performs no
//! filesystem I/O, discovery, framing, or checkpoint durability, and it
//! registers no component factory: [`FILELOG_RECEIVER_URN`] is exported so a
//! later stage can wire a `ReceiverFactory` without renaming anything here.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

use super::checkpoint::framing_profile;
use super::checkpoint::primitives::{
    ADVISORY_PATH_MAX_BYTES, FINGERPRINT_MAX_BYTES, FINGERPRINT_PROFILE_VERSION,
    NAMESPACE_ID_MAX_BYTES,
};
use super::checkpoint::store::limits::StoreLimits;

/// URN for the filelog receiver.
///
/// Not yet registered with a `ReceiverFactory` / `distributed_slice`; the
/// runtime (discovery, framing, checkpoint store) does not exist yet. See the
/// module-level scope note.
pub const FILELOG_RECEIVER_URN: &str = "urn:otel:receiver:filelog";

/// Namespace root matching Appendix B's layout:
/// `${engine.state_dir}/filelog/<checkpoint.id>/`.
const CHECKPOINT_NAMESPACE_ROOT: &str = "${engine.state_dir}/filelog";

/// Common `NAME_MAX` (maximum bytes in a single path component) enforced by
/// most POSIX filesystems (ext4, APFS, NTFS-under-WSL, etc). The percent-encoded
/// checkpoint-id path segment must stay within this bound even though the
/// checkpoint format's own `NAMESPACE_ID_MAX_BYTES` (256) is one byte larger,
/// so a config that validates cleanly never fails later with an OS-level
/// "file name too long" error.
const COMMON_PATH_SEGMENT_MAX_BYTES: usize = 255;

/// Effective maximum length, in bytes, of the resolved `checkpoint.id`'s
/// percent-encoded path segment: the tighter of the checkpoint format's
/// `NAMESPACE_ID_MAX_BYTES` (the on-disk `namespace_id_bytes` field bound)
/// and the common filesystem `NAME_MAX`. Neither the design nor
/// `docs/filelog-checkpoint-format.md` documents a different maximum, so the
/// tighter of the two applies.
const CHECKPOINT_ID_SEGMENT_MAX_BYTES: usize =
    if NAMESPACE_ID_MAX_BYTES <= COMMON_PATH_SEGMENT_MAX_BYTES {
        NAMESPACE_ID_MAX_BYTES
    } else {
        COMMON_PATH_SEGMENT_MAX_BYTES
    };

/// Minimum accepted `identity.fingerprint_bytes`.
const MIN_FINGERPRINT_BYTES: u64 = 16;

/// Maximum accepted `identity.fingerprint_bytes`: the checkpoint codec
/// stores `fingerprint` as a `u16`-length-prefixed byte field, so a value
/// larger than this can never round-trip through the durable format.
const MAX_FINGERPRINT_BYTES: u64 = FINGERPRINT_MAX_BYTES as u64;
/// Conservative process-memory ceiling for one identity reconciliation pass.
const IDENTITY_RECONCILIATION_BYTES_CEILING: u64 = 1024 * 1024 * 1024;
/// One worker-owned inventory, one inventory in the bounded event channel,
/// and one inventory under construction can coexist.
const DISCOVERY_MAX_SIMULTANEOUS_INVENTORIES: u64 = 3;
/// Candidate state, each simultaneous inventory key, and the temporary
/// evidence vector each retain a fingerprint payload at peak construction.
const DISCOVERY_CANDIDATE_FINGERPRINT_COPIES: u64 = 1 + DISCOVERY_MAX_SIMULTANEOUS_INVENTORIES + 1;
/// Candidate matched/resolved/advisory paths plus the temporary inventory
/// evidence path coexist at peak construction.
const DISCOVERY_CANDIDATE_PATH_COPIES: u64 = 4;
/// Conservative map/vector/allocation overhead per pending/in-flight candidate.
const IDENTITY_CANDIDATE_OVERHEAD_BYTES: u64 = 2048;
/// Additional allocation overhead for an in-flight candidate while its event
/// and durable operations are preflighted.
const IDENTITY_OPEN_CANDIDATE_OVERHEAD_BYTES: u64 = 4096;
/// An emitted event, original and cloned old/new fingerprint operations,
/// transaction body and frame, and transient applied record can coexist.
const IDENTITY_OPEN_CANDIDATE_FINGERPRINT_COPIES: u64 = 10;
/// The event's three paths, four metadata-operation encoding stages, and a
/// transient applied record require eight path payloads; retain two more as
/// allocator and operation-shape margin.
const IDENTITY_OPEN_CANDIDATE_PATH_COPIES: u64 = 10;
/// Conservative locator/index overhead per checkpoint row, excluding its
/// separately counted fingerprint and advisory-path payloads.
const IDENTITY_RECORD_INDEX_OVERHEAD_BYTES: u64 = 384;
/// Conservative discovery-map, live-locator inventory, and removal-event
/// storage per tracked locator while one earlier batch occupies the channel.
const DISCOVERY_TRACKED_OVERHEAD_BYTES: u64 = 1024;

/// Maximum accepted `identity.ignored_header_bytes`: the checkpoint codec
/// stores `ignored_header_bytes` as a `u32`, so a value larger than this can
/// never round-trip through the durable format.
const MAX_IGNORED_HEADER_BYTES: u64 = u32::MAX as u64;

/// Default `identity.fingerprint_bytes`.
const DEFAULT_FINGERPRINT_BYTES: u64 = 1000;
/// Default `identity.ignored_header_bytes`.
const DEFAULT_IGNORED_HEADER_BYTES: u64 = 0;
/// Default `max_recursion_depth`.
const DEFAULT_MAX_RECURSION_DEPTH: u32 = 64;
/// Default `discovery.poll_interval`.
const DEFAULT_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Default `ignore_older_than` (0 disables the filter).
const DEFAULT_IGNORE_OLDER_THAN: Duration = Duration::ZERO;
/// Maximum total include-plus-exclude glob count.
const MAX_DISCOVERY_PATTERNS: usize = 1024;
/// Maximum UTF-8 bytes in one discovery glob.
const MAX_DISCOVERY_PATTERN_BYTES: usize = 4096;
/// Maximum aggregate UTF-8 bytes retained for all discovery globs.
const MAX_DISCOVERY_PATTERN_TOTAL_BYTES: usize = 1024 * 1024;
/// Maximum traversal depth accepted from configuration.
const MAX_RECURSION_DEPTH: u32 = 1024;
/// Default `framing.max_line_bytes` (1 MiB).
const DEFAULT_MAX_LINE_BYTES: u64 = 1024 * 1024;
/// Default `framing.max_record_bytes` (1 MiB).
const DEFAULT_MAX_RECORD_BYTES: u64 = 1024 * 1024;
/// Default `framing.force_flush_period` (0 disables idle partial flushing).
const DEFAULT_FORCE_FLUSH_PERIOD: Duration = Duration::from_millis(500);
/// Default `framing.max_multiline_lines`.
const DEFAULT_MAX_MULTILINE_LINES: u32 = 500;
/// Default `limits.max_tracked_files`.
const DEFAULT_MAX_TRACKED_FILES: u32 = 10_000;
/// Default `limits.max_pending_candidates`.
const DEFAULT_MAX_PENDING_CANDIDATES: u32 = 10_000;
/// Default `limits.max_open_files`.
const DEFAULT_MAX_OPEN_FILES: u32 = 512;
/// Default `limits.max_read_bytes_per_turn` (128 KiB).
const DEFAULT_MAX_READ_BYTES_PER_TURN: u64 = 128 * 1024;
/// Default `batch.max_records`.
const DEFAULT_BATCH_MAX_RECORDS: u32 = 1024;
/// Maximum accepted `batch.max_records`; matches the OTAP Arrow log record
/// `u16` id space.
const MAX_BATCH_MAX_RECORDS: u32 = u16::MAX as u32;
/// Default `batch.max_bytes` (8 MiB).
const DEFAULT_BATCH_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Default `batch.max_flush_period`.
const DEFAULT_BATCH_MAX_FLUSH_PERIOD: Duration = Duration::from_secs(1);
/// Default `rotation.rotate_wait`, matching Fluent Bit's `Rotate_Wait`.
const DEFAULT_ROTATE_WAIT: Duration = Duration::from_secs(5);
/// Default `checkpoint.sync_interval` (0 means sync every Ack transaction).
const DEFAULT_CHECKPOINT_SYNC_INTERVAL: Duration = Duration::ZERO;
/// Default `checkpoint.compact_after_bytes` (64 MiB).
const DEFAULT_COMPACT_AFTER_BYTES: u64 = 64 * 1024 * 1024;
/// Default `checkpoint.compact_after_transactions`.
const DEFAULT_COMPACT_AFTER_TRANSACTIONS: u32 = 10_000;
/// Default `checkpoint.retention` (7 days; 0 retains indefinitely).
const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Default `checkpoint.ownership_timeout`.
const DEFAULT_OWNERSHIP_TIMEOUT: Duration = Duration::from_secs(30);
/// Default `checkpoint.max_consecutive_failures`.
const DEFAULT_MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Default `retry.max_attempts` (includes the first send).
const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 8;
/// Default `retry.initial_backoff`.
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
/// Default `retry.max_backoff`.
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Default `drain_timeout`.
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Registered semantic-convention attribute keys always attached to an
/// emitted record, counted by [`logical_record_size`].
const ATTR_KEY_LOG_FILE_PATH: &str = "log.file.path";
const ATTR_KEY_LOG_FILE_NAME: &str = "log.file.name";
/// Experimental metadata attribute keys, counted only when their matching
/// `metadata.*` flag is enabled.
const ATTR_KEY_RECORD_OFFSET: &str = "otel_arrow.filelog.record.offset";
const ATTR_KEY_RECORD_NUMBER: &str = "otel_arrow.filelog.record.number";
/// Experimental oversize-marker attribute keys, counted according to the
/// configured `max_log_size_behavior`.
const ATTR_KEY_FRAGMENT_ID: &str = "otel_arrow.filelog.fragment.id";
const ATTR_KEY_FRAGMENT_INDEX: &str = "otel_arrow.filelog.fragment.index";
const ATTR_KEY_FRAGMENT_LAST: &str = "otel_arrow.filelog.fragment.last";
const ATTR_KEY_RECORD_TRUNCATED: &str = "otel_arrow.filelog.record.truncated";

/// Conservative reserved bytes for a path-shaped attribute value (`log.file.path`,
/// `log.file.name`) whose exact runtime length is unknown at config-build
/// time. Matches common `PATH_MAX` on POSIX and Windows.
const RESERVED_PATH_ATTRIBUTE_VALUE_BYTES: u64 = 4096;
/// Conservative reserved bytes for a decimal-encoded `u64` attribute value
/// (source byte offset / record number), sized for the longest possible
/// `u64` (20 digits).
const RESERVED_DECIMAL_U64_VALUE_BYTES: u64 = 20;
/// Length in bytes of the fixed-width lowercase-hex fragment id value.
const FRAGMENT_ID_VALUE_BYTES: u64 = 64;
/// Conservative reserved bytes for a boolean attribute value (`"false"`).
const BOOLEAN_VALUE_BYTES: u64 = 5;
/// Conservative fixed per-record bookkeeping overhead not attributable to
/// any single attribute (Arrow builder / OTAP record bookkeeping). Matches
/// the "conservative fixed per-record overhead" language in the design.
const FIXED_PER_RECORD_OVERHEAD_BYTES: u64 = 128;

fn deserialize_byte_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    otap_df_config::byte_units::deserialize_u64(deserializer)?
        .ok_or_else(|| serde::de::Error::custom("byte size must not be null"))
}

/// Where the receiver should start reading a newly discovered file when no
/// checkpoint exists. A durable checkpoint always wins over this setting.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartAt {
    /// Start at the beginning of the file.
    Beginning,
    /// Start at the current end of the file (default); bytes appended
    /// afterward are eligible for normal Ack-gated reading.
    #[default]
    End,
}

/// Behavior when identity recovery cannot unambiguously match a candidate to
/// an existing checkpoint record.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnRecoveryMismatch {
    /// Start the new logical identity from the beginning of the file
    /// (default); biases toward duplicates over skipped data.
    #[default]
    Beginning,
    /// Start the new logical identity at the current end of the file; an
    /// explicit intentional-loss policy.
    SkipToEnd,
    /// Quarantine the file pending operator action.
    Fail,
}

/// Configured character encoding, applied before framing.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Encoding {
    /// UTF-8 (default).
    #[default]
    #[serde(rename = "utf-8")]
    Utf8,
    /// ASCII.
    #[serde(rename = "ascii")]
    Ascii,
    /// UTF-16, little-endian.
    #[serde(rename = "utf-16le")]
    Utf16Le,
    /// UTF-16, big-endian.
    #[serde(rename = "utf-16be")]
    Utf16Be,
    /// Raw bytes; no character validation, no byte-order-mark handling,
    /// frames physical lines on byte `0x0a`.
    #[serde(rename = "raw")]
    Raw,
}

/// Behavior when decoding source bytes under the configured [`Encoding`]
/// fails.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDecodeError {
    /// Emit the complete framed source slice as a bytes body and mark the
    /// record (default).
    #[default]
    PreserveRaw,
    /// Emit a lossy replacement and count it.
    Replace,
    /// Quarantine the file.
    Fail,
}

/// Versioned, RE2-compatible regex syntax profile shared by control-plane
/// validation and the runtime. Phase 1 supports exactly one profile.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum RegexProfile {
    /// RE2-compatible syntax profile version 1 (default; the only Phase 1
    /// profile).
    #[default]
    #[serde(rename = "re2-v1")]
    Re2V1,
}

impl RegexProfile {
    /// The versioned profile number persisted in the framing-profile digest.
    const fn version(self) -> u16 {
        match self {
            RegexProfile::Re2V1 => 1,
        }
    }
}

/// Oversize-record policy shared by the physical-line and logical-record
/// bounds.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxLogSizeBehavior {
    /// Preserve all input by emitting bounded fragments (default).
    #[default]
    Split,
    /// Emit the bounded prefix and discard through the logical record
    /// boundary.
    Truncate,
}

impl MaxLogSizeBehavior {
    fn to_framing_profile(self) -> framing_profile::MaxLogSizeBehavior {
        match self {
            MaxLogSizeBehavior::Split => framing_profile::MaxLogSizeBehavior::Split,
            MaxLogSizeBehavior::Truncate => framing_profile::MaxLogSizeBehavior::Truncate,
        }
    }
}

/// Behavior when copy-truncate rotation is detected.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnTruncate {
    /// Stop reading, durably quarantine the file, and report a high-severity
    /// error (default).
    #[default]
    Fail,
    /// Explicitly accept the recovery risk: increment `file_epoch`, reset
    /// offset and framing state, and resume reading the new stream.
    ReadNew,
}

/// Behavior when the downstream Nacks a batch and no further resend is
/// attempted (a permanent Nack, `RouteClosed`, `NodeShutdown`, or retry
/// exhaustion).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnNack {
    /// Terminate without advancing progress (default).
    #[default]
    Fail,
    /// Record explicit loss and advance past the batch.
    DropAndContinue,
}

/// Discovery scan cadence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// Periodic glob reconciliation interval.
    #[serde(
        default = "DiscoveryConfig::default_poll_interval",
        with = "humantime_serde"
    )]
    pub poll_interval: Duration,
}

impl DiscoveryConfig {
    const fn default_poll_interval() -> Duration {
        DEFAULT_DISCOVERY_POLL_INTERVAL
    }
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            poll_interval: Self::default_poll_interval(),
        }
    }
}

/// File identity matching configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    /// Number of raw matching-evidence bytes captured after
    /// `ignored_header_bytes`. Minimum 16.
    #[serde(
        default = "IdentityConfig::default_fingerprint_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub fingerprint_bytes: u64,
    /// Bytes skipped at the start of the file before fingerprinting begins.
    #[serde(
        default = "IdentityConfig::default_ignored_header_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub ignored_header_bytes: u64,
    /// Behavior when recovery matching is ambiguous or invalid.
    #[serde(default)]
    pub on_recovery_mismatch: OnRecoveryMismatch,
}

impl IdentityConfig {
    const fn default_fingerprint_bytes() -> u64 {
        DEFAULT_FINGERPRINT_BYTES
    }
    const fn default_ignored_header_bytes() -> u64 {
        DEFAULT_IGNORED_HEADER_BYTES
    }
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            fingerprint_bytes: Self::default_fingerprint_bytes(),
            ignored_header_bytes: Self::default_ignored_header_bytes(),
            on_recovery_mismatch: OnRecoveryMismatch::default(),
        }
    }
}

/// Multiline boundary configuration. Zero or one of `line_start_pattern` or
/// `line_end_pattern`; setting neither selects newline framing.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MultilineConfig {
    /// Versioned RE2-compatible regex syntax profile.
    #[serde(default)]
    pub regex_profile: RegexProfile,
    /// Buffer until the next match, which begins the next record.
    #[serde(default)]
    pub line_start_pattern: Option<String>,
    /// Buffer until a match, which is included in the record.
    #[serde(default)]
    pub line_end_pattern: Option<String>,
}

/// Framing, encoding-adjacent bounds and the multiline contract.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FramingConfig {
    /// Physical-line decoded buffer bound, in bytes.
    #[serde(
        default = "FramingConfig::default_max_line_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub max_line_bytes: u64,
    /// Logical-record body bound, in bytes.
    #[serde(
        default = "FramingConfig::default_max_record_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub max_record_bytes: u64,
    /// Oversize policy shared by both bounds above.
    #[serde(default)]
    pub max_log_size_behavior: MaxLogSizeBehavior,
    /// Idle time since the most recent physical line after which a buffered
    /// partial record is flushed. Zero disables idle partial flushing.
    #[serde(
        default = "FramingConfig::default_force_flush_period",
        with = "humantime_serde"
    )]
    pub force_flush_period: Duration,
    /// Multiline boundary configuration.
    #[serde(default)]
    pub multiline: MultilineConfig,
    /// Bounded multiline line-count limit.
    #[serde(default = "FramingConfig::default_max_multiline_lines")]
    pub max_multiline_lines: u32,
}

impl FramingConfig {
    const fn default_max_line_bytes() -> u64 {
        DEFAULT_MAX_LINE_BYTES
    }
    const fn default_max_record_bytes() -> u64 {
        DEFAULT_MAX_RECORD_BYTES
    }
    const fn default_force_flush_period() -> Duration {
        DEFAULT_FORCE_FLUSH_PERIOD
    }
    const fn default_max_multiline_lines() -> u32 {
        DEFAULT_MAX_MULTILINE_LINES
    }
}

impl Default for FramingConfig {
    fn default() -> Self {
        Self {
            max_line_bytes: Self::default_max_line_bytes(),
            max_record_bytes: Self::default_max_record_bytes(),
            max_log_size_behavior: MaxLogSizeBehavior::default(),
            force_flush_period: Self::default_force_flush_period(),
            multiline: MultilineConfig::default(),
            max_multiline_lines: Self::default_max_multiline_lines(),
        }
    }
}

/// Optional source-position metadata, off by default.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataConfig {
    /// Attach the first source byte offset represented by the record.
    #[serde(default)]
    pub include_file_record_offset: bool,
    /// Attach the source record number.
    #[serde(default)]
    pub include_file_record_number: bool,
}

/// Bounded discovery, admission, and read populations.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Maximum durable `file_id` checkpoint records tracked at once.
    #[serde(default = "LimitsConfig::default_max_tracked_files")]
    pub max_tracked_files: u32,
    /// Maximum matches retained while tracked-identity capacity is
    /// unavailable.
    #[serde(default = "LimitsConfig::default_max_pending_candidates")]
    pub max_pending_candidates: u32,
    /// Maximum files with an open operating-system handle.
    #[serde(default = "LimitsConfig::default_max_open_files")]
    pub max_open_files: u32,
    /// Maximum bytes read per file per worker turn.
    #[serde(
        default = "LimitsConfig::default_max_read_bytes_per_turn",
        deserialize_with = "deserialize_byte_size"
    )]
    pub max_read_bytes_per_turn: u64,
}

impl LimitsConfig {
    const fn default_max_tracked_files() -> u32 {
        DEFAULT_MAX_TRACKED_FILES
    }
    const fn default_max_pending_candidates() -> u32 {
        DEFAULT_MAX_PENDING_CANDIDATES
    }
    const fn default_max_open_files() -> u32 {
        DEFAULT_MAX_OPEN_FILES
    }
    const fn default_max_read_bytes_per_turn() -> u64 {
        DEFAULT_MAX_READ_BYTES_PER_TURN
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tracked_files: Self::default_max_tracked_files(),
            max_pending_candidates: Self::default_max_pending_candidates(),
            max_open_files: Self::default_max_open_files(),
            max_read_bytes_per_turn: Self::default_max_read_bytes_per_turn(),
        }
    }
}

/// Worker -> async batch shaping.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatchConfig {
    /// Maximum records per emitted batch. Must be `<= 65535` (the OTAP
    /// Arrow log record `u16` id space).
    #[serde(default = "BatchConfig::default_max_records")]
    pub max_records: u32,
    /// Maximum emitted-batch byte budget, using the shared
    /// [`logical_record_size`] function.
    #[serde(
        default = "BatchConfig::default_max_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub max_bytes: u64,
    /// Maximum time the worker holds a partial batch before flushing.
    #[serde(
        default = "BatchConfig::default_max_flush_period",
        with = "humantime_serde"
    )]
    pub max_flush_period: Duration,
}

impl BatchConfig {
    const fn default_max_records() -> u32 {
        DEFAULT_BATCH_MAX_RECORDS
    }
    const fn default_max_bytes() -> u64 {
        DEFAULT_BATCH_MAX_BYTES
    }
    const fn default_max_flush_period() -> Duration {
        DEFAULT_BATCH_MAX_FLUSH_PERIOD
    }
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_records: Self::default_max_records(),
            max_bytes: Self::default_max_bytes(),
            max_flush_period: Self::default_max_flush_period(),
        }
    }
}

/// Move/create and copy-truncate rotation handling.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RotationConfig {
    /// Best-effort inactivity window after EOF before a rotated identity is
    /// finalized.
    #[serde(
        default = "RotationConfig::default_rotate_wait",
        with = "humantime_serde"
    )]
    pub rotate_wait: Duration,
    /// Behavior when copy-truncate rotation is detected.
    #[serde(default)]
    pub on_truncate: OnTruncate,
}

impl RotationConfig {
    const fn default_rotate_wait() -> Duration {
        DEFAULT_ROTATE_WAIT
    }
}

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            rotate_wait: Self::default_rotate_wait(),
            on_truncate: OnTruncate::default(),
        }
    }
}

/// Durable checkpoint store configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointConfig {
    /// Explicit stable checkpoint identifier. Defaults to the receiver's
    /// configured node identity when omitted (resolved outside this type;
    /// see [`RuntimeConfig::from_config`]).
    #[serde(default)]
    pub id: Option<String>,
    /// Interval between durable syncs. Zero means sync every Ack
    /// transaction.
    #[serde(
        default = "CheckpointConfig::default_sync_interval",
        with = "humantime_serde"
    )]
    pub sync_interval: Duration,
    /// Compact once the progress log reaches this many bytes.
    #[serde(
        default = "CheckpointConfig::default_compact_after_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub compact_after_bytes: u64,
    /// Compact once the progress log reaches this many transactions.
    #[serde(default = "CheckpointConfig::default_compact_after_transactions")]
    pub compact_after_transactions: u32,
    /// Inactive durable state retention. Zero retains indefinitely.
    #[serde(
        default = "CheckpointConfig::default_retention",
        with = "humantime_serde"
    )]
    pub retention: Duration,
    /// Bounded wait for the checkpoint-namespace ownership lock.
    #[serde(
        default = "CheckpointConfig::default_ownership_timeout",
        with = "humantime_serde"
    )]
    pub ownership_timeout: Duration,
    /// Consecutive checkpoint append/sync/compaction failures before the
    /// source becomes terminal.
    #[serde(default = "CheckpointConfig::default_max_consecutive_failures")]
    pub max_consecutive_failures: u32,
}

impl CheckpointConfig {
    const fn default_sync_interval() -> Duration {
        DEFAULT_CHECKPOINT_SYNC_INTERVAL
    }
    const fn default_compact_after_bytes() -> u64 {
        DEFAULT_COMPACT_AFTER_BYTES
    }
    const fn default_compact_after_transactions() -> u32 {
        DEFAULT_COMPACT_AFTER_TRANSACTIONS
    }
    const fn default_retention() -> Duration {
        DEFAULT_RETENTION
    }
    const fn default_ownership_timeout() -> Duration {
        DEFAULT_OWNERSHIP_TIMEOUT
    }
    const fn default_max_consecutive_failures() -> u32 {
        DEFAULT_MAX_CONSECUTIVE_FAILURES
    }
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            id: None,
            sync_interval: Self::default_sync_interval(),
            compact_after_bytes: Self::default_compact_after_bytes(),
            compact_after_transactions: Self::default_compact_after_transactions(),
            retention: Self::default_retention(),
            ownership_timeout: Self::default_ownership_timeout(),
            max_consecutive_failures: Self::default_max_consecutive_failures(),
        }
    }
}

/// Retry budget for a Nacked or resent batch.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    /// Maximum total sends, including the first attempt.
    #[serde(default = "RetryConfig::default_max_attempts")]
    pub max_attempts: u32,
    /// Initial exponential-backoff delay.
    #[serde(
        default = "RetryConfig::default_initial_backoff",
        with = "humantime_serde"
    )]
    pub initial_backoff: Duration,
    /// Maximum backoff delay after doubling.
    #[serde(default = "RetryConfig::default_max_backoff", with = "humantime_serde")]
    pub max_backoff: Duration,
}

impl RetryConfig {
    const fn default_max_attempts() -> u32 {
        DEFAULT_RETRY_MAX_ATTEMPTS
    }
    const fn default_initial_backoff() -> Duration {
        DEFAULT_INITIAL_BACKOFF
    }
    const fn default_max_backoff() -> Duration {
        DEFAULT_MAX_BACKOFF
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: Self::default_max_attempts(),
            initial_backoff: Self::default_initial_backoff(),
            max_backoff: Self::default_max_backoff(),
        }
    }
}

/// User-facing filelog receiver configuration (Appendix C).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Glob include patterns. Required, non-empty.
    pub include: Vec<String>,
    /// Glob exclude patterns; excludes take precedence over includes.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Whether the scanner may descend below a directory named by an
    /// include.
    #[serde(default = "Config::default_recursive")]
    pub recursive: bool,
    /// Whether symlinked directories are traversed.
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Bounded traversal depth.
    #[serde(default = "Config::default_max_recursion_depth")]
    pub max_recursion_depth: u32,
    /// Where to start reading a newly discovered file. A checkpoint always
    /// wins over this setting.
    #[serde(default)]
    pub start_at: StartAt,
    /// Discovery scan cadence.
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Skip admission of a candidate whose modification time is older than
    /// this. Zero disables the filter.
    #[serde(
        default = "Config::default_ignore_older_than",
        with = "humantime_serde"
    )]
    pub ignore_older_than: Duration,
    /// File identity matching configuration.
    #[serde(default)]
    pub identity: IdentityConfig,
    /// Configured character encoding.
    #[serde(default)]
    pub encoding: Encoding,
    /// Behavior when decoding fails.
    #[serde(default)]
    pub on_decode_error: OnDecodeError,
    /// Framing bounds and multiline contract.
    #[serde(default)]
    pub framing: FramingConfig,
    /// Optional source-position metadata.
    #[serde(default)]
    pub metadata: MetadataConfig,
    /// Bounded discovery, admission, and read populations.
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Worker -> async batch shaping.
    #[serde(default)]
    pub batch: BatchConfig,
    /// Move/create and copy-truncate rotation handling.
    #[serde(default)]
    pub rotation: RotationConfig,
    /// Durable checkpoint store configuration.
    #[serde(default)]
    pub checkpoint: CheckpointConfig,
    /// Retry budget for a Nacked or resent batch.
    #[serde(default)]
    pub retry: RetryConfig,
    /// Behavior on a non-retryable Nack or retry exhaustion.
    #[serde(default)]
    pub on_nack: OnNack,
    /// Drain deadline budget.
    #[serde(default = "Config::default_drain_timeout", with = "humantime_serde")]
    pub drain_timeout: Duration,
}

impl Config {
    const fn default_recursive() -> bool {
        true
    }
    const fn default_max_recursion_depth() -> u32 {
        DEFAULT_MAX_RECURSION_DEPTH
    }
    const fn default_ignore_older_than() -> Duration {
        DEFAULT_IGNORE_OLDER_THAN
    }
    const fn default_drain_timeout() -> Duration {
        DEFAULT_DRAIN_TIMEOUT
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            recursive: Self::default_recursive(),
            follow_symlinks: false,
            max_recursion_depth: Self::default_max_recursion_depth(),
            start_at: StartAt::default(),
            discovery: DiscoveryConfig::default(),
            ignore_older_than: Self::default_ignore_older_than(),
            identity: IdentityConfig::default(),
            encoding: Encoding::default(),
            on_decode_error: OnDecodeError::default(),
            framing: FramingConfig::default(),
            metadata: MetadataConfig::default(),
            limits: LimitsConfig::default(),
            batch: BatchConfig::default(),
            rotation: RotationConfig::default(),
            checkpoint: CheckpointConfig::default(),
            retry: RetryConfig::default(),
            on_nack: OnNack::default(),
            drain_timeout: Self::default_drain_timeout(),
        }
    }
}

/// Error returned by [`logical_record_size`] when its checked-arithmetic
/// computation would overflow `u64`. Kept distinct from
/// `otap_df_config::error::Error` so the single shared runtime/config
/// function stays independent of the config crate's error type; config
/// build-time validation maps this into an `InvalidUserConfig` error, and a
/// later runtime batch-flushing stage can handle it however is appropriate
/// for that context (for example, always splitting/truncating before this
/// point is reached).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum LogicalSizeError {
    /// The computed logical record size would overflow `u64`.
    #[error("logical record size computation overflows u64")]
    Overflow,
}

/// The shared logical-size function used both to validate `max_line_bytes`
/// and `max_record_bytes` against `batch.max_bytes` at config-build time and,
/// later, by runtime batch flushing to size an actual emitted record or
/// fragment. Deliberately a single documented function so config validation
/// and runtime flushing can never disagree (Appendix C, "Emitted data
/// model").
///
/// Computes `body_bytes` plus attribute-key bytes, attribute-value bytes for
/// the attributes an emitted record always or conditionally carries, and a
/// conservative fixed per-record overhead. This is a documented logical
/// bound, not a claim about exact Arrow allocation size.
///
/// Uses checked arithmetic throughout and returns
/// [`LogicalSizeError::Overflow`] rather than saturating: a saturating sum
/// would let a `body_bytes` (or a `batch.max_bytes` bound compared against
/// it) near `u64::MAX` validate successfully by coincidentally saturating to
/// the same clamped value on both sides, even though the true logical size
/// is unrepresentable and could never be allocated.
pub(crate) fn logical_record_size(
    body_bytes: u64,
    metadata: &MetadataConfig,
    oversize_behavior: MaxLogSizeBehavior,
) -> Result<u64, LogicalSizeError> {
    let mut size = body_bytes;
    // Always-present registered semantic-convention attributes. Their value
    // length is unknown at config-build time (an arbitrary source path), so
    // reserve a conservative fixed budget per path-shaped attribute.
    size = size
        .checked_add(ATTR_KEY_LOG_FILE_PATH.len() as u64)
        .ok_or(LogicalSizeError::Overflow)?;
    size = size
        .checked_add(RESERVED_PATH_ATTRIBUTE_VALUE_BYTES)
        .ok_or(LogicalSizeError::Overflow)?;
    size = size
        .checked_add(ATTR_KEY_LOG_FILE_NAME.len() as u64)
        .ok_or(LogicalSizeError::Overflow)?;
    size = size
        .checked_add(RESERVED_PATH_ATTRIBUTE_VALUE_BYTES)
        .ok_or(LogicalSizeError::Overflow)?;
    if metadata.include_file_record_offset {
        size = size
            .checked_add(ATTR_KEY_RECORD_OFFSET.len() as u64)
            .ok_or(LogicalSizeError::Overflow)?;
        size = size
            .checked_add(RESERVED_DECIMAL_U64_VALUE_BYTES)
            .ok_or(LogicalSizeError::Overflow)?;
    }
    if metadata.include_file_record_number {
        size = size
            .checked_add(ATTR_KEY_RECORD_NUMBER.len() as u64)
            .ok_or(LogicalSizeError::Overflow)?;
        size = size
            .checked_add(RESERVED_DECIMAL_U64_VALUE_BYTES)
            .ok_or(LogicalSizeError::Overflow)?;
    }
    size = match oversize_behavior {
        MaxLogSizeBehavior::Split => {
            let mut fragment_overhead = ATTR_KEY_FRAGMENT_ID.len() as u64;
            fragment_overhead = fragment_overhead
                .checked_add(FRAGMENT_ID_VALUE_BYTES)
                .ok_or(LogicalSizeError::Overflow)?;
            fragment_overhead = fragment_overhead
                .checked_add(ATTR_KEY_FRAGMENT_INDEX.len() as u64)
                .ok_or(LogicalSizeError::Overflow)?;
            fragment_overhead = fragment_overhead
                .checked_add(RESERVED_DECIMAL_U64_VALUE_BYTES)
                .ok_or(LogicalSizeError::Overflow)?;
            fragment_overhead = fragment_overhead
                .checked_add(ATTR_KEY_FRAGMENT_LAST.len() as u64)
                .ok_or(LogicalSizeError::Overflow)?;
            fragment_overhead = fragment_overhead
                .checked_add(BOOLEAN_VALUE_BYTES)
                .ok_or(LogicalSizeError::Overflow)?;
            size.checked_add(fragment_overhead)
                .ok_or(LogicalSizeError::Overflow)?
        }
        MaxLogSizeBehavior::Truncate => {
            let mut truncate_overhead = ATTR_KEY_RECORD_TRUNCATED.len() as u64;
            truncate_overhead = truncate_overhead
                .checked_add(BOOLEAN_VALUE_BYTES)
                .ok_or(LogicalSizeError::Overflow)?;
            size.checked_add(truncate_overhead)
                .ok_or(LogicalSizeError::Overflow)?
        }
    };
    size.checked_add(FIXED_PER_RECORD_OVERHEAD_BYTES)
        .ok_or(LogicalSizeError::Overflow)
}

/// Validated, runtime-ready form of the filelog receiver configuration.
///
/// Parsing produces this from [`Config`] once; a later runtime stage
/// consumes it without re-validating.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeConfig {
    /// Glob include patterns.
    pub(crate) include: Vec<String>,
    /// Include patterns compiled once with path-separator-aware semantics.
    pub(crate) compiled_include: Vec<GlobMatcher>,
    /// Glob exclude patterns.
    pub(crate) exclude: Vec<String>,
    /// Exclude patterns compiled once with path-separator-aware semantics.
    pub(crate) compiled_exclude: Vec<GlobMatcher>,
    /// Whether the scanner may descend below a directory named by an
    /// include.
    pub(crate) recursive: bool,
    /// Whether symlinked directories are traversed.
    pub(crate) follow_symlinks: bool,
    /// Bounded traversal depth.
    pub(crate) max_recursion_depth: u32,
    /// Where to start reading a newly discovered file.
    pub(crate) start_at: StartAt,
    /// Discovery scan cadence.
    pub(crate) discovery: DiscoveryConfig,
    /// Admission age filter; zero disables it.
    pub(crate) ignore_older_than: Duration,
    /// File identity matching configuration.
    pub(crate) identity: IdentityConfig,
    /// Configured character encoding.
    pub(crate) encoding: Encoding,
    /// Behavior when decoding fails.
    pub(crate) on_decode_error: OnDecodeError,
    /// Framing bounds and multiline contract.
    pub(crate) framing: FramingConfig,
    /// Optional source-position metadata.
    pub(crate) metadata: MetadataConfig,
    /// Bounded discovery, admission, and read populations.
    pub(crate) limits: LimitsConfig,
    /// Worker -> async batch shaping.
    pub(crate) batch: BatchConfig,
    /// Move/create and copy-truncate rotation handling.
    pub(crate) rotation: RotationConfig,
    /// Durable checkpoint store configuration as configured (its `id` is
    /// superseded by the resolved [`Self::checkpoint_id`]).
    pub(crate) checkpoint: CheckpointConfig,
    /// Resolved stable checkpoint identifier: the configured
    /// `checkpoint.id`, or the caller-supplied default (typically the
    /// node's configured identity) when omitted.
    pub(crate) checkpoint_id: String,
    /// Resolved checkpoint namespace directory:
    /// `${engine.state_dir}/filelog/<percent-encoded checkpoint_id>/`.
    pub(crate) checkpoint_namespace_dir: PathBuf,
    /// Retry budget for a Nacked or resent batch.
    pub(crate) retry: RetryConfig,
    /// Behavior on a non-retryable Nack or retry exhaustion.
    pub(crate) on_nack: OnNack,
    /// Drain deadline budget.
    pub(crate) drain_timeout: Duration,
    /// SHA-256 framing-profile digest for the configured framing contract
    /// (encoding, multiline mode, size bounds, oversize policy, multiline
    /// line cap, idle flush period). Persisted checkpoint records compare
    /// this to detect an incompatible framing-profile change across
    /// restart.
    pub(crate) framing_profile_digest: [u8; 32],
    /// The compiled multiline boundary pattern, when one is configured.
    /// Compiled once here so the runtime never recompiles or re-validates
    /// it.
    pub(crate) compiled_multiline_pattern: Option<Regex>,
}

impl RuntimeConfig {
    /// Validates `config` and resolves it into a runtime-ready form.
    ///
    /// `default_checkpoint_id` is used only when the user config omits
    /// `checkpoint.id`; a later factory stage passes the pipeline's
    /// configured node identity here so `checkpoint.id` "defaults to the
    /// configured node identity" exactly as Appendix C documents. An empty
    /// `default_checkpoint_id` requires the user config to set
    /// `checkpoint.id` explicitly.
    pub(crate) fn from_config(
        config: Config,
        default_checkpoint_id: &str,
    ) -> Result<Self, otap_df_config::error::Error> {
        let Config {
            include,
            exclude,
            recursive,
            follow_symlinks,
            max_recursion_depth,
            start_at,
            discovery,
            ignore_older_than,
            identity,
            encoding,
            on_decode_error,
            framing,
            metadata,
            limits,
            batch,
            rotation,
            checkpoint,
            retry,
            on_nack,
            drain_timeout,
        } = config;

        let identity = validate_identity(identity)?;
        if !(1..=MAX_RECURSION_DEPTH).contains(&max_recursion_depth) {
            return Err(invalid(&format!(
                "max_recursion_depth must be in 1..={MAX_RECURSION_DEPTH}"
            )));
        }
        let discovery = validate_discovery(discovery)?;
        if drain_timeout.is_zero() {
            return Err(invalid("drain_timeout must be greater than zero"));
        }
        let limits = validate_limits(limits)?;
        let (framing, framing_profile_digest, compiled_multiline_pattern) =
            validate_framing(framing, encoding, &identity)?;
        let batch = validate_batch(batch, &framing, &metadata)?;
        let rotation = validate_rotation(rotation)?;
        let retry = validate_retry(retry)?;

        let checkpoint_id = resolve_checkpoint_id(checkpoint.id.as_deref(), default_checkpoint_id)?;
        let checkpoint_namespace_dir = checkpoint_namespace_dir(&checkpoint_id);
        validate_checkpoint_bounds(&checkpoint, &limits, &identity)?;
        validate_identity_reconciliation_bounds(&identity, &limits)?;

        validate_discovery_pattern_population(&include, &exclude)?;
        let (include, compiled_include) = validate_include(include, &checkpoint_namespace_dir)?;
        let (exclude, compiled_exclude) = validate_exclude(exclude)?;

        Ok(Self {
            include,
            compiled_include,
            exclude,
            compiled_exclude,
            recursive,
            follow_symlinks,
            max_recursion_depth,
            start_at,
            discovery,
            ignore_older_than,
            identity,
            encoding,
            on_decode_error,
            framing,
            metadata,
            limits,
            batch,
            rotation,
            checkpoint,
            checkpoint_id,
            checkpoint_namespace_dir,
            retry,
            on_nack,
            drain_timeout,
            framing_profile_digest,
            compiled_multiline_pattern,
        })
    }
}

impl TryFrom<Config> for RuntimeConfig {
    type Error = otap_df_config::error::Error;

    /// Validates `config` without an externally supplied node identity, so
    /// `checkpoint.id` must be set explicitly. Use
    /// [`RuntimeConfig::from_config`] once factory wiring can supply the
    /// pipeline node identity as the Appendix C default.
    fn try_from(config: Config) -> Result<Self, Self::Error> {
        Self::from_config(config, "")
    }
}

fn invalid(msg: &str) -> otap_df_config::error::Error {
    otap_df_config::error::Error::InvalidUserConfig {
        error: msg.to_owned(),
    }
}

/// Validates that a configured byte-size field fits in a `usize` on this
/// build target, so a later runtime allocation sized from it (a read
/// buffer, a decode buffer, a batch accumulator) can never be asked to
/// allocate a size that does not fit the platform's native size type. This
/// matters primarily on 32-bit targets, where `usize` is narrower than the
/// `u64` the field is configured and stored as.
fn ensure_fits_usize(field: &str, value: u64) -> Result<(), otap_df_config::error::Error> {
    if usize::try_from(value).is_err() {
        return Err(invalid(&format!(
            "{field} ({value} bytes) exceeds usize::MAX ({}) on this target and cannot be \
             allocated at runtime",
            usize::MAX
        )));
    }
    Ok(())
}

fn validate_identity(
    identity: IdentityConfig,
) -> Result<IdentityConfig, otap_df_config::error::Error> {
    if identity.fingerprint_bytes < MIN_FINGERPRINT_BYTES {
        return Err(invalid(&format!(
            "identity.fingerprint_bytes must be >= {MIN_FINGERPRINT_BYTES}"
        )));
    }
    // The checkpoint codec stores `fingerprint` as a `u16`-length-prefixed
    // byte field (`FINGERPRINT_MAX_BYTES = u16::MAX`); a larger configured
    // window could never be durably persisted.
    if identity.fingerprint_bytes > MAX_FINGERPRINT_BYTES {
        return Err(invalid(&format!(
            "identity.fingerprint_bytes must be <= {MAX_FINGERPRINT_BYTES} \
             (the checkpoint format's FINGERPRINT_MAX_BYTES)"
        )));
    }
    // The checkpoint codec stores `ignored_header_bytes` as a `u32`; a
    // larger configured value could never be durably persisted.
    if identity.ignored_header_bytes > MAX_IGNORED_HEADER_BYTES {
        return Err(invalid(&format!(
            "identity.ignored_header_bytes must be <= {MAX_IGNORED_HEADER_BYTES} (u32::MAX)"
        )));
    }
    // The matching-evidence window starts after `ignored_header_bytes` and
    // spans `fingerprint_bytes`; reject a configuration whose window would
    // overflow rather than silently wrapping.
    if identity
        .ignored_header_bytes
        .checked_add(identity.fingerprint_bytes)
        .is_none()
    {
        return Err(invalid(
            "identity.ignored_header_bytes + identity.fingerprint_bytes overflows u64",
        ));
    }
    Ok(identity)
}

/// Validates `discovery.poll_interval`. Unlike `ignore_older_than`,
/// `force_flush_period`, `checkpoint.sync_interval`, and
/// `checkpoint.retention`, the discovery cadence has no documented "zero
/// disables" meaning: a zero poll interval would mean the scanner never
/// reconciles new or removed files.
fn validate_discovery(
    discovery: DiscoveryConfig,
) -> Result<DiscoveryConfig, otap_df_config::error::Error> {
    if discovery.poll_interval.is_zero() {
        return Err(invalid("discovery.poll_interval must be greater than zero"));
    }
    Ok(discovery)
}

fn validate_limits(limits: LimitsConfig) -> Result<LimitsConfig, otap_df_config::error::Error> {
    if limits.max_tracked_files == 0 {
        return Err(invalid(
            "limits.max_tracked_files must be greater than zero",
        ));
    }
    if limits.max_pending_candidates == 0 {
        return Err(invalid(
            "limits.max_pending_candidates must be greater than zero",
        ));
    }
    if limits.max_open_files == 0 {
        return Err(invalid("limits.max_open_files must be greater than zero"));
    }
    if limits.max_open_files > limits.max_tracked_files {
        return Err(invalid(
            "limits.max_open_files must be <= limits.max_tracked_files",
        ));
    }
    ensure_fits_usize(
        "limits.max_pending_candidates + limits.max_open_files",
        u64::from(limits.max_pending_candidates) + u64::from(limits.max_open_files),
    )?;
    ensure_fits_usize(
        "limits.max_tracked_files + limits.max_pending_candidates + limits.max_open_files",
        u64::from(limits.max_tracked_files)
            + u64::from(limits.max_pending_candidates)
            + u64::from(limits.max_open_files),
    )?;
    if limits.max_read_bytes_per_turn == 0 {
        return Err(invalid(
            "limits.max_read_bytes_per_turn must be greater than zero",
        ));
    }
    ensure_fits_usize(
        "limits.max_read_bytes_per_turn",
        limits.max_read_bytes_per_turn,
    )?;
    Ok(limits)
}

fn validate_identity_reconciliation_bounds(
    identity: &IdentityConfig,
    limits: &LimitsConfig,
) -> Result<(), otap_df_config::error::Error> {
    let candidate_population = u64::from(limits.max_pending_candidates)
        .checked_add(u64::from(limits.max_open_files))
        .ok_or_else(|| invalid("identity reconciliation candidate population overflows u64"))?;
    let candidate_bytes = identity
        .fingerprint_bytes
        .checked_mul(DISCOVERY_CANDIDATE_FINGERPRINT_COPIES)
        .and_then(|bytes| {
            (ADVISORY_PATH_MAX_BYTES as u64)
                .checked_mul(DISCOVERY_CANDIDATE_PATH_COPIES)
                .and_then(|paths| bytes.checked_add(paths))
        })
        .and_then(|bytes| bytes.checked_add(IDENTITY_CANDIDATE_OVERHEAD_BYTES))
        .and_then(|bytes| bytes.checked_mul(candidate_population))
        .ok_or_else(|| invalid("identity reconciliation candidate memory bound overflows u64"))?;
    // The common candidate term covers retained state, the temporary evidence
    // vector, and every simultaneously retained inventory. In-flight
    // admission also retains a full event copy, and durable update preflight
    // can retain old/new operations, a cloned transaction, encoded body and
    // frame, and a transient applied record. Ten extra fingerprint and path
    // payloads conservatively cover that amplification.
    let open_candidate_bytes = identity
        .fingerprint_bytes
        .checked_mul(IDENTITY_OPEN_CANDIDATE_FINGERPRINT_COPIES)
        .and_then(|bytes| {
            (ADVISORY_PATH_MAX_BYTES as u64)
                .checked_mul(IDENTITY_OPEN_CANDIDATE_PATH_COPIES)
                .and_then(|paths| bytes.checked_add(paths))
        })
        .and_then(|bytes| bytes.checked_add(IDENTITY_OPEN_CANDIDATE_OVERHEAD_BYTES))
        .and_then(|bytes| bytes.checked_mul(u64::from(limits.max_open_files)))
        .ok_or_else(|| {
            invalid("identity reconciliation open-candidate memory bound overflows u64")
        })?;
    let record_state_bytes = identity
        .fingerprint_bytes
        .checked_add(ADVISORY_PATH_MAX_BYTES as u64)
        .and_then(|bytes| bytes.checked_add(IDENTITY_RECORD_INDEX_OVERHEAD_BYTES))
        .and_then(|bytes| bytes.checked_mul(u64::from(limits.max_tracked_files)))
        .ok_or_else(|| invalid("identity reconciliation record-state bound overflows u64"))?;
    let discovery_tracked_bytes = u64::from(limits.max_tracked_files)
        .checked_mul(DISCOVERY_TRACKED_OVERHEAD_BYTES)
        .ok_or_else(|| invalid("discovery tracked-state bound overflows u64"))?;
    let total = candidate_bytes
        .checked_add(open_candidate_bytes)
        .and_then(|bytes| bytes.checked_add(record_state_bytes))
        .and_then(|bytes| bytes.checked_add(discovery_tracked_bytes))
        .ok_or_else(|| invalid("identity reconciliation working-set bound overflows u64"))?;
    if total > IDENTITY_RECONCILIATION_BYTES_CEILING {
        return Err(invalid(&format!(
            "identity reconciliation worst-case working set is {total} bytes, exceeding the \
             {IDENTITY_RECONCILIATION_BYTES_CEILING}-byte ceiling; reduce \
             limits.max_pending_candidates, limits.max_open_files, limits.max_tracked_files, \
             or identity.fingerprint_bytes"
        )));
    }
    Ok(())
}

/// Validates the RE2-compatible pattern (rejecting backreferences,
/// lookaround, and any other construct the `regex` crate itself does not
/// accept -- the same restricted feature set RE2 supports) and returns the
/// compiled [`Regex`].
fn compile_re2_pattern(field: &str, pattern: &str) -> Result<Regex, otap_df_config::error::Error> {
    if pattern.is_empty() {
        return Err(invalid(&format!("{field} must not be empty when set")));
    }
    Regex::new(pattern).map_err(|source| {
        invalid(&format!(
            "{field} is not a valid RE2-compatible regular expression: {source}"
        ))
    })
}

/// Canonicalizes `framing.force_flush_period` into the whole-millisecond
/// count persisted in the framing-profile digest, which is the same unit a
/// later runtime idle-flush timer will use. Zero is allowed and means
/// "idle partial flushing disabled" (Appendix C). A nonzero value must
/// resolve to a whole number of milliseconds -- any sub-millisecond
/// remainder is rejected rather than silently rounded away or silently
/// disabled -- and must fit `u64` milliseconds; an out-of-range value is
/// rejected rather than saturated, so config validation and the runtime
/// timer can never disagree about the configured period.
fn canonicalize_force_flush_period_millis(
    period: Duration,
) -> Result<u64, otap_df_config::error::Error> {
    if period.is_zero() {
        return Ok(0);
    }
    // Any sub-millisecond component of `period` lives entirely within
    // `subsec_nanos()`; a nonzero remainder there means the duration cannot
    // be represented as a whole number of milliseconds without rounding.
    if !period.subsec_nanos().is_multiple_of(1_000_000) {
        return Err(invalid(
            "framing.force_flush_period must be zero (disabled) or resolve to a whole number \
             of milliseconds; a sub-millisecond remainder is not accepted",
        ));
    }
    u64::try_from(period.as_millis())
        .map_err(|_| invalid("framing.force_flush_period exceeds u64::MAX milliseconds"))
}

fn validate_framing(
    framing: FramingConfig,
    encoding: Encoding,
    identity: &IdentityConfig,
) -> Result<(FramingConfig, [u8; 32], Option<Regex>), otap_df_config::error::Error> {
    if framing.max_line_bytes == 0 {
        return Err(invalid("framing.max_line_bytes must be greater than zero"));
    }
    if framing.max_record_bytes == 0 {
        return Err(invalid(
            "framing.max_record_bytes must be greater than zero",
        ));
    }
    if framing.max_multiline_lines == 0 {
        return Err(invalid(
            "framing.max_multiline_lines must be greater than zero",
        ));
    }
    ensure_fits_usize("framing.max_line_bytes", framing.max_line_bytes)?;
    ensure_fits_usize("framing.max_record_bytes", framing.max_record_bytes)?;

    let (multiline_mode, compiled_pattern) = match (
        &framing.multiline.line_start_pattern,
        &framing.multiline.line_end_pattern,
    ) {
        (None, None) => (framing_profile::MultilineMode::Newline, None),
        (Some(_), Some(_)) => {
            return Err(invalid(
                "framing.multiline must set at most one of line_start_pattern or \
                 line_end_pattern; setting both is rejected",
            ));
        }
        (Some(pattern), None) => {
            let compiled = compile_re2_pattern("framing.multiline.line_start_pattern", pattern)?;
            (
                framing_profile::MultilineMode::StartPattern {
                    regex_profile_version: framing.multiline.regex_profile.version(),
                    pattern: pattern.clone(),
                },
                Some(compiled),
            )
        }
        (None, Some(pattern)) => {
            let compiled = compile_re2_pattern("framing.multiline.line_end_pattern", pattern)?;
            (
                framing_profile::MultilineMode::EndPattern {
                    regex_profile_version: framing.multiline.regex_profile.version(),
                    pattern: pattern.clone(),
                },
                Some(compiled),
            )
        }
    };

    let force_flush_period_millis =
        canonicalize_force_flush_period_millis(framing.force_flush_period)?;
    let params = framing_profile::FramingProfileParams {
        fingerprint_profile_version: FINGERPRINT_PROFILE_VERSION,
        fingerprint_bytes: u16::try_from(identity.fingerprint_bytes)
            .expect("validated fingerprint_bytes fits u16"),
        ignored_header_bytes: u32::try_from(identity.ignored_header_bytes)
            .expect("validated ignored_header_bytes fits u32"),
        encoding: encoding_to_framing_profile(encoding),
        multiline_mode,
        max_line_bytes: framing.max_line_bytes,
        max_record_bytes: framing.max_record_bytes,
        max_log_size_behavior: framing.max_log_size_behavior.to_framing_profile(),
        max_multiline_lines: framing.max_multiline_lines,
        force_flush_period_millis,
    };
    let digest = params.digest().map_err(|source| {
        invalid(&format!(
            "framing.multiline pattern could not be encoded for the framing-profile digest: {source}"
        ))
    })?;

    Ok((framing, digest, compiled_pattern))
}

const fn encoding_to_framing_profile(encoding: Encoding) -> framing_profile::FramingEncoding {
    match encoding {
        Encoding::Utf8 => framing_profile::FramingEncoding::Utf8,
        Encoding::Ascii => framing_profile::FramingEncoding::Ascii,
        Encoding::Utf16Le => framing_profile::FramingEncoding::Utf16Le,
        Encoding::Utf16Be => framing_profile::FramingEncoding::Utf16Be,
        Encoding::Raw => framing_profile::FramingEncoding::Raw,
    }
}

fn validate_batch(
    batch: BatchConfig,
    framing: &FramingConfig,
    metadata: &MetadataConfig,
) -> Result<BatchConfig, otap_df_config::error::Error> {
    if batch.max_records == 0 {
        return Err(invalid("batch.max_records must be greater than zero"));
    }
    if batch.max_records > MAX_BATCH_MAX_RECORDS {
        return Err(invalid(&format!(
            "batch.max_records must be <= {MAX_BATCH_MAX_RECORDS}"
        )));
    }
    if batch.max_bytes == 0 {
        return Err(invalid("batch.max_bytes must be greater than zero"));
    }
    if batch.max_flush_period.is_zero() {
        return Err(invalid("batch.max_flush_period must be greater than zero"));
    }
    ensure_fits_usize("batch.max_bytes", batch.max_bytes)?;

    // A single emitted record or fragment cannot exceed batch.max_bytes: the
    // same logical-size function used by runtime flushing validates both
    // max_line_bytes and max_record_bytes with configured and fixed
    // attributes at config build time (Appendix C, "Emitted data model").
    // `logical_record_size` uses checked arithmetic and reports overflow
    // rather than saturating, so a `body_bytes`/`batch.max_bytes` pair that
    // could only "validate" via saturation is rejected here instead.
    let line_bound = logical_record_size(
        framing.max_line_bytes,
        metadata,
        framing.max_log_size_behavior,
    )
    .map_err(|_| {
        invalid(&format!(
            "framing.max_line_bytes ({} bytes) plus its fixed attribute overhead overflows u64",
            framing.max_line_bytes
        ))
    })?;
    if line_bound > batch.max_bytes {
        return Err(invalid(&format!(
            "framing.max_line_bytes ({} bytes) plus its fixed attribute overhead \
             ({line_bound} bytes) exceeds batch.max_bytes ({} bytes)",
            framing.max_line_bytes, batch.max_bytes
        )));
    }
    ensure_fits_usize("framing.max_line_bytes logical record size", line_bound)?;
    let record_bound = logical_record_size(
        framing.max_record_bytes,
        metadata,
        framing.max_log_size_behavior,
    )
    .map_err(|_| {
        invalid(&format!(
            "framing.max_record_bytes ({} bytes) plus its fixed attribute overhead overflows u64",
            framing.max_record_bytes
        ))
    })?;
    if record_bound > batch.max_bytes {
        return Err(invalid(&format!(
            "framing.max_record_bytes ({} bytes) plus its fixed attribute overhead \
             ({record_bound} bytes) exceeds batch.max_bytes ({} bytes)",
            framing.max_record_bytes, batch.max_bytes
        )));
    }
    ensure_fits_usize("framing.max_record_bytes logical record size", record_bound)?;

    Ok(batch)
}

fn validate_rotation(
    rotation: RotationConfig,
) -> Result<RotationConfig, otap_df_config::error::Error> {
    if rotation.rotate_wait.is_zero() {
        return Err(invalid("rotation.rotate_wait must be greater than zero"));
    }
    Ok(rotation)
}

fn validate_retry(retry: RetryConfig) -> Result<RetryConfig, otap_df_config::error::Error> {
    if retry.max_attempts == 0 {
        return Err(invalid("retry.max_attempts must be greater than zero"));
    }
    if retry.initial_backoff.is_zero() {
        return Err(invalid("retry.initial_backoff must be greater than zero"));
    }
    if retry.max_backoff < retry.initial_backoff {
        return Err(invalid(
            "retry.max_backoff must be >= retry.initial_backoff",
        ));
    }
    Ok(retry)
}

fn validate_checkpoint_bounds(
    checkpoint: &CheckpointConfig,
    limits: &LimitsConfig,
    identity: &IdentityConfig,
) -> Result<(), otap_df_config::error::Error> {
    if checkpoint.compact_after_bytes == 0 {
        return Err(invalid(
            "checkpoint.compact_after_bytes must be greater than zero",
        ));
    }
    if checkpoint.compact_after_transactions == 0 {
        return Err(invalid(
            "checkpoint.compact_after_transactions must be greater than zero",
        ));
    }
    if checkpoint.ownership_timeout.is_zero() {
        return Err(invalid(
            "checkpoint.ownership_timeout must be greater than zero",
        ));
    }
    if checkpoint.max_consecutive_failures == 0 {
        return Err(invalid(
            "checkpoint.max_consecutive_failures must be greater than zero",
        ));
    }
    // The durable checkpoint store derives its artifact and combined
    // recovery-working-set caps from exactly these three knobs, and enforces
    // the same artifact caps when it writes. Running the derivation here
    // rejects an unrecoverable configuration at build time, with the knobs
    // to reduce, rather than at the first compaction or reopen.
    let _store_limits = StoreLimits::derive(
        checkpoint.compact_after_bytes,
        limits.max_tracked_files,
        identity.fingerprint_bytes,
    )
    .map_err(|error| invalid(&error.to_string()))?;
    Ok(())
}

fn resolve_checkpoint_id(
    configured: Option<&str>,
    default_checkpoint_id: &str,
) -> Result<String, otap_df_config::error::Error> {
    let id = configured.unwrap_or(default_checkpoint_id);
    if id.is_empty() {
        return Err(invalid(
            "checkpoint.id must not be empty; set checkpoint.id explicitly or ensure the \
             receiver node has a configured identity to default to",
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(invalid(
            "checkpoint.id must contain only ASCII alphanumerics, '_', '-', or '.'",
        ));
    }
    // The checkpoint format's `remove_file.namespace_id` field stores the
    // exact `checkpoint.id` in a `u16`-length-prefixed byte field bounded by
    // `NAMESPACE_ID_MAX_BYTES`; a longer id could never round-trip through
    // the durable audit record.
    if id.len() > NAMESPACE_ID_MAX_BYTES {
        return Err(invalid(&format!(
            "checkpoint.id must be <= {NAMESPACE_ID_MAX_BYTES} bytes \
            (the checkpoint format's NAMESPACE_ID_MAX_BYTES)"
        )));
    }
    // The id is also used, percent-encoded, as a single filesystem path
    // component (Appendix B, "Namespace layout"). Enforce the tighter of
    // NAMESPACE_ID_MAX_BYTES and the common 255-byte NAME_MAX so a
    // configuration that validates here never fails later with an OS-level
    // "file name too long" error; this checks the actual encoded segment
    // rather than assuming the charset restriction above never expands
    // under percent-encoding.
    let encoded_len = encode_path_segment(id).len();
    if encoded_len > CHECKPOINT_ID_SEGMENT_MAX_BYTES {
        return Err(invalid(&format!(
            "checkpoint.id's percent-encoded path segment is {encoded_len} bytes, exceeding \
            {CHECKPOINT_ID_SEGMENT_MAX_BYTES} bytes (the tighter of NAMESPACE_ID_MAX_BYTES and \
            the common 255-byte filesystem NAME_MAX)"
        )));
    }
    Ok(id.to_owned())
}

/// Computes the stable Phase 1 checkpoint namespace directory for
/// `checkpoint_id`: `${engine.state_dir}/filelog/<percent-encoded id>/`
/// (Appendix B, "Namespace layout"). Mirrors the journald receiver's
/// `${engine.state_dir}` expansion and percent-encoding convention so both
/// receivers resolve the token identically.
///
/// Normalizes away a leading `CurDir` (`.`) path component so a
/// `${engine.state_dir}` expansion that happens to start with `./` (for
/// example an `OTAP_DF_STATE_DIR` of `./state`) resolves to the exact same
/// [`PathBuf`] as the equivalent path without it; this keeps the namespace
/// side of the direct-include collision check
/// ([`include_targets_checkpoint_namespace`]) consistent with the same
/// normalization applied to include patterns via [`glob_literal_prefix`].
fn checkpoint_namespace_dir(checkpoint_id: &str) -> PathBuf {
    let mut path = expand_state_dir(Path::new(CHECKPOINT_NAMESPACE_ROOT));
    path.push(encode_path_segment(checkpoint_id));
    strip_leading_curdir(&path)
}

fn expand_state_dir(root: &Path) -> PathBuf {
    let text = root.to_string_lossy();
    if let Some(rest) = text.strip_prefix("${engine.state_dir}") {
        let base = std::env::var_os("OTAP_DF_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".otap-state"));
        return base.join(rest.trim_start_matches('/'));
    }
    root.to_path_buf()
}

/// Strips a single leading [`Component::CurDir`] (`.`) component, if
/// present, so two paths that differ only by a redundant leading `./` (or
/// an equivalent run of them, which [`Path::components`] itself collapses
/// to at most one leading `CurDir`) compare equal. `Path::components`
/// otherwise preserves a leading `CurDir` verbatim -- unlike an internal
/// `.` component, which it already normalizes away -- so without this an
/// include pattern like `./.otap-state/filelog/<id>/*.log` would not be
/// recognized as targeting the same directory as
/// `.otap-state/filelog/<id>/*.log`, letting the leading `./` bypass the
/// direct checkpoint-namespace-inclusion rejection.
fn strip_leading_curdir(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    if matches!(components.peek(), Some(Component::CurDir)) {
        let _ = components.next();
    }
    components.collect()
}

fn encode_path_segment(value: &str) -> String {
    if value.is_empty() {
        return "%".to_owned();
    }
    let encode_all = matches!(value, "." | "..");
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if !encode_all && is_checkpoint_path_safe_byte(byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(hex_digit(byte >> 4)));
            encoded.push(char::from(hex_digit(byte & 0x0f)));
        }
    }
    encoded
}

fn is_checkpoint_path_safe_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        10..=15 => b'A' + (value - 10),
        _ => unreachable!("nibble must be in range"),
    }
}

/// Extracts the literal (non-glob) directory-component prefix of a glob
/// pattern: the path components before the first one containing a glob
/// metacharacter (`*`, `?`, `[`, `{`). Normalizes away a leading `CurDir`
/// (`./`) component so a pattern like `./.otap-state/filelog/<id>/*.log`
/// compares equal to the equivalent pattern without it; see
/// [`strip_leading_curdir`].
pub(super) fn glob_literal_prefix(pattern: &str) -> PathBuf {
    let mut prefix = PathBuf::new();
    for component in Path::new(pattern).components() {
        if component_has_glob_meta(&component.as_os_str().to_string_lossy()) {
            break;
        }
        #[cfg(not(windows))]
        {
            prefix.push(unescape_glob_component(
                &component.as_os_str().to_string_lossy(),
            ));
        }
        #[cfg(windows)]
        {
            prefix.push(component);
        }
    }
    strip_leading_curdir(&prefix)
}

#[cfg(not(windows))]
fn unescape_glob_component(component: &str) -> String {
    let mut unescaped = String::with_capacity(component.len());
    let mut characters = component.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(escaped) = characters.next() {
                unescaped.push(escaped);
            } else {
                unescaped.push(character);
            }
        } else {
            unescaped.push(character);
        }
    }
    unescaped
}

/// Reports whether `pattern`'s literal (non-glob) directory prefix resolves
/// at or under `namespace_dir`. This is the "direct include" collision the
/// receiver rejects unconditionally (Appendix, "Discovery rules and
/// safety"); it is a best-effort lexical check, not filesystem
/// canonicalization, and deliberately does not flag the softer "appears to
/// cover" ancestor case (that remains an operator warning, not a hard
/// rejection).
fn include_targets_checkpoint_namespace(pattern: &str, namespace_dir: &Path) -> bool {
    let literal_prefix = glob_literal_prefix(pattern);
    !literal_prefix.as_os_str().is_empty()
        && (literal_prefix == namespace_dir || literal_prefix.starts_with(namespace_dir))
}

fn component_has_glob_meta(component: &str) -> bool {
    let mut escaped = false;
    for character in component.chars() {
        if cfg!(not(windows)) && !escaped && character == '\\' {
            escaped = true;
            continue;
        }
        if !escaped && matches!(character, '*' | '?' | '[' | '{') {
            return true;
        }
        escaped = false;
    }
    false
}

fn compile_path_glob(
    pattern: &str,
    field: &'static str,
) -> Result<GlobMatcher, otap_df_config::error::Error> {
    let normalized = strip_leading_curdir(Path::new(pattern));
    let normalized = if normalized.as_os_str().is_empty() {
        Cow::Borrowed(pattern)
    } else {
        normalized.to_string_lossy()
    };
    let mut builder = GlobBuilder::new(&normalized);
    let _ = builder
        .literal_separator(true)
        .backslash_escape(cfg!(not(windows)));
    builder
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|source| invalid(&format!("{field} pattern '{pattern}' is invalid: {source}")))
}

fn validate_discovery_pattern_population(
    include: &[String],
    exclude: &[String],
) -> Result<(), otap_df_config::error::Error> {
    let count = include
        .len()
        .checked_add(exclude.len())
        .ok_or_else(|| invalid("discovery glob count overflows usize"))?;
    if count > MAX_DISCOVERY_PATTERNS {
        return Err(invalid(&format!(
            "include plus exclude supports at most {MAX_DISCOVERY_PATTERNS} patterns"
        )));
    }
    let mut total_bytes = 0usize;
    for (field, patterns) in [("include", include), ("exclude", exclude)] {
        for pattern in patterns {
            if pattern.len() > MAX_DISCOVERY_PATTERN_BYTES {
                return Err(invalid(&format!(
                    "{field} pattern is {} bytes, exceeding the \
                     {MAX_DISCOVERY_PATTERN_BYTES}-byte maximum",
                    pattern.len()
                )));
            }
            total_bytes = total_bytes
                .checked_add(pattern.len())
                .ok_or_else(|| invalid("aggregate discovery glob bytes overflow usize"))?;
        }
    }
    if total_bytes > MAX_DISCOVERY_PATTERN_TOTAL_BYTES {
        return Err(invalid(&format!(
            "include plus exclude patterns total {total_bytes} bytes, exceeding the \
             {MAX_DISCOVERY_PATTERN_TOTAL_BYTES}-byte maximum"
        )));
    }
    Ok(())
}

fn validate_include(
    include: Vec<String>,
    checkpoint_namespace_dir: &Path,
) -> Result<(Vec<String>, Vec<GlobMatcher>), otap_df_config::error::Error> {
    if include.is_empty() {
        return Err(invalid("include must be non-empty"));
    }
    let mut compiled = Vec::with_capacity(include.len());
    for pattern in &include {
        if pattern.is_empty() {
            return Err(invalid("include entries must not be empty"));
        }
        if include_targets_checkpoint_namespace(pattern, checkpoint_namespace_dir) {
            return Err(invalid(&format!(
                "include pattern '{pattern}' resolves directly to the receiver's own \
                 checkpoint namespace ({}); this is rejected unconditionally so the \
                 receiver cannot read its own checkpoint state",
                checkpoint_namespace_dir.display()
            )));
        }
        compiled.push(compile_path_glob(pattern, "include")?);
    }
    Ok((include, compiled))
}

fn validate_exclude(
    exclude: Vec<String>,
) -> Result<(Vec<String>, Vec<GlobMatcher>), otap_df_config::error::Error> {
    let mut compiled = Vec::with_capacity(exclude.len());
    for pattern in &exclude {
        if pattern.is_empty() {
            return Err(invalid("exclude entries must not be empty"));
        }
        compiled.push(compile_path_glob(pattern, "exclude")?);
    }
    Ok((exclude, compiled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receivers::filelog_receiver::checkpoint::store::limits as store_limits;

    fn minimal_config() -> Config {
        Config {
            include: vec!["/var/log/app/*.log".to_owned()],
            ..Config::default()
        }
    }

    fn parse(value: serde_json::Value) -> Result<Config, serde_json::Error> {
        serde_json::from_value(value)
    }

    fn largest_accepted(mut low: u64, mut high: u64, mut accepts: impl FnMut(u64) -> bool) -> u64 {
        assert!(accepts(low), "the lower boundary must be accepted");
        while low < high {
            let middle = low + ((high - low) / 2) + 1;
            if accepts(middle) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        low
    }

    /// Scenario: parsing a minimal config with only `include` set.
    /// Guarantees: every Appendix C default (bool flags, enums, byte-size
    /// and duration bounds) matches the documented defaults, and validation
    /// into `RuntimeConfig` succeeds when an explicit checkpoint id is
    /// supplied via `from_config`.
    #[test]
    fn defaults_match_appendix_c() {
        let cfg = minimal_config();
        assert_eq!(cfg.exclude, Vec::<String>::new());
        assert!(cfg.recursive);
        assert!(!cfg.follow_symlinks);
        assert_eq!(cfg.max_recursion_depth, 64);
        assert_eq!(cfg.start_at, StartAt::End);
        assert_eq!(cfg.discovery.poll_interval, Duration::from_secs(5));
        assert_eq!(cfg.ignore_older_than, Duration::ZERO);
        assert_eq!(cfg.identity.fingerprint_bytes, 1000);
        assert_eq!(cfg.identity.ignored_header_bytes, 0);
        assert_eq!(
            cfg.identity.on_recovery_mismatch,
            OnRecoveryMismatch::Beginning
        );
        assert_eq!(cfg.encoding, Encoding::Utf8);
        assert_eq!(cfg.on_decode_error, OnDecodeError::PreserveRaw);
        assert_eq!(cfg.framing.max_line_bytes, 1024 * 1024);
        assert_eq!(cfg.framing.max_record_bytes, 1024 * 1024);
        assert_eq!(cfg.framing.max_log_size_behavior, MaxLogSizeBehavior::Split);
        assert_eq!(cfg.framing.force_flush_period, Duration::from_millis(500));
        assert_eq!(cfg.framing.multiline.regex_profile, RegexProfile::Re2V1);
        assert_eq!(cfg.framing.multiline.line_start_pattern, None);
        assert_eq!(cfg.framing.multiline.line_end_pattern, None);
        assert_eq!(cfg.framing.max_multiline_lines, 500);
        assert!(!cfg.metadata.include_file_record_offset);
        assert!(!cfg.metadata.include_file_record_number);
        assert_eq!(cfg.limits.max_tracked_files, 10_000);
        assert_eq!(cfg.limits.max_pending_candidates, 10_000);
        assert_eq!(cfg.limits.max_open_files, 512);
        assert_eq!(cfg.limits.max_read_bytes_per_turn, 128 * 1024);
        assert_eq!(cfg.batch.max_records, 1024);
        assert_eq!(cfg.batch.max_bytes, 8 * 1024 * 1024);
        assert_eq!(cfg.batch.max_flush_period, Duration::from_secs(1));
        assert_eq!(cfg.rotation.rotate_wait, Duration::from_secs(5));
        assert_eq!(cfg.rotation.on_truncate, OnTruncate::Fail);
        assert_eq!(cfg.checkpoint.id, None);
        assert_eq!(cfg.checkpoint.sync_interval, Duration::ZERO);
        assert_eq!(cfg.checkpoint.compact_after_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.checkpoint.compact_after_transactions, 10_000);
        assert_eq!(
            cfg.checkpoint.retention,
            Duration::from_secs(7 * 24 * 60 * 60)
        );
        assert_eq!(cfg.checkpoint.ownership_timeout, Duration::from_secs(30));
        assert_eq!(cfg.checkpoint.max_consecutive_failures, 5);
        assert_eq!(cfg.retry.max_attempts, 8);
        assert_eq!(cfg.retry.initial_backoff, Duration::from_millis(100));
        assert_eq!(cfg.retry.max_backoff, Duration::from_secs(5));
        assert_eq!(cfg.on_nack, OnNack::Fail);
        assert_eq!(cfg.drain_timeout, Duration::from_secs(10));

        let runtime = RuntimeConfig::from_config(cfg, "node-1").expect("defaults must validate");
        assert_eq!(runtime.checkpoint_id, "node-1");
        assert!(runtime.compiled_multiline_pattern.is_none());
    }

    /// Scenario: an unknown field appears at the top level, inside a nested
    /// object (`framing`), and inside a doubly-nested object (`identity`).
    /// Guarantees: `deny_unknown_fields` rejects typos at every nesting
    /// level instead of silently ignoring them.
    #[test]
    fn deny_unknown_fields_at_every_level() {
        let top = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "unexpected_top_level_field": true
        }));
        assert!(top.is_err(), "unknown top-level field must be rejected");

        let nested = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "framing": { "unexpected_framing_field": 1 }
        }));
        assert!(nested.is_err(), "unknown framing field must be rejected");

        let double_nested = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "framing": { "multiline": { "unexpected_multiline_field": 1 } }
        }));
        assert!(
            double_nested.is_err(),
            "unknown framing.multiline field must be rejected"
        );

        let identity = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "identity": { "unexpected_identity_field": 1 }
        }));
        assert!(identity.is_err(), "unknown identity field must be rejected");

        let checkpoint = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "checkpoint": { "unexpected_checkpoint_field": 1 }
        }));
        assert!(
            checkpoint.is_err(),
            "unknown checkpoint field must be rejected"
        );
    }

    /// Scenario: `include` is omitted entirely from the input.
    /// Guarantees: `include` has no serde default, so a config without it
    /// fails to deserialize rather than silently defaulting to empty.
    #[test]
    fn include_has_no_default() {
        let result = parse(serde_json::json!({}));
        assert!(result.is_err(), "include must be required");
    }

    /// Scenario: `include` is present but an empty list.
    /// Guarantees: semantic validation rejects an empty include list even
    /// though it deserializes successfully.
    #[test]
    fn include_must_be_non_empty() {
        let cfg = Config {
            include: vec![],
            ..Config::default()
        };
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("include must be non-empty"));
    }

    /// Scenario: `include` contains one valid entry and one empty string.
    /// Guarantees: an empty include entry is rejected even when the list
    /// itself is non-empty.
    #[test]
    fn include_entries_must_be_non_empty() {
        let cfg = Config {
            include: vec!["/var/log/app/*.log".to_owned(), String::new()],
            ..Config::default()
        };
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `exclude` contains an empty string entry.
    /// Guarantees: an empty exclude entry is rejected, matching the same
    /// non-empty-entry rule applied to `include`.
    #[test]
    fn exclude_entries_must_be_non_empty() {
        let mut cfg = minimal_config();
        cfg.exclude = vec![String::new()];
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: include and exclude lists contain malformed character-class
    /// globs.
    /// Guarantees: every path glob is compiled during configuration
    /// validation, so syntax failures cannot surface after the discovery
    /// thread starts.
    #[test]
    fn discovery_glob_syntax_is_validated_eagerly() {
        let mut include = minimal_config();
        include.include = vec!["/var/log/[".to_owned()];
        let error = RuntimeConfig::from_config(include, "node-1").unwrap_err();
        assert!(error.to_string().contains("include pattern"));

        let mut exclude = minimal_config();
        exclude.exclude = vec!["/var/log/[".to_owned()];
        let error = RuntimeConfig::from_config(exclude, "node-1").unwrap_err();
        assert!(error.to_string().contains("exclude pattern"));
    }

    /// Scenario: discovery configuration exceeds the per-pattern, pattern
    /// count, and aggregate pattern-byte bounds independently.
    /// Guarantees: glob compilation and retained pattern memory are bounded
    /// before any discovery plan or worker thread is created.
    #[test]
    fn discovery_pattern_population_is_bounded() {
        let mut oversized = minimal_config();
        oversized.include = vec!["x".repeat(MAX_DISCOVERY_PATTERN_BYTES + 1)];
        let error = RuntimeConfig::from_config(oversized, "node-1").unwrap_err();
        assert!(error.to_string().contains("byte maximum"));

        let mut too_many = minimal_config();
        too_many.exclude = vec!["x".to_owned(); MAX_DISCOVERY_PATTERNS];
        let error = RuntimeConfig::from_config(too_many, "node-1").unwrap_err();
        assert!(error.to_string().contains("at most"));

        let mut aggregate = minimal_config();
        aggregate.exclude = vec!["x".repeat(MAX_DISCOVERY_PATTERN_BYTES); 257];
        let error = RuntimeConfig::from_config(aggregate, "node-1").unwrap_err();
        assert!(error.to_string().contains("patterns total"));
    }

    /// Scenario: a single-star include is matched against a direct child and
    /// a nested descendant path.
    /// Guarantees: compiled path globs treat separators literally, leaving
    /// recursive matching to `**` plus the independent traversal switch.
    #[test]
    fn compiled_discovery_globs_treat_path_separators_literally() {
        let runtime = RuntimeConfig::from_config(minimal_config(), "node-1").unwrap();
        let matcher = &runtime.compiled_include[0];

        assert!(matcher.is_match("/var/log/app/direct.log"));
        assert!(!matcher.is_match("/var/log/app/nested/child.log"));
    }

    /// Scenario: `max_recursion_depth` is set to zero.
    /// Guarantees: a zero recursion bound is rejected; it is one of the
    /// "all nonzero bounds" the design requires.
    #[test]
    fn max_recursion_depth_must_be_nonzero() {
        let mut cfg = minimal_config();
        cfg.max_recursion_depth = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `max_recursion_depth` exceeds the fixed traversal-stack
    /// ceiling.
    /// Guarantees: a user cannot configure an effectively unbounded
    /// symlink-cycle or directory traversal stack.
    #[test]
    fn max_recursion_depth_has_a_fixed_ceiling() {
        let mut cfg = minimal_config();
        cfg.max_recursion_depth = MAX_RECURSION_DEPTH + 1;
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(error.to_string().contains("1..="));
    }

    /// Scenario: `discovery.poll_interval` is set to zero.
    /// Guarantees: a zero poll interval is rejected; unlike
    /// `force_flush_period`, `ignore_older_than`, `checkpoint.sync_interval`,
    /// and `checkpoint.retention`, it has no documented "zero disables"
    /// meaning -- a zero interval would mean discovery never reconciles.
    #[test]
    fn discovery_poll_interval_must_be_nonzero() {
        let mut cfg = minimal_config();
        cfg.discovery.poll_interval = Duration::ZERO;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("poll_interval"));
    }

    /// Scenario: `drain_timeout` is set to zero.
    /// Guarantees: a zero drain deadline is rejected; it bounds the drain
    /// step alongside the engine deadline and has no "zero disables"
    /// meaning.
    #[test]
    fn drain_timeout_must_be_nonzero() {
        let mut cfg = minimal_config();
        cfg.drain_timeout = Duration::ZERO;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("drain_timeout"));
    }

    /// Scenario: `identity.fingerprint_bytes` is set below the documented
    /// minimum of 16, at exactly 16, and above it.
    /// Guarantees: only values `< 16` are rejected; 16 and above validate.
    #[test]
    fn fingerprint_bytes_minimum_is_enforced() {
        let mut cfg = minimal_config();
        cfg.identity.fingerprint_bytes = 15;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_err());

        cfg.identity.fingerprint_bytes = 16;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());
    }

    /// Scenario: `identity.ignored_header_bytes + identity.fingerprint_bytes`
    /// would overflow `u64`.
    /// Guarantees: the overflowing combination is rejected instead of
    /// silently wrapping into an incorrect matching-evidence window.
    #[test]
    fn fingerprint_window_overflow_is_rejected() {
        let mut cfg = minimal_config();
        cfg.identity.ignored_header_bytes = u64::MAX;
        cfg.identity.fingerprint_bytes = 1000;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `identity.fingerprint_bytes` is set to exactly the
    /// checkpoint format's `FINGERPRINT_MAX_BYTES` (`u16::MAX`) and to one
    /// byte above it.
    /// Guarantees: the exact format maximum reaches the stricter recovery
    /// working-set check, while one byte more is rejected specifically as
    /// unrepresentable by the codec's `u16` length prefix.
    #[test]
    fn fingerprint_bytes_maximum_is_enforced() {
        let mut cfg = minimal_config();
        cfg.identity.fingerprint_bytes = MAX_FINGERPRINT_BYTES;
        let err = RuntimeConfig::from_config(cfg.clone(), "node-1").unwrap_err();
        assert!(
            err.to_string().contains("checkpoint recovery working set"),
            "{err}"
        );

        cfg.identity.fingerprint_bytes = MAX_FINGERPRINT_BYTES + 1;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("fingerprint_bytes"), "{err}");
        assert!(err.to_string().contains("65535"), "{err}");
    }

    /// Scenario: `identity.ignored_header_bytes` is set to exactly
    /// `u32::MAX` and to one more than `u32::MAX`.
    /// Guarantees: the codec's `u32` `ignored_header_bytes` field bound is
    /// enforced at config-build time: the maximum validates, and one more
    /// is rejected before it could ever fail later at checkpoint-encode
    /// time.
    #[test]
    fn ignored_header_bytes_maximum_is_enforced() {
        let mut cfg = minimal_config();
        cfg.identity.ignored_header_bytes = MAX_IGNORED_HEADER_BYTES;
        cfg.identity.fingerprint_bytes = MIN_FINGERPRINT_BYTES;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_ok());

        cfg.identity.ignored_header_bytes = MAX_IGNORED_HEADER_BYTES + 1;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("ignored_header_bytes"));
    }

    /// Scenario: parsing each documented `encoding` value.
    /// Guarantees: all five Phase 1 encodings (`utf-8`, `ascii`, `utf-16le`,
    /// `utf-16be`, `raw`) parse to their matching enum variant, and an
    /// unrecognized value is rejected.
    #[test]
    fn encoding_enum_covers_every_documented_value() {
        let cases = [
            ("utf-8", Encoding::Utf8),
            ("ascii", Encoding::Ascii),
            ("utf-16le", Encoding::Utf16Le),
            ("utf-16be", Encoding::Utf16Be),
            ("raw", Encoding::Raw),
        ];
        for (text, expected) in cases {
            let cfg = parse(serde_json::json!({
                "include": ["/var/log/app/*.log"],
                "encoding": text
            }))
            .unwrap_or_else(|e| panic!("encoding '{text}' must parse: {e}"));
            assert_eq!(cfg.encoding, expected);
        }

        let invalid = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "encoding": "latin-1"
        }));
        assert!(invalid.is_err(), "unsupported encoding must be rejected");
    }

    /// Scenario: parsing each documented `on_decode_error` value.
    /// Guarantees: all three policies parse to their matching enum variant.
    #[test]
    fn on_decode_error_enum_covers_every_documented_value() {
        let cases = [
            ("preserve_raw", OnDecodeError::PreserveRaw),
            ("replace", OnDecodeError::Replace),
            ("fail", OnDecodeError::Fail),
        ];
        for (text, expected) in cases {
            let cfg = parse(serde_json::json!({
                "include": ["/var/log/app/*.log"],
                "on_decode_error": text
            }))
            .unwrap_or_else(|e| panic!("on_decode_error '{text}' must parse: {e}"));
            assert_eq!(cfg.on_decode_error, expected);
        }
    }

    /// Scenario: `framing.max_line_bytes`, `framing.max_record_bytes`, and
    /// `framing.max_multiline_lines` are each set to zero in turn.
    /// Guarantees: every framing bound is a "nonzero bound"; zero is
    /// rejected for all three.
    #[test]
    fn framing_bounds_must_be_nonzero() {
        let mut cfg = minimal_config();
        cfg.framing.max_line_bytes = 0;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.framing.max_record_bytes = 0;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.framing.max_multiline_lines = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `framing.force_flush_period` is zero, exactly one
    /// millisecond, and an ordinary whole-millisecond value (`500ms`, the
    /// default).
    /// Guarantees: zero validates and means idle partial flushing is
    /// disabled; any nonzero whole-millisecond value down to 1ms validates.
    #[test]
    fn force_flush_period_zero_and_whole_millisecond_values_validate() {
        let mut cfg = minimal_config();
        cfg.framing.force_flush_period = Duration::ZERO;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.framing.force_flush_period = Duration::from_millis(1);
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let cfg = minimal_config();
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());
    }

    /// Scenario: `framing.force_flush_period` is set to a nonzero duration
    /// with a sub-millisecond remainder (`500` microseconds). Constructed
    /// directly on `Config` rather than through `humantime_serde` parsing,
    /// since sub-millisecond precision is not exercised through the
    /// documented duration syntax.
    /// Guarantees: a sub-millisecond remainder is rejected instead of being
    /// silently rounded down to zero (which would mean "disabled", the
    /// opposite of what a nonzero value requests) or rounded up, either of
    /// which would let config validation and the runtime idle-flush timer
    /// disagree about the configured period.
    #[test]
    fn force_flush_period_sub_millisecond_remainder_is_rejected() {
        let mut cfg = minimal_config();
        cfg.framing.force_flush_period = Duration::from_micros(500);
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("force_flush_period"));
        assert!(err.to_string().contains("millisecond"));
    }

    /// Scenario: `framing.force_flush_period` is set to a duration whose
    /// exact millisecond count does not fit `u64` (a whole number of
    /// seconds far larger than `u64::MAX` milliseconds). Constructed
    /// directly on `Config` since no documented duration syntax reaches
    /// this magnitude in practice.
    /// Guarantees: an out-of-range value is rejected rather than silently
    /// saturated to `u64::MAX` milliseconds, which would make the
    /// framing-profile digest (and a later runtime idle-flush timer) use an
    /// incorrect, silently clamped period instead of failing closed.
    #[test]
    fn force_flush_period_exceeding_u64_millis_is_rejected() {
        let mut cfg = minimal_config();
        // u64::MAX seconds, subsec_nanos = 0 (a whole number of seconds, so
        // the whole-millisecond-resolution check passes); its millisecond
        // count vastly exceeds u64::MAX.
        cfg.framing.force_flush_period = Duration::from_secs(u64::MAX);
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("force_flush_period"));
    }

    /// Scenario: computing `canonicalize_force_flush_period_millis`
    /// directly for boundary durations.
    /// Guarantees: the function's return value matches the exact contract
    /// task-level tests above exercise indirectly: zero maps to `0`, whole
    /// milliseconds map exactly, and both failure modes are distinguishable
    /// by their error messages.
    #[test]
    fn canonicalize_force_flush_period_millis_boundary_values() {
        assert_eq!(
            canonicalize_force_flush_period_millis(Duration::ZERO).unwrap(),
            0
        );
        assert_eq!(
            canonicalize_force_flush_period_millis(Duration::from_millis(1)).unwrap(),
            1
        );
        assert_eq!(
            canonicalize_force_flush_period_millis(Duration::from_millis(500)).unwrap(),
            500
        );
        assert!(canonicalize_force_flush_period_millis(Duration::from_micros(500)).is_err());
        assert!(canonicalize_force_flush_period_millis(Duration::from_secs(u64::MAX)).is_err());
    }

    /// Scenario: both `line_start_pattern` and `line_end_pattern` are set.
    /// Guarantees: setting both is rejected at build time, matching the
    /// documented "setting both is rejected" rule.
    #[test]
    fn both_multiline_patterns_is_rejected() {
        let mut cfg = minimal_config();
        cfg.framing.multiline.line_start_pattern = Some("^START".to_owned());
        cfg.framing.multiline.line_end_pattern = Some("^END".to_owned());
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("at most one"));
    }

    /// Scenario: neither `line_start_pattern` nor `line_end_pattern` is set.
    /// Guarantees: the default newline-framing mode is selected, no pattern
    /// is compiled, and the framing-profile digest is still computed.
    #[test]
    fn neither_multiline_pattern_selects_newline_framing() {
        let cfg = minimal_config();
        let runtime = RuntimeConfig::from_config(cfg, "node-1").expect("must validate");
        assert!(runtime.compiled_multiline_pattern.is_none());
        assert_ne!(runtime.framing_profile_digest, [0u8; 32]);
    }

    /// Scenario: `framing.multiline.regex_profile` is set to a value other
    /// than `re2-v1`.
    /// Guarantees: only `re2-v1` is accepted; any other profile identifier
    /// fails to deserialize.
    #[test]
    fn only_re2_v1_regex_profile_is_accepted() {
        let result = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "framing": { "multiline": { "regex_profile": "pcre-v1" } }
        }));
        assert!(
            result.is_err(),
            "unsupported regex_profile must be rejected"
        );

        let ok = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "framing": { "multiline": { "regex_profile": "re2-v1" } }
        }));
        assert!(ok.is_ok(), "re2-v1 must be accepted");
    }

    /// Scenario: a multiline end pattern using a backreference (`\1`), a
    /// lookahead (`(?=`), and a lookbehind (`(?<=`) construct.
    /// Guarantees: every RE2-incompatible construct the `regex` crate
    /// itself refuses is surfaced as a structured `InvalidUserConfig`
    /// instead of panicking or silently accepting it.
    #[test]
    fn re2_incompatible_constructs_are_rejected() {
        for pattern in [r"(foo)\1", r"(?=foo)", r"(?!foo)", r"(?<=foo)", r"(?<!foo)"] {
            let mut cfg = minimal_config();
            cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
            let err = RuntimeConfig::from_config(cfg, "node-1")
                .expect_err(&format!("pattern '{pattern}' must be rejected"));
            assert!(err.to_string().contains("RE2-compatible"));
        }
    }

    /// Scenario: a valid RE2-compatible end pattern is configured.
    /// Guarantees: the pattern compiles, is stored as the runtime's
    /// pre-compiled multiline pattern, and the framing-profile digest
    /// differs from the default newline-framing digest.
    #[test]
    fn valid_end_pattern_compiles_and_changes_digest() {
        let mut cfg = minimal_config();
        cfg.framing.multiline.line_end_pattern = Some("^END request$".to_owned());
        let runtime = RuntimeConfig::from_config(cfg, "node-1").expect("must validate");
        assert!(runtime.compiled_multiline_pattern.is_some());

        let default_runtime =
            RuntimeConfig::from_config(minimal_config(), "node-1").expect("must validate");
        assert_ne!(
            runtime.framing_profile_digest,
            default_runtime.framing_profile_digest
        );
    }

    /// Scenario: an empty multiline pattern string is configured.
    /// Guarantees: an explicitly empty pattern is rejected rather than
    /// silently treated as "no pattern".
    #[test]
    fn empty_multiline_pattern_is_rejected() {
        let mut cfg = minimal_config();
        cfg.framing.multiline.line_start_pattern = Some(String::new());
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: each `limits.*` bound is set to zero, and `max_open_files`
    /// is set above `max_tracked_files`.
    /// Guarantees: every limit is a nonzero bound, and `max_open_files` must
    /// stay `<= max_tracked_files`.
    #[test]
    fn limits_bounds_and_relationship_are_enforced() {
        let mut cfg = minimal_config();
        cfg.limits.max_tracked_files = 0;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.limits.max_pending_candidates = 0;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.limits.max_open_files = 0;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.limits.max_read_bytes_per_turn = 0;
        assert!(RuntimeConfig::from_config(cfg.clone(), "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.limits.max_tracked_files = 10;
        cfg.limits.max_open_files = 11;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("max_open_files"));

        let mut cfg = minimal_config();
        cfg.limits.max_tracked_files = 10;
        cfg.limits.max_open_files = 10;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());
    }

    /// Scenario: discovery retains candidate state while one inventory is
    /// worker-owned, one occupies the event channel, and a third is built
    /// from a temporary evidence vector.
    /// Guarantees: the configured memory coefficients cover all five
    /// fingerprint payload copies, all four path payload copies, and the
    /// minimum inline key/value storage implied by that topology.
    #[test]
    fn identity_memory_formula_covers_discovery_copy_topology() {
        use std::mem::size_of;

        use crate::receivers::filelog_receiver::checkpoint::Locator;
        use crate::receivers::filelog_receiver::discovery::DiscoveredCandidate;
        use crate::receivers::filelog_receiver::identity::CandidateEvidence;

        assert_eq!(DISCOVERY_MAX_SIMULTANEOUS_INVENTORIES, 3);
        assert_eq!(
            DISCOVERY_CANDIDATE_FINGERPRINT_COPIES,
            1 + DISCOVERY_MAX_SIMULTANEOUS_INVENTORIES + 1
        );
        assert_eq!(DISCOVERY_CANDIDATE_PATH_COPIES, 3 + 1);

        let minimum_inline_bytes = size_of::<DiscoveredCandidate>()
            + size_of::<CandidateEvidence>()
            + usize::try_from(DISCOVERY_MAX_SIMULTANEOUS_INVENTORIES)
                .expect("inventory count fits usize")
                * (size_of::<Locator>() + size_of::<Vec<u8>>() + size_of::<usize>());
        assert!(
            usize::try_from(IDENTITY_CANDIDATE_OVERHEAD_BYTES).expect("overhead fits usize")
                >= minimum_inline_bytes
        );
    }

    /// Scenario: every open candidate updates both fingerprint and metadata
    /// while checkpoint preflight retains original and cloned operations,
    /// encoded transaction body and frame, the existing table, and a
    /// transient applied record.
    /// Guarantees: the in-flight coefficients cover the complete durable
    /// update topology, and a concrete configuration that the smaller
    /// discovery-only formula accepted is rejected before allocation.
    #[test]
    fn identity_memory_formula_covers_durable_update_topology() {
        assert_eq!(IDENTITY_OPEN_CANDIDATE_FINGERPRINT_COPIES, 1 + (4 * 2) + 1);
        const {
            assert!(
                IDENTITY_OPEN_CANDIDATE_PATH_COPIES > 3 + 4,
                "event, metadata encoding stages, and applied record must fit"
            );
        }

        let mut cfg = minimal_config();
        cfg.identity.fingerprint_bytes = 22_000;
        cfg.limits.max_open_files = 2_048;
        cfg.limits.max_tracked_files = 2_048;
        cfg.limits.max_pending_candidates = 4_192;
        cfg.checkpoint.compact_after_bytes = 1;

        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("identity reconciliation worst-case working set"),
            "{error}"
        );
    }

    /// Scenario: pending-candidate capacity reaches the exact largest value
    /// whose complete discovery, identity, index, and tracked-state formula
    /// fits one GiB at the other defaults, then increases by one.
    /// Guarantees: checked cross-field accounting accepts the boundary,
    /// rejects the next value before runtime allocation, and identifies the
    /// knobs that reduce the bound.
    #[test]
    fn identity_reconciliation_working_set_is_bounded() {
        let boundary = largest_accepted(1, u64::from(u32::MAX), |candidate| {
            let mut cfg = minimal_config();
            cfg.limits.max_pending_candidates =
                u32::try_from(candidate).expect("search stays in the u32 range");
            RuntimeConfig::from_config(cfg, "node-1").is_ok()
        });
        let mut cfg = minimal_config();
        cfg.limits.max_pending_candidates =
            u32::try_from(boundary + 1).expect("identity boundary is below u32::MAX");

        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("identity reconciliation worst-case working set")
        );
    }

    /// Scenario: `batch.max_records` is zero, at the documented maximum of
    /// 65535, and one above that maximum.
    /// Guarantees: zero is rejected, 65535 validates, and 65536 is rejected
    /// (the OTAP Arrow log record `u16` id space).
    #[test]
    fn batch_max_records_bounds() {
        let mut cfg = minimal_config();
        cfg.batch.max_records = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.batch.max_records = 65_535;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.batch.max_records = 65_536;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `batch.max_bytes` is zero, and `batch.max_flush_period` is
    /// zero.
    /// Guarantees: both are rejected; `batch.max_flush_period` has no
    /// documented "zero disables" meaning, unlike `force_flush_period`.
    #[test]
    fn batch_bytes_and_flush_period_must_be_nonzero() {
        let mut cfg = minimal_config();
        cfg.batch.max_bytes = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.batch.max_flush_period = Duration::ZERO;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `framing.max_record_bytes` is configured close enough to
    /// `batch.max_bytes` that the shared `logical_record_size` overhead
    /// pushes it over the batch bound.
    /// Guarantees: the same logical-size function used by runtime flushing
    /// rejects the configuration at build time instead of deferring the
    /// failure to a runtime record that can never be flushed.
    #[test]
    fn oversized_record_bound_relative_to_batch_bytes_is_rejected() {
        let metadata = MetadataConfig::default();
        let small_line_bytes = 10u64;
        let batch_bytes =
            logical_record_size(small_line_bytes, &metadata, MaxLogSizeBehavior::Split)
                .expect("small body_bytes must not overflow");

        let mut cfg = minimal_config();
        cfg.batch.max_bytes = batch_bytes;
        cfg.framing.max_line_bytes = small_line_bytes;
        // One byte more than `small_line_bytes` pushes only the record bound
        // over `batch_bytes`, since logical_record_size is monotonic in
        // body_bytes and the line bound exactly fits.
        cfg.framing.max_record_bytes = small_line_bytes + 1;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("max_record_bytes"));
    }

    /// Scenario: `framing.max_line_bytes` alone (independent of
    /// `max_record_bytes`) is configured close enough to `batch.max_bytes`
    /// that the overhead pushes it over the bound.
    /// Guarantees: `max_line_bytes` is validated against `batch.max_bytes`
    /// using the same shared logical-size function, not just
    /// `max_record_bytes`.
    #[test]
    fn oversized_line_bound_relative_to_batch_bytes_is_rejected() {
        let metadata = MetadataConfig::default();
        let small_record_bytes = 10u64;
        let batch_bytes =
            logical_record_size(small_record_bytes, &metadata, MaxLogSizeBehavior::Split)
                .expect("small body_bytes must not overflow");

        let mut cfg = minimal_config();
        cfg.batch.max_bytes = batch_bytes;
        cfg.framing.max_record_bytes = small_record_bytes;
        // One byte more than `small_record_bytes` pushes only the line bound
        // over `batch_bytes`, since the line bound is validated first and
        // the record bound exactly fits.
        cfg.framing.max_line_bytes = small_record_bytes + 1;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("max_line_bytes"));
    }

    /// Scenario: enabling both `metadata.include_file_record_offset` and
    /// `metadata.include_file_record_number` increases the fixed attribute
    /// overhead counted by `logical_record_size`.
    /// Guarantees: a `batch.max_bytes` configuration that fits the default
    /// (disabled) metadata overhead is rejected once both metadata flags are
    /// enabled, proving the flags are actually counted.
    #[test]
    fn metadata_flags_increase_logical_record_size() {
        let disabled = logical_record_size(
            10,
            &MetadataConfig {
                include_file_record_offset: false,
                include_file_record_number: false,
            },
            MaxLogSizeBehavior::Split,
        )
        .expect("small body_bytes must not overflow");
        let enabled = logical_record_size(
            10,
            &MetadataConfig {
                include_file_record_offset: true,
                include_file_record_number: true,
            },
            MaxLogSizeBehavior::Split,
        )
        .expect("small body_bytes must not overflow");
        assert!(enabled > disabled);

        let mut cfg = minimal_config();
        cfg.framing.max_record_bytes = 10;
        cfg.framing.max_line_bytes = 10;
        cfg.batch.max_bytes = enabled - 1;
        cfg.metadata.include_file_record_offset = true;
        cfg.metadata.include_file_record_number = true;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `MaxLogSizeBehavior::Truncate` is configured instead of the
    /// default `Split`.
    /// Guarantees: `logical_record_size` counts the truncate marker
    /// attribute's overhead rather than the (larger) fragment-id/index/last
    /// attribute set, so the two policies are not conflated.
    #[test]
    fn oversize_behavior_changes_logical_record_size_overhead() {
        let metadata = MetadataConfig::default();
        let split = logical_record_size(10, &metadata, MaxLogSizeBehavior::Split)
            .expect("small body_bytes must not overflow");
        let truncate = logical_record_size(10, &metadata, MaxLogSizeBehavior::Truncate)
            .expect("small body_bytes must not overflow");
        assert_ne!(split, truncate);
    }

    /// Scenario: `body_bytes` is `u64::MAX`, so adding any fixed attribute
    /// overhead on top of it would overflow `u64`.
    /// Guarantees: `logical_record_size` reports
    /// `LogicalSizeError::Overflow` via checked arithmetic instead of
    /// silently saturating to `u64::MAX`, which would let a `body_bytes` /
    /// `batch.max_bytes` pair both clamped to `u64::MAX` "validate"
    /// successfully despite being unrepresentable and unallocatable.
    #[test]
    fn logical_record_size_reports_overflow_instead_of_saturating() {
        let metadata = MetadataConfig::default();
        let err = logical_record_size(u64::MAX, &metadata, MaxLogSizeBehavior::Split)
            .expect_err("u64::MAX body_bytes plus any overhead must overflow");
        assert_eq!(err, LogicalSizeError::Overflow);
    }

    /// Scenario: `framing.max_record_bytes` is `u64::MAX` and
    /// `batch.max_bytes` is also `u64::MAX`.
    /// Guarantees: config validation rejects this configuration with an
    /// `InvalidUserConfig` error (an overflowing logical size) rather than
    /// accepting it because a saturating comparison would coincidentally
    /// clamp both sides to the same value.
    #[test]
    fn max_u64_record_bytes_and_batch_bytes_is_rejected() {
        let mut cfg = minimal_config();
        cfg.framing.max_record_bytes = u64::MAX;
        cfg.framing.max_line_bytes = 10;
        cfg.batch.max_bytes = u64::MAX;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("overflows u64"));
    }

    /// Scenario: `ensure_fits_usize` is called with an ordinary in-range
    /// value and with the largest value `usize` can represent on this
    /// target.
    /// Guarantees: both accept. On a 64-bit target `usize::MAX as u64`
    /// equals `u64::MAX`, so there is no larger `u64` value to exercise the
    /// rejection branch directly on this build; that branch (a plain
    /// `usize::try_from` failure) is otherwise straightforward and is what
    /// protects a 32-bit target from being asked to allocate a byte-size
    /// field wider than its native `usize`.
    #[test]
    fn ensure_fits_usize_accepts_values_within_usize_range() {
        assert!(ensure_fits_usize("test.field", 1024).is_ok());
        assert!(ensure_fits_usize("test.field", usize::MAX as u64).is_ok());
    }

    /// Scenario: `limits.max_read_bytes_per_turn` is configured to an
    /// ordinary value.
    /// Guarantees: `validate_limits` calls `ensure_fits_usize` on
    /// `max_read_bytes_per_turn`, so a value that fits comfortably
    /// continues to validate (a regression guard for the added usize-fit
    /// check alongside the existing nonzero check).
    #[test]
    fn max_read_bytes_per_turn_fits_usize() {
        let mut cfg = minimal_config();
        cfg.limits.max_read_bytes_per_turn = 4096;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());
    }

    /// Scenario: `rotation.rotate_wait` is set to zero.
    /// Guarantees: a zero rotation grace period is rejected; Phase 1 has no
    /// documented "zero disables" meaning for this bound.
    #[test]
    fn rotate_wait_must_be_nonzero() {
        let mut cfg = minimal_config();
        cfg.rotation.rotate_wait = Duration::ZERO;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: parsing each documented `rotation.on_truncate` value.
    /// Guarantees: both `fail` and `read_new` parse to their matching enum
    /// variant.
    #[test]
    fn on_truncate_enum_covers_every_documented_value() {
        for (text, expected) in [
            ("fail", OnTruncate::Fail),
            ("read_new", OnTruncate::ReadNew),
        ] {
            let cfg = parse(serde_json::json!({
                "include": ["/var/log/app/*.log"],
                "rotation": { "on_truncate": text }
            }))
            .unwrap_or_else(|e| panic!("on_truncate '{text}' must parse: {e}"));
            assert_eq!(cfg.rotation.on_truncate, expected);
        }
    }

    /// Scenario: `checkpoint.compact_after_bytes`,
    /// `checkpoint.compact_after_transactions`, `checkpoint.ownership_timeout`,
    /// and `checkpoint.max_consecutive_failures` are each set to zero.
    /// Guarantees: every checkpoint bound, including both compaction
    /// triggers, is a nonzero bound.
    #[test]
    fn checkpoint_bounds_must_be_nonzero() {
        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_bytes = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_transactions = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.checkpoint.ownership_timeout = Duration::ZERO;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.checkpoint.max_consecutive_failures = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// The exact remedies the store's size formulas attach to a bound that
    /// cannot be honored, asserted verbatim so a rejection always names the
    /// knob an operator has to change.
    const WAL_REMEDY: &str = "reduce checkpoint.compact_after_bytes or identity.fingerprint_bytes";
    const RECOVERY_REMEDY: &str = "reduce limits.max_tracked_files, \
                                   checkpoint.compact_after_bytes, or \
                                   identity.fingerprint_bytes";

    /// Scenario: the checkpoint size knobs that jointly determine artifact
    /// sizes and recovery memory -- `compact_after_bytes`,
    /// `limits.max_tracked_files`, and `identity.fingerprint_bytes` -- are
    /// validated at their defaults, at the combined recovery-working-set
    /// boundary, and one step beyond each.
    /// Guarantees: config validation runs the durable store's own checked
    /// formulas, so accepted boundary values fit the artifact and peak
    /// recovery-memory ceilings without rounding, while each one-step
    /// rejection names all knobs that can reduce the combined bound.
    #[test]
    fn checkpoint_size_bounds_must_stay_recoverable() {
        let defaults = StoreLimits::derive(
            DEFAULT_COMPACT_AFTER_BYTES,
            DEFAULT_MAX_TRACKED_FILES,
            DEFAULT_FINGERPRINT_BYTES,
        )
        .expect("the shipped defaults are recoverable");
        assert!(defaults.max_snapshot_bytes <= store_limits::ARTIFACT_BYTES_CEILING);
        assert!(defaults.max_wal_bytes <= store_limits::ARTIFACT_BYTES_CEILING);
        assert!(
            defaults.max_recovery_working_bytes <= store_limits::RECOVERY_WORKING_BYTES_CEILING
        );
        assert!(RuntimeConfig::from_config(minimal_config(), "node-1").is_ok());

        // Largest tracked-file population whose complete recovery working
        // set remains bounded at the other defaults.
        let boundary_files = u32::try_from(largest_accepted(
            u64::from(DEFAULT_MAX_TRACKED_FILES),
            u64::from(u32::MAX),
            |candidate| {
                StoreLimits::derive(
                    DEFAULT_COMPACT_AFTER_BYTES,
                    candidate as u32,
                    DEFAULT_FINGERPRINT_BYTES,
                )
                .is_ok()
            },
        ))
        .expect("the tracked-file boundary fits u32");
        let mut cfg = minimal_config();
        cfg.limits.max_tracked_files = boundary_files;
        assert!(
            RuntimeConfig::from_config(cfg, "node-1").is_ok(),
            "the largest recoverable tracked-file population must validate"
        );

        let mut cfg = minimal_config();
        cfg.limits.max_tracked_files = boundary_files + 1;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            err.to_string().contains("checkpoint recovery working set"),
            "{err}"
        );
        assert!(err.to_string().contains(RECOVERY_REMEDY), "{err}");

        // Largest compaction threshold whose complete recovery working set
        // remains bounded at the other defaults.
        let boundary_bytes = largest_accepted(
            DEFAULT_COMPACT_AFTER_BYTES,
            store_limits::ARTIFACT_BYTES_CEILING,
            |candidate| {
                StoreLimits::derive(
                    candidate,
                    DEFAULT_MAX_TRACKED_FILES,
                    DEFAULT_FINGERPRINT_BYTES,
                )
                .is_ok()
            },
        );
        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_bytes = boundary_bytes;
        assert!(
            RuntimeConfig::from_config(cfg, "node-1").is_ok(),
            "the largest recoverable compaction threshold must validate"
        );

        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_bytes = boundary_bytes + 1;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            err.to_string().contains("checkpoint recovery working set"),
            "{err}"
        );
        assert!(err.to_string().contains(RECOVERY_REMEDY), "{err}");

        // Nothing saturates: an unrepresentable worst case is an error, not
        // a clamped bound that would claim it fits.
        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_bytes = u64::MAX;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("overflows u64"), "{err}");
        assert!(err.to_string().contains(WAL_REMEDY), "{err}");

        // Find the widest fingerprint window whose complete recovery
        // working set remains bounded at the durable-store defaults. Use
        // minimal pending/open populations so this assertion isolates the
        // store boundary from the separate runtime reconciliation ceiling.
        let boundary_fingerprint = largest_accepted(
            DEFAULT_FINGERPRINT_BYTES,
            MAX_FINGERPRINT_BYTES,
            |candidate| {
                StoreLimits::derive(
                    DEFAULT_COMPACT_AFTER_BYTES,
                    DEFAULT_MAX_TRACKED_FILES,
                    candidate,
                )
                .is_ok()
            },
        );
        let mut cfg = minimal_config();
        cfg.identity.fingerprint_bytes = boundary_fingerprint;
        cfg.limits.max_pending_candidates = 1;
        cfg.limits.max_open_files = 1;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.identity.fingerprint_bytes = boundary_fingerprint + 1;
        cfg.limits.max_pending_candidates = 1;
        cfg.limits.max_open_files = 1;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            err.to_string().contains("checkpoint recovery working set"),
            "{err}"
        );
        assert!(err.to_string().contains(RECOVERY_REMEDY), "{err}");

        // The format's widest representable fingerprint is consequently
        // refused at the shipped population and WAL defaults.
        let mut cfg = minimal_config();
        cfg.identity.fingerprint_bytes = MAX_FINGERPRINT_BYTES;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            err.to_string().contains("checkpoint recovery working set"),
            "{err}"
        );
        assert!(err.to_string().contains(RECOVERY_REMEDY), "{err}");
    }

    /// Scenario: `checkpoint.id` is omitted, and `RuntimeConfig` is built
    /// without an externally supplied default identity (via `TryFrom`).
    /// Guarantees: a missing `checkpoint.id` with no default available is
    /// rejected with an actionable message, instead of silently resolving
    /// to an empty or ambiguous namespace.
    #[test]
    fn missing_checkpoint_id_without_default_is_rejected() {
        let cfg = minimal_config();
        let err = RuntimeConfig::try_from(cfg).unwrap_err();
        assert!(err.to_string().contains("checkpoint.id"));
    }

    /// Scenario: `checkpoint.id` is omitted, but `from_config` is given a
    /// default identity (as a future factory would supply the pipeline
    /// node identity).
    /// Guarantees: the default is used to resolve `checkpoint_id`, matching
    /// the documented "defaults to the configured node identity" behavior.
    #[test]
    fn missing_checkpoint_id_uses_supplied_default() {
        let cfg = minimal_config();
        let runtime = RuntimeConfig::from_config(cfg, "my-node").expect("must validate");
        assert_eq!(runtime.checkpoint_id, "my-node");
    }

    /// Scenario: `checkpoint.id` contains a character outside the accepted
    /// ASCII alphanumeric / `_` / `-` / `.` set.
    /// Guarantees: an unsafe checkpoint id is rejected before it can reach
    /// the namespace-path helper.
    #[test]
    fn checkpoint_id_rejects_unsafe_characters() {
        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("app logs/../etc".to_owned());
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: `checkpoint.id` is exactly `CHECKPOINT_ID_SEGMENT_MAX_BYTES`
    /// (255) ASCII-safe bytes, and one byte more (256 bytes).
    /// Guarantees: 255 bytes validates (its percent-encoded path segment is
    /// 1:1 with the safe-charset id and so is also 255 bytes), and 256
    /// bytes is rejected -- even though it is still `<=
    /// NAMESPACE_ID_MAX_BYTES` (256) -- because its encoded path segment
    /// would exceed the common 255-byte filesystem `NAME_MAX`, the tighter
    /// of the two documented bounds.
    #[test]
    fn checkpoint_id_encoded_segment_boundary_is_enforced() {
        assert_eq!(CHECKPOINT_ID_SEGMENT_MAX_BYTES, 255);

        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("a".repeat(255));
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("a".repeat(256));
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("percent-encoded path segment"));
    }

    /// Scenario: `checkpoint.id` is exactly `NAMESPACE_ID_MAX_BYTES` (256)
    /// bytes but constructed so its raw length, not its encoded length, is
    /// the first bound crossed.
    /// Guarantees: the raw `NAMESPACE_ID_MAX_BYTES` check and the tighter
    /// encoded-segment check agree; a 256-byte id is rejected by one or the
    /// other regardless of which fires first.
    #[test]
    fn checkpoint_id_never_exceeds_namespace_id_max_bytes() {
        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("a".repeat(NAMESPACE_ID_MAX_BYTES));
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: an include pattern's literal (non-glob) prefix resolves
    /// exactly to the receiver's own resolved checkpoint namespace
    /// directory, and a second pattern resolves to a file directly inside
    /// that namespace.
    /// Guarantees: both cases are rejected unconditionally, so the receiver
    /// can never be configured to read back its own checkpoint state.
    #[test]
    fn direct_include_of_checkpoint_namespace_is_rejected() {
        let namespace_dir = checkpoint_namespace_dir("app-logs");

        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("app-logs".to_owned());
        cfg.include = vec![namespace_dir.to_string_lossy().into_owned()];
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("app-logs".to_owned());
        cfg.include = vec![format!("{}/CURRENT", namespace_dir.display())];
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: an include pattern targets an unrelated directory that
    /// does not overlap the resolved checkpoint namespace.
    /// Guarantees: the namespace-collision check does not reject ordinary,
    /// unrelated include patterns.
    #[test]
    fn unrelated_include_does_not_collide_with_checkpoint_namespace() {
        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("app-logs".to_owned());
        cfg.include = vec!["/var/log/app/*.log".to_owned()];
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());
    }

    /// Scenario: an include pattern targets the checkpoint namespace but
    /// prefixes it with a redundant leading `./` CurDir component (for
    /// example `./.otap-state/filelog/app-logs/*.log`).
    /// Guarantees: the leading `./` does not let the pattern bypass the
    /// direct-checkpoint-namespace-inclusion rejection; `./x` and `x` are
    /// recognized as the same directory.
    #[test]
    fn leading_curdir_does_not_bypass_checkpoint_namespace_rejection() {
        let namespace_dir = checkpoint_namespace_dir("app-logs");

        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("app-logs".to_owned());
        cfg.include = vec![format!("./{}/*.log", namespace_dir.display())];
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("checkpoint namespace"));
    }

    /// Scenario: computing the literal glob prefix of a pattern with a
    /// leading `./` component directly.
    /// Guarantees: [`glob_literal_prefix`] normalizes away the leading
    /// `CurDir` component so it compares equal to the same path without it.
    #[test]
    fn glob_literal_prefix_normalizes_leading_curdir() {
        let with_curdir = glob_literal_prefix("./.otap-state/filelog/app-logs/*.log");
        let without_curdir = glob_literal_prefix(".otap-state/filelog/app-logs/*.log");
        assert_eq!(with_curdir, without_curdir);
        assert_eq!(with_curdir, PathBuf::from(".otap-state/filelog/app-logs"));
    }

    #[cfg(unix)]
    /// Scenario: a Unix glob component escapes metacharacters and a literal
    /// backslash that are part of an exact filename.
    /// Guarantees: the derived filesystem prefix removes glob escapes while
    /// retaining each escaped character as a literal path byte.
    #[test]
    fn glob_literal_prefix_unescapes_unix_literal_components() {
        assert_eq!(
            glob_literal_prefix(r"/var/log/app\[1\]\*\\name.log"),
            PathBuf::from(r"/var/log/app[1]*\name.log")
        );
    }

    /// Scenario: stripping a leading `CurDir` component from a handful of
    /// representative paths.
    /// Guarantees: [`strip_leading_curdir`] removes only a genuinely
    /// leading `.` component (which `Path::components` -- unlike an
    /// internal `.` -- otherwise preserves) and leaves every other path
    /// unchanged, so the namespace side of the collision check
    /// ([`checkpoint_namespace_dir`]) normalizes equivalently to the
    /// include-pattern side.
    #[test]
    fn strip_leading_curdir_removes_only_a_leading_component() {
        assert_eq!(
            strip_leading_curdir(Path::new("./a/b")),
            PathBuf::from("a/b")
        );
        assert_eq!(strip_leading_curdir(Path::new("././a")), PathBuf::from("a"));
        assert_eq!(strip_leading_curdir(Path::new("a/b")), PathBuf::from("a/b"));
        // An internal `.` component is already normalized away by
        // `Path::components` itself, independent of this helper.
        assert_eq!(
            strip_leading_curdir(Path::new("a/./b")),
            PathBuf::from("a/b")
        );
    }

    /// Scenario: `retry.max_attempts` is zero, `retry.initial_backoff` is
    /// zero, and `retry.max_backoff` is set below `retry.initial_backoff`.
    /// Guarantees: all three retry/backoff relationships are enforced, and
    /// `max_backoff == initial_backoff` (the boundary) is accepted.
    #[test]
    fn retry_backoff_relationships_are_enforced() {
        let mut cfg = minimal_config();
        cfg.retry.max_attempts = 0;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.retry.initial_backoff = Duration::ZERO;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.retry.initial_backoff = Duration::from_secs(2);
        cfg.retry.max_backoff = Duration::from_secs(1);
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());

        let mut cfg = minimal_config();
        cfg.retry.initial_backoff = Duration::from_secs(1);
        cfg.retry.max_backoff = Duration::from_secs(1);
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());
    }

    /// Scenario: parsing each documented `on_nack` value.
    /// Guarantees: both `fail` and `drop_and_continue` parse to their
    /// matching enum variant.
    #[test]
    fn on_nack_enum_covers_every_documented_value() {
        for (text, expected) in [
            ("fail", OnNack::Fail),
            ("drop_and_continue", OnNack::DropAndContinue),
        ] {
            let cfg = parse(serde_json::json!({
                "include": ["/var/log/app/*.log"],
                "on_nack": text
            }))
            .unwrap_or_else(|e| panic!("on_nack '{text}' must parse: {e}"));
            assert_eq!(cfg.on_nack, expected);
        }
    }

    /// Scenario: parsing each documented `start_at` value.
    /// Guarantees: both `beginning` and `end` parse to their matching enum
    /// variant.
    #[test]
    fn start_at_enum_covers_every_documented_value() {
        for (text, expected) in [("beginning", StartAt::Beginning), ("end", StartAt::End)] {
            let cfg = parse(serde_json::json!({
                "include": ["/var/log/app/*.log"],
                "start_at": text
            }))
            .unwrap_or_else(|e| panic!("start_at '{text}' must parse: {e}"));
            assert_eq!(cfg.start_at, expected);
        }
    }

    /// Scenario: parsing each documented `identity.on_recovery_mismatch`
    /// value.
    /// Guarantees: `beginning`, `skip_to_end`, and `fail` all parse to
    /// their matching enum variant.
    #[test]
    fn on_recovery_mismatch_enum_covers_every_documented_value() {
        for (text, expected) in [
            ("beginning", OnRecoveryMismatch::Beginning),
            ("skip_to_end", OnRecoveryMismatch::SkipToEnd),
            ("fail", OnRecoveryMismatch::Fail),
        ] {
            let cfg = parse(serde_json::json!({
                "include": ["/var/log/app/*.log"],
                "identity": { "on_recovery_mismatch": text }
            }))
            .unwrap_or_else(|e| panic!("on_recovery_mismatch '{text}' must parse: {e}"));
            assert_eq!(cfg.identity.on_recovery_mismatch, expected);
        }
    }

    /// Scenario: byte-size fields accept both a plain numeric value and a
    /// unit-suffixed string, matching Appendix C's mixed examples
    /// (`fingerprint_bytes: 1000` vs `max_line_bytes: 1MiB`).
    /// Guarantees: `deserialize_byte_size` is wired to every documented
    /// byte-size field, not just a subset.
    #[test]
    fn byte_size_fields_accept_units_and_plain_numbers() {
        let cfg = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "identity": { "fingerprint_bytes": 1000 },
            "framing": { "max_line_bytes": "1MiB", "max_record_bytes": "1 MiB" },
            "limits": { "max_read_bytes_per_turn": "128KiB" },
            "batch": { "max_bytes": "8MiB" },
            "checkpoint": { "compact_after_bytes": "64MiB" }
        }))
        .expect("byte-size fields must accept units and plain numbers");
        assert_eq!(cfg.identity.fingerprint_bytes, 1000);
        assert_eq!(cfg.framing.max_line_bytes, 1024 * 1024);
        assert_eq!(cfg.framing.max_record_bytes, 1024 * 1024);
        assert_eq!(cfg.limits.max_read_bytes_per_turn, 128 * 1024);
        assert_eq!(cfg.batch.max_bytes, 8 * 1024 * 1024);
        assert_eq!(cfg.checkpoint.compact_after_bytes, 64 * 1024 * 1024);
    }
}
