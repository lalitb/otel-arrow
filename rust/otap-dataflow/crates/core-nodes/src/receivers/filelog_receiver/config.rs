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
//! integration (built on [`checkpoint::framing_profile`]). Runtime discovery,
//! framing, identity, reader, and checkpoint behavior lives in sibling
//! modules; this configuration module performs no filesystem I/O and
//! registers no component factory.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

use super::checkpoint::framing_profile;
use super::checkpoint::namespace::{CheckpointNamespace, CheckpointNamespaceError};
use super::checkpoint::primitives::{
    ADVISORY_PATH_FIXED_BYTES, ADVISORY_PATH_STORED_MAX_BYTES, COMMITTED_FRONTIER_GUARD_LEN,
    FINGERPRINT_MAX_BYTES, FINGERPRINT_PROFILE_VERSION, FRAMING_PATTERN_MAX_BYTES,
    MAX_PROGRESS_TX_FRAME_BYTES, TX_FRAME_CRC_BYTES, TX_HEADER_BYTES,
    UPDATE_PROGRESS_MAX_OP_FRAME_BYTES, WAL_MAX_OPS_PER_TX,
};
use super::checkpoint::store::limits::{RECOVERY_WORKING_BYTES_CEILING, StoreLimits};

/// URN for the filelog receiver.
pub const FILELOG_RECEIVER_URN: &str = "urn:otel:receiver:filelog";

/// Minimum accepted `identity.fingerprint_bytes`.
const MIN_FINGERPRINT_BYTES: u64 = 16;

/// Maximum accepted `identity.fingerprint_bytes`: the checkpoint codec
/// stores `fingerprint` as a `u16`-length-prefixed byte field, so a value
/// larger than this can never round-trip through the durable format.
const MAX_FINGERPRINT_BYTES: u64 = FINGERPRINT_MAX_BYTES as u64;
/// RE2 rejects counted repetition bounds above 1000.
const RE2_MAX_COUNTED_REPETITION: u16 = 1000;
/// Maximum compiled regex program size for one multiline matcher.
const MULTILINE_REGEX_SIZE_LIMIT_BYTES: usize = 10 * 1024 * 1024;
/// Maximum lazy-DFA cache retained by one multiline matcher.
const MULTILINE_REGEX_DFA_SIZE_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// Conservative process-memory ceiling for one identity reconciliation pass.
const IDENTITY_RECONCILIATION_BYTES_CEILING: u64 = 1024 * 1024 * 1024;
/// Complete modeled `AdvisoryPath` value: fixed fields plus retained bytes.
const ADVISORY_PATH_MODELED_BYTES: u64 =
    ADVISORY_PATH_FIXED_BYTES as u64 + ADVISORY_PATH_STORED_MAX_BYTES as u64;
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
/// Resource terms whose bounded populations do not yet have a reviewed byte
/// coefficient. They are reported explicitly and excluded from numeric
/// subtotals rather than assigned an invented RSS value.
const RESOURCE_ADMISSION_UNMEASURED_TERMS: [&str; 6] = [
    "checkpoint runtime maintenance scratch: checkpoint.compact_after_bytes, checkpoint.compact_after_transactions, limits.max_tracked_files (base table and progress frame already modeled)",
    "decoder and fixed framer object state: limits.max_open_files, framing.max_line_bytes, framing.max_record_bytes (payload and resident population already modeled)",
    "bounded channel and queue storage: fixed internal capacities (inventory and Arrow payloads already modeled)",
    "incremental per-receiver lease registry state: limits.max_tracked_files (reader guard already modeled; process registry shared)",
    "Arrow and allocator/library overhead: batch.max_records, batch.max_bytes (logical retained and carry-over bytes already modeled)",
    "fixed worker/runtime state and excess native path storage: discovery patterns and limits.* (variable modeled payloads excluded)",
];
/// Stable bounded text emitted with the startup admission report.
const RESOURCE_ADMISSION_UNMEASURED_TERMS_TEXT: &str = "checkpoint runtime maintenance scratch: checkpoint.compact_after_bytes, checkpoint.compact_after_transactions, limits.max_tracked_files (base table and progress frame already modeled); \
     decoder and fixed framer object state: limits.max_open_files, framing.max_line_bytes, framing.max_record_bytes (payload and resident population already modeled); \
     bounded channel and queue storage: fixed internal capacities (inventory and Arrow payloads already modeled); \
     incremental per-receiver lease registry state: limits.max_tracked_files (reader guard already modeled; process registry shared); \
     Arrow and allocator/library overhead: batch.max_records, batch.max_bytes (logical retained and carry-over bytes already modeled); \
     fixed worker/runtime state and excess native path storage: discovery patterns and limits.* (variable modeled payloads excluded)";

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
/// Minimum `discovery.reconcile_interval`.
const MIN_RECONCILE_INTERVAL: Duration = Duration::from_millis(100);
/// Maximum `discovery.reconcile_interval`.
const MAX_RECONCILE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Default `discovery.reconcile_interval`.
const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
/// Default `discovery.reconcile_jitter_percent`.
const DEFAULT_RECONCILE_JITTER_PERCENT: u8 = 10;
/// Maximum `discovery.reconcile_jitter_percent`.
const MAX_RECONCILE_JITTER_PERCENT: u8 = 25;
/// Minimum `reader.eof_reprobe_interval`.
const MIN_EOF_REPROBE_INTERVAL: Duration = Duration::from_millis(10);
/// Maximum `reader.eof_reprobe_interval`.
const MAX_EOF_REPROBE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Default `reader.eof_reprobe_interval`.
const DEFAULT_EOF_REPROBE_INTERVAL: Duration = Duration::from_millis(250);
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
const DISCOVERY_TRAVERSAL_DESCRIPTOR_BUDGET: u64 = 1;
const TRANSIENT_PROBE_DESCRIPTOR_BUDGET: u64 = 1;
const CHECKPOINT_DESCRIPTOR_BUDGET: u64 = 8;

/// Registered semantic-convention attribute keys always attached to an
/// emitted record when complete path evidence is lossless text, counted by
/// [`checked_logical_record_size`].
pub(crate) const ATTR_KEY_LOG_FILE_PATH: &str = "log.file.path";
pub(crate) const ATTR_KEY_LOG_FILE_NAME: &str = "log.file.name";
/// Project-owned bounded native-path fallback attributes.
pub(crate) const ATTR_KEY_PATH_KIND: &str = "otel.arrow.filelog.path.kind";
pub(crate) const ATTR_KEY_PATH_NATIVE: &str = "otel.arrow.filelog.path.native";
pub(crate) const ATTR_KEY_PATH_TRUNCATED: &str = "otel.arrow.filelog.path.truncated";
pub(crate) const ATTR_KEY_PATH_SHA256: &str = "otel.arrow.filelog.path.sha256";
/// Project-owned split-fragment attributes.
pub(crate) const ATTR_KEY_FRAGMENT_ID: &str = "otel.arrow.filelog.fragment.id";
pub(crate) const ATTR_KEY_FRAGMENT_INDEX: &str = "otel.arrow.filelog.fragment.index";
pub(crate) const ATTR_KEY_FRAGMENT_IS_LAST: &str = "otel.arrow.filelog.fragment.is_last";
pub(crate) const ATTR_KEY_FRAGMENT_BODY_START: &str = "otel.arrow.filelog.fragment.body.start";
pub(crate) const ATTR_KEY_FRAGMENT_BODY_END: &str = "otel.arrow.filelog.fragment.body.end";
pub(crate) const ATTR_KEY_FRAGMENT_FRAME_START: &str = "otel.arrow.filelog.fragment.frame.start";
pub(crate) const ATTR_KEY_FRAGMENT_FRAME_END: &str = "otel.arrow.filelog.fragment.frame.end";
pub(crate) const ATTR_KEY_RECORD_TRUNCATED: &str = "otel_arrow.filelog.record.truncated";
pub(crate) const ATTR_KEY_FLUSH_REASON: &str = "otel_arrow.filelog.flush.reason";
pub(crate) const ATTR_KEY_TERMINAL_UNTERMINATED: &str = "otel.arrow.filelog.terminal_unterminated";
pub(crate) const ATTR_KEY_DECODE_ERROR_POLICY: &str = "otel_arrow.filelog.decode.error.policy";
pub(crate) const ATTR_KEY_DECODE_ERROR_COUNT: &str = "otel_arrow.filelog.decode.error_count";

pub(crate) const PATH_KIND_UNIX_BYTES: &str = "unix_bytes";
pub(crate) const PATH_KIND_WINDOWS_UTF16LE: &str = "windows_utf16le";
const MAX_PATH_KIND_VALUE_BYTES: u64 = PATH_KIND_WINDOWS_UTF16LE.len() as u64;
const MAX_NATIVE_PATH_ATTRIBUTE_VALUE_BYTES: u64 = ADVISORY_PATH_STORED_MAX_BYTES as u64;
const MAX_REGISTERED_PATH_TEXT_VALUE_BYTES: u64 = 3 * (ADVISORY_PATH_STORED_MAX_BYTES as u64 / 2);
const SHA256_HEX_VALUE_BYTES: u64 = 64;
/// Conservative reserved bytes for a decimal-encoded `u64` attribute value
/// (decode-error count), sized for the longest possible `u64` (20 digits).
const RESERVED_DECIMAL_U64_VALUE_BYTES: u64 = 20;
/// Length in bytes of the fixed-width lowercase-hex fragment id value.
const FRAGMENT_ID_VALUE_BYTES: u64 = 64;
/// Conservative fixed per-record bookkeeping overhead not attributable to
/// any single attribute (Arrow builder / OTAP record bookkeeping). Matches
/// the "conservative fixed per-record overhead" language in the design.
pub(crate) const FIXED_PER_RECORD_OVERHEAD_BYTES: u64 = 128;

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

impl OnDecodeError {
    fn to_framing_profile(self) -> framing_profile::FramingOnDecodeError {
        match self {
            OnDecodeError::PreserveRaw => framing_profile::FramingOnDecodeError::PreserveRaw,
            OnDecodeError::Replace => framing_profile::FramingOnDecodeError::Replace,
            OnDecodeError::Fail => framing_profile::FramingOnDecodeError::Fail,
        }
    }
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

/// Behavior applied once the bounded retry budget is exhausted (Phase 1
/// treats every aggregate downstream Nack and pre-publication `NoRoute`
/// uniformly: `permanent`, `cause`, and free-form reason text never bypass
/// retry, so this policy applies only at exhaustion, never earlier).
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
    /// Base delay between completed full reconciliation passes.
    #[serde(
        default = "DiscoveryConfig::default_reconcile_interval",
        with = "humantime_serde"
    )]
    pub reconcile_interval: Duration,
    /// Per-pass symmetric jitter percentage.
    #[serde(default = "DiscoveryConfig::default_reconcile_jitter_percent")]
    pub reconcile_jitter_percent: u8,
}

impl DiscoveryConfig {
    const fn default_reconcile_interval() -> Duration {
        DEFAULT_RECONCILE_INTERVAL
    }
    const fn default_reconcile_jitter_percent() -> u8 {
        DEFAULT_RECONCILE_JITTER_PERCENT
    }
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            reconcile_interval: Self::default_reconcile_interval(),
            reconcile_jitter_percent: Self::default_reconcile_jitter_percent(),
        }
    }
}

/// Admitted-reader scheduling cadence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderConfig {
    /// Delay before reprobing one validated handle at temporary EOF.
    #[serde(
        default = "ReaderConfig::default_eof_reprobe_interval",
        with = "humantime_serde"
    )]
    pub eof_reprobe_interval: Duration,
}

impl ReaderConfig {
    const fn default_eof_reprobe_interval() -> Duration {
        DEFAULT_EOF_REPROBE_INTERVAL
    }
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            eof_reprobe_interval: Self::default_eof_reprobe_interval(),
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
    /// Maximum resident tail readers with an open operating-system handle.
    /// Discovery uses at most one additional transient probe handle.
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
    /// Explicit stable checkpoint identifier. When omitted, the factory
    /// supplies a stable digest of pipeline-group, pipeline, node, and
    /// receiver-instance names.
    #[serde(default)]
    pub id: Option<String>,
    /// Interval between durable syncs. Zero means sync every Ack
    /// transaction.
    #[serde(
        default = "CheckpointConfig::default_sync_interval",
        with = "humantime_serde"
    )]
    pub sync_interval: Duration,
    /// Compact before an append would make the complete WAL, including its
    /// fixed header, exceed this many bytes.
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

/// Retry budget for a Nacked, `NoRoute`, or resent batch.
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
    /// Admitted-reader scheduling cadence.
    #[serde(default)]
    pub reader: ReaderConfig,
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
    /// Retry budget for a Nacked, `NoRoute`, or resent batch.
    #[serde(default)]
    pub retry: RetryConfig,
    /// Behavior applied once the retry budget is exhausted.
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
            reader: ReaderConfig::default(),
            ignore_older_than: Self::default_ignore_older_than(),
            identity: IdentityConfig::default(),
            encoding: Encoding::default(),
            on_decode_error: OnDecodeError::default(),
            framing: FramingConfig::default(),
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

/// Error returned by [`checked_logical_record_size`] when its checked-arithmetic
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

/// Logical byte contribution of one projected attribute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LogicalAttributeSize {
    key_bytes: u64,
    value_bytes: u64,
}

impl LogicalAttributeSize {
    /// Creates one contribution from the exact key and the value's logical
    /// length.
    pub(crate) fn new(key: &str, value_bytes: u64) -> Result<Self, LogicalSizeError> {
        Ok(Self {
            key_bytes: u64::try_from(key.len()).map_err(|_| LogicalSizeError::Overflow)?,
            value_bytes,
        })
    }
}

/// Returns the logical length of a String attribute value.
pub(crate) fn logical_string_value_len(value: &str) -> Result<u64, LogicalSizeError> {
    u64::try_from(value.len()).map_err(|_| LogicalSizeError::Overflow)
}

/// Returns the conservative decimal-text length used for an Int attribute.
///
/// OTAP stores the value as an integer; sizing charges the number of bytes
/// needed by its base-10 spelling, including a leading minus sign.
pub(crate) const fn logical_int_value_len(value: i64) -> u64 {
    let mut magnitude = value.unsigned_abs();
    let mut digits = 1u64;
    while magnitude >= 10 {
        magnitude /= 10;
        digits += 1;
    }
    if value < 0 { digits + 1 } else { digits }
}

/// Returns the exact lowercase textual width charged for a Bool attribute.
pub(crate) const fn logical_bool_value_len(value: bool) -> u64 {
    if value { 4 } else { 5 }
}

/// The single low-level logical-size primitive shared by configuration and
/// runtime projection.
///
/// It sums body bytes, every actual (or deliberately reserved worst-case)
/// attribute key and value contribution, and the documented 128-byte fixed
/// record term. It never serializes Arrow or OTLP data to estimate size.
pub(crate) fn checked_logical_record_size(
    body_bytes: u64,
    attributes: impl IntoIterator<Item = LogicalAttributeSize>,
) -> Result<u64, LogicalSizeError> {
    let mut size = body_bytes;
    for attribute in attributes {
        size = size
            .checked_add(attribute.key_bytes)
            .and_then(|size| size.checked_add(attribute.value_bytes))
            .ok_or(LogicalSizeError::Overflow)?;
    }
    size.checked_add(FIXED_PER_RECORD_OVERHEAD_BYTES)
        .ok_or(LogicalSizeError::Overflow)
}

/// Computes the conservative configuration-time bound for one record.
///
/// Provenance reserves the larger of the lossless registered text pair and
/// the bounded native fallback. Fragment ranges, the longest flush reason,
/// terminal evidence, and policy-specific decode evidence are included.
pub(crate) fn configured_logical_record_size(
    body_bytes: u64,
    oversize_behavior: MaxLogSizeBehavior,
    decode_policy: OnDecodeError,
) -> Result<u64, LogicalSizeError> {
    const MAX_COMMON_ATTRIBUTES: usize = 11;
    const EMPTY: LogicalAttributeSize = LogicalAttributeSize {
        key_bytes: 0,
        value_bytes: 0,
    };

    let mut attributes = [EMPTY; MAX_COMMON_ATTRIBUTES];
    let mut count = 0usize;
    let mut push = |key: &str, value_bytes: u64| -> Result<(), LogicalSizeError> {
        let slot = attributes
            .get_mut(count)
            .ok_or(LogicalSizeError::Overflow)?;
        *slot = LogicalAttributeSize::new(key, value_bytes)?;
        count = count.checked_add(1).ok_or(LogicalSizeError::Overflow)?;
        Ok(())
    };

    match oversize_behavior {
        MaxLogSizeBehavior::Split => {
            push(ATTR_KEY_FRAGMENT_ID, FRAGMENT_ID_VALUE_BYTES)?;
            push(
                ATTR_KEY_FRAGMENT_INDEX,
                logical_int_value_len(i64::from(u32::MAX)),
            )?;
            push(ATTR_KEY_FRAGMENT_IS_LAST, logical_bool_value_len(false))?;
            push(
                ATTR_KEY_FRAGMENT_BODY_START,
                logical_int_value_len(i64::MAX),
            )?;
            push(ATTR_KEY_FRAGMENT_BODY_END, logical_int_value_len(i64::MAX))?;
            push(
                ATTR_KEY_FRAGMENT_FRAME_START,
                logical_int_value_len(i64::MAX),
            )?;
            push(ATTR_KEY_FRAGMENT_FRAME_END, logical_int_value_len(i64::MAX))?;
        }
        MaxLogSizeBehavior::Truncate => {
            push(ATTR_KEY_RECORD_TRUNCATED, logical_bool_value_len(true))?;
        }
    }
    push(ATTR_KEY_FLUSH_REASON, "oversize_line_boundary".len() as u64)?;
    push(ATTR_KEY_TERMINAL_UNTERMINATED, logical_bool_value_len(true))?;
    match decode_policy {
        OnDecodeError::PreserveRaw => {
            push(ATTR_KEY_DECODE_ERROR_POLICY, "preserve_raw".len() as u64)?;
            push(
                ATTR_KEY_DECODE_ERROR_COUNT,
                RESERVED_DECIMAL_U64_VALUE_BYTES,
            )?;
        }
        OnDecodeError::Replace => {
            push(ATTR_KEY_DECODE_ERROR_POLICY, "replace".len() as u64)?;
            push(
                ATTR_KEY_DECODE_ERROR_COUNT,
                RESERVED_DECIMAL_U64_VALUE_BYTES,
            )?;
        }
        OnDecodeError::Fail => {}
    }

    let registered = [
        LogicalAttributeSize::new(ATTR_KEY_LOG_FILE_PATH, MAX_REGISTERED_PATH_TEXT_VALUE_BYTES)?,
        LogicalAttributeSize::new(ATTR_KEY_LOG_FILE_NAME, MAX_REGISTERED_PATH_TEXT_VALUE_BYTES)?,
    ];
    let native = [
        LogicalAttributeSize::new(ATTR_KEY_PATH_KIND, MAX_PATH_KIND_VALUE_BYTES)?,
        LogicalAttributeSize::new(ATTR_KEY_PATH_NATIVE, MAX_NATIVE_PATH_ATTRIBUTE_VALUE_BYTES)?,
        LogicalAttributeSize::new(ATTR_KEY_PATH_TRUNCATED, logical_bool_value_len(true))?,
        LogicalAttributeSize::new(ATTR_KEY_PATH_SHA256, SHA256_HEX_VALUE_BYTES)?,
    ];
    let common = attributes[..count].iter().copied();
    let registered_size =
        checked_logical_record_size(body_bytes, common.clone().chain(registered))?;
    let native_size = checked_logical_record_size(body_bytes, common.chain(native))?;
    Ok(registered_size.max(native_size))
}

/// Receiver-owned descriptor admission evidence captured during config
/// validation before any source or checkpoint handle is opened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DescriptorBudget {
    pub(crate) owned: u64,
    pub(crate) soft_limit: Option<u64>,
    pub(crate) warning: bool,
}

/// One numerically modeled resource-admission term.
///
/// `named_provisional_ceiling_bytes` is `Some` only where the authoritative
/// conformance design defines a startup rejection threshold. A `None` value
/// means the term is still checked for representability but is not compared
/// with an invented ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceAdmissionTerm {
    pub(crate) bytes: u64,
    pub(crate) named_provisional_ceiling_bytes: Option<u64>,
}

/// Integrated resource-admission evidence retained by the validated config.
///
/// The numeric values are conservative modeled subtotals, not exact heap or
/// RSS measurements. Recovery and steady runtime are separate phases so the
/// checkpoint recovery peak is not double-counted with state allocated only
/// after recovery. Terms requiring representative measurement remain named
/// by [`Self::unmeasured_terms`] and are intentionally absent from the numeric
/// subtotals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceAdmissionReport {
    pub(crate) candidate_identity_state: ResourceAdmissionTerm,
    pub(crate) reader_state: ResourceAdmissionTerm,
    pub(crate) framer_payload_per_reader: ResourceAdmissionTerm,
    pub(crate) framer_payload: ResourceAdmissionTerm,
    pub(crate) retained_batch: ResourceAdmissionTerm,
    pub(crate) carry_over: ResourceAdmissionTerm,
    pub(crate) checkpoint_recovery: ResourceAdmissionTerm,
    pub(crate) regex_program_cache: ResourceAdmissionTerm,
    pub(crate) checkpoint_limits: StoreLimits,
    pub(crate) numeric_recovery_subtotal_bytes: u64,
    pub(crate) numeric_runtime_subtotal_bytes: u64,
    pub(crate) numeric_peak_subtotal_bytes: u64,
}

impl ResourceAdmissionReport {
    /// Bounded terms that still require implementation-specific measurement
    /// before a complete RSS ceiling can be claimed.
    pub(crate) const fn unmeasured_terms(&self) -> &'static [&'static str] {
        &RESOURCE_ADMISSION_UNMEASURED_TERMS
    }

    /// Stable bounded rendering of [`Self::unmeasured_terms`] for startup
    /// diagnostics.
    pub(crate) const fn unmeasured_terms_text(&self) -> &'static str {
        RESOURCE_ADMISSION_UNMEASURED_TERMS_TEXT
    }

    /// No reviewed universal RSS ceiling exists for the complete aggregate.
    pub(crate) const fn complete_rss_ceiling_bytes(&self) -> Option<u64> {
        None
    }
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
    /// Admitted-reader scheduling cadence.
    pub(crate) reader: ReaderConfig,
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
    /// Bounded discovery, admission, and read populations.
    pub(crate) limits: LimitsConfig,
    /// Receiver-local descriptor budget and process soft-limit comparison.
    pub(crate) descriptor_budget: DescriptorBudget,
    /// Checked integrated resource-admission evidence.
    pub(crate) resource_admission: ResourceAdmissionReport,
    /// Worker -> async batch shaping.
    pub(crate) batch: BatchConfig,
    /// Move/create and copy-truncate rotation handling.
    pub(crate) rotation: RotationConfig,
    /// Durable checkpoint store configuration as configured (its `id` is
    /// superseded by the resolved [`Self::checkpoint_id`]).
    pub(crate) checkpoint: CheckpointConfig,
    /// Resolved stable checkpoint identifier: the configured
    /// `checkpoint.id`, or the caller-supplied placement-derived default when
    /// omitted.
    pub(crate) checkpoint_id: String,
    /// Resolved checkpoint namespace directory:
    /// `${engine.state_dir}/filelog/@v1/<lowercase-hex checkpoint_id>/`.
    pub(crate) checkpoint_namespace_dir: PathBuf,
    /// Retry budget for a Nacked, `NoRoute`, or resent batch.
    pub(crate) retry: RetryConfig,
    /// Behavior applied once the retry budget is exhausted.
    pub(crate) on_nack: OnNack,
    /// Drain deadline budget.
    pub(crate) drain_timeout: Duration,
    /// SHA-256 framing-profile digest for the configured identity and framing
    /// contract (identity evidence, encoding, decode-error policy, multiline
    /// mode, size bounds, oversize policy, multiline line cap, and idle flush
    /// period). Persisted checkpoint records compare this to detect an
    /// incompatible framing-profile change across restart.
    pub(crate) framing_profile_digest: [u8; 32],
    /// The compiled multiline boundary pattern, when one is configured.
    /// Compiled once here so the runtime never recompiles or re-validates
    /// it.
    pub(crate) compiled_multiline_pattern: Option<CompiledMultilinePattern>,
}

/// A multiline boundary pattern compiled for the configured framing
/// semantics.
#[derive(Clone, Debug)]
pub(crate) enum CompiledMultilinePattern {
    /// A pattern matching decoded UTF-8 text.
    Text(Regex),
    /// A pattern matching raw source bytes.
    Raw(regex::bytes::Regex),
}

impl CompiledMultilinePattern {
    /// Matches a decoded physical line without performing a lossy
    /// conversion.
    ///
    /// Raw patterns accept every byte sequence. Text patterns return the
    /// original UTF-8 validation error if a caller violates the decoded-text
    /// invariant.
    pub(crate) fn is_match(&self, line: &[u8]) -> Result<bool, std::str::Utf8Error> {
        match self {
            Self::Text(pattern) => {
                let text = std::str::from_utf8(line)?;
                Ok(pattern.is_match(text))
            }
            Self::Raw(pattern) => Ok(pattern.is_match(line)),
        }
    }
}

impl RuntimeConfig {
    /// Validates `config` and resolves it into a runtime-ready form.
    ///
    /// `default_checkpoint_id` is used only when the user config omits
    /// `checkpoint.id`; the factory supplies its stable placement-derived
    /// digest. An empty default requires the user config to set
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
            reader,
            ignore_older_than,
            identity,
            encoding,
            on_decode_error,
            framing,
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
        let reader = validate_reader(reader)?;
        if drain_timeout.is_zero() {
            return Err(invalid("drain_timeout must be greater than zero"));
        }
        let limits = validate_limits(limits)?;
        let (
            framing,
            framing_profile_digest,
            compiled_multiline_pattern,
            framer_payload_per_reader_bytes,
        ) = validate_framing(framing, encoding, on_decode_error, &identity)?;
        let batch = validate_batch(batch, &framing, on_decode_error)?;
        let rotation = validate_rotation(rotation)?;
        let retry = validate_retry(retry)?;

        let checkpoint_id = resolve_checkpoint_id(checkpoint.id.as_deref(), default_checkpoint_id)?;
        let checkpoint_namespace_dir = checkpoint_namespace_dir(&checkpoint_id);
        let checkpoint_limits = validate_checkpoint_bounds(&checkpoint, &limits, &identity)?;
        let resource_admission = validate_resource_admission(
            &identity,
            &limits,
            &batch,
            checkpoint_limits,
            framer_payload_per_reader_bytes,
            compiled_multiline_pattern.is_some(),
        )?;
        let descriptor_budget = validate_descriptor_budget(limits.max_open_files)?;

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
            reader,
            ignore_older_than,
            identity,
            encoding,
            on_decode_error,
            framing,
            limits,
            descriptor_budget,
            resource_admission,
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
    /// [`RuntimeConfig::from_config`] when factory wiring can supply the
    /// placement-derived default.
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

fn validate_discovery(
    discovery: DiscoveryConfig,
) -> Result<DiscoveryConfig, otap_df_config::error::Error> {
    if !(MIN_RECONCILE_INTERVAL..=MAX_RECONCILE_INTERVAL).contains(&discovery.reconcile_interval) {
        return Err(invalid(
            "discovery.reconcile_interval must be in 100ms..=24h",
        ));
    }
    if discovery.reconcile_jitter_percent > MAX_RECONCILE_JITTER_PERCENT {
        return Err(invalid(
            "discovery.reconcile_jitter_percent must be in 0..=25",
        ));
    }
    let Some((_, maximum_delay_ns)) = reconciliation_delay_bounds_ns(
        discovery.reconcile_interval,
        discovery.reconcile_jitter_percent,
    ) else {
        return Err(invalid(
            "discovery reconciliation jitter arithmetic overflows u64 nanoseconds",
        ));
    };
    if Instant::now()
        .checked_add(Duration::from_nanos(maximum_delay_ns))
        .is_none()
    {
        return Err(invalid(
            "discovery reconciliation deadline exceeds the host clock domain",
        ));
    }
    Ok(discovery)
}

fn validate_reader(reader: ReaderConfig) -> Result<ReaderConfig, otap_df_config::error::Error> {
    if !(MIN_EOF_REPROBE_INTERVAL..=MAX_EOF_REPROBE_INTERVAL).contains(&reader.eof_reprobe_interval)
    {
        return Err(invalid("reader.eof_reprobe_interval must be in 10ms..=1h"));
    }
    if u64::try_from(reader.eof_reprobe_interval.as_nanos()).is_err()
        || Instant::now()
            .checked_add(reader.eof_reprobe_interval)
            .is_none()
    {
        return Err(invalid(
            "reader.eof_reprobe_interval exceeds the host clock domain",
        ));
    }
    Ok(reader)
}

pub(crate) fn reconciliation_delay_bounds_ns(
    interval: Duration,
    jitter_percent: u8,
) -> Option<(u64, u64)> {
    let base_ns = u64::try_from(interval.as_nanos()).ok()?;
    let spread_ns = base_ns
        .checked_mul(u64::from(jitter_percent))?
        .checked_div(100)?;
    Some((
        base_ns.checked_sub(spread_ns)?,
        base_ns.checked_add(spread_ns)?,
    ))
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

fn validate_descriptor_budget(
    max_open_files: u32,
) -> Result<DescriptorBudget, otap_df_config::error::Error> {
    let soft_limit = process_descriptor_soft_limit().map_err(|source| {
        invalid(&format!(
            "could not inspect the process descriptor soft limit: {source}"
        ))
    })?;
    descriptor_budget_against_soft_limit(max_open_files, soft_limit)
        .map_err(|message| invalid(&message))
}

fn descriptor_budget_against_soft_limit(
    max_open_files: u32,
    soft_limit: Option<u64>,
) -> Result<DescriptorBudget, String> {
    let owned = u64::from(max_open_files)
        .checked_add(DISCOVERY_TRAVERSAL_DESCRIPTOR_BUDGET)
        .and_then(|value| value.checked_add(TRANSIENT_PROBE_DESCRIPTOR_BUDGET))
        .and_then(|value| value.checked_add(CHECKPOINT_DESCRIPTOR_BUDGET))
        .ok_or_else(|| "filelog receiver descriptor budget overflows u64".to_owned())?;
    let warning = match soft_limit {
        Some(limit) if owned > limit => {
            return Err(format!(
                "filelog receiver descriptor budget {owned} exceeds process soft limit {limit} \
                 (limits.max_open_files={max_open_files}, traversal=1, transient_probe=1, \
                 checkpoint=8)"
            ));
        }
        Some(limit) => u128::from(owned) * 100 > u128::from(limit) * 80,
        None => false,
    };
    Ok(DescriptorBudget {
        owned,
        soft_limit,
        warning,
    })
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn process_descriptor_soft_limit() -> std::io::Result<Option<u64>> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid writable `rlimit` and remains alive for the
    // duration of the call. `getrlimit` does not retain the pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if limit.rlim_cur == libc::RLIM_INFINITY {
        Ok(None)
    } else {
        Ok(Some(limit.rlim_cur))
    }
}

#[cfg(not(unix))]
fn process_descriptor_soft_limit() -> std::io::Result<Option<u64>> {
    Ok(None)
}

fn identity_reconciliation_bytes(
    identity: &IdentityConfig,
    limits: &LimitsConfig,
) -> Result<u64, otap_df_config::error::Error> {
    let candidate_population = u64::from(limits.max_pending_candidates)
        .checked_add(u64::from(limits.max_open_files))
        .ok_or_else(|| invalid("identity reconciliation candidate population overflows u64"))?;
    let candidate_bytes = identity
        .fingerprint_bytes
        .checked_mul(DISCOVERY_CANDIDATE_FINGERPRINT_COPIES)
        .and_then(|bytes| {
            ADVISORY_PATH_MODELED_BYTES
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
            ADVISORY_PATH_MODELED_BYTES
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
        .checked_add(ADVISORY_PATH_MODELED_BYTES)
        .and_then(|bytes| bytes.checked_add(COMMITTED_FRONTIER_GUARD_LEN as u64))
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
    Ok(total)
}

fn reader_table_payload_bytes(
    identity: &IdentityConfig,
    limits: &LimitsConfig,
) -> Result<u64, otap_df_config::error::Error> {
    let per_reader = ADVISORY_PATH_MODELED_BYTES
        .checked_mul(2)
        .and_then(|paths| identity.fingerprint_bytes.checked_add(paths))
        .and_then(|bytes| bytes.checked_add(1024))
        .ok_or_else(|| {
            invalid(
                "reader-table per-reader formula overflows u64; reduce \
                 identity.fingerprint_bytes",
            )
        })?;
    u64::from(limits.max_tracked_files)
        .checked_mul(per_reader)
        .and_then(|bytes| bytes.checked_add(limits.max_read_bytes_per_turn))
        .ok_or_else(|| {
            invalid(
                "reader-table payload formula overflows u64; reduce \
                 limits.max_tracked_files, limits.max_read_bytes_per_turn, or \
                 identity.fingerprint_bytes",
            )
        })
}

fn checked_resource_sum(
    formula: &'static str,
    remedies: &'static str,
    parts: &[u64],
) -> Result<u64, otap_df_config::error::Error> {
    let mut total = 0u64;
    for part in parts {
        total = total
            .checked_add(*part)
            .ok_or_else(|| invalid(&format!("{formula} overflows u64; reduce {remedies}")))?;
    }
    Ok(total)
}

fn progress_transaction_bytes(
    batch: &BatchConfig,
    limits: &LimitsConfig,
) -> Result<u64, otap_df_config::error::Error> {
    let operation_count = u64::from(
        batch
            .max_records
            .min(limits.max_tracked_files)
            .min(u32::from(WAL_MAX_OPS_PER_TX)),
    );
    let operations = operation_count
        .checked_mul(UPDATE_PROGRESS_MAX_OP_FRAME_BYTES)
        .ok_or_else(|| {
            invalid(
                "retained-batch progress transaction formula overflows u64; reduce \
                 batch.max_records or limits.max_tracked_files",
            )
        })?;
    let bytes = checked_resource_sum(
        "retained-batch progress transaction formula",
        "batch.max_records or limits.max_tracked_files",
        &[
            TX_HEADER_BYTES as u64,
            operations,
            TX_FRAME_CRC_BYTES as u64,
        ],
    )?;
    debug_assert!(bytes <= MAX_PROGRESS_TX_FRAME_BYTES);
    Ok(bytes)
}

fn validate_resource_admission(
    identity: &IdentityConfig,
    limits: &LimitsConfig,
    batch: &BatchConfig,
    checkpoint_limits: StoreLimits,
    framer_payload_per_reader_bytes: u64,
    multiline_configured: bool,
) -> Result<ResourceAdmissionReport, otap_df_config::error::Error> {
    let candidate_identity_bytes = identity_reconciliation_bytes(identity, limits)?;
    let reader_state_bytes = reader_table_payload_bytes(identity, limits)?;
    // A framer is retained only for a resident reader. The worker discards
    // speculative framer state before confirming descriptor eviction, and a
    // carry-over's post-frame framer occupies one of the same resident slots.
    let framer_payload_bytes = framer_payload_per_reader_bytes
        .checked_mul(u64::from(limits.max_open_files))
        .ok_or_else(|| {
            invalid(
                "aggregate framer payload formula overflows u64; reduce \
                 limits.max_open_files, framing.max_line_bytes, or \
                 framing.max_record_bytes",
            )
        })?;
    let progress_transaction_bytes = progress_transaction_bytes(batch, limits)?;
    let retained_batch_bytes = batch
        .max_bytes
        .checked_add(progress_transaction_bytes)
        .ok_or_else(|| {
            invalid(
                "retained-batch resource formula overflows u64; reduce batch.max_bytes, \
                 batch.max_records, or limits.max_tracked_files",
            )
        })?;
    // Exact projected body and attributes are bounded by batch.max_bytes. One
    // progress-operation frame covers its sole delta. Its post-frame Framer
    // is already included in the resident-framer population above.
    let carry_over_bytes = batch
        .max_bytes
        .checked_add(UPDATE_PROGRESS_MAX_OP_FRAME_BYTES)
        .ok_or_else(|| {
            invalid("carry-over resource formula overflows u64; reduce batch.max_bytes")
        })?;
    let regex_program_cache_bytes = if multiline_configured {
        u64::try_from(MULTILINE_REGEX_SIZE_LIMIT_BYTES)
            .ok()
            .and_then(|program| {
                u64::try_from(MULTILINE_REGEX_DFA_SIZE_LIMIT_BYTES)
                    .ok()
                    .and_then(|cache| program.checked_add(cache))
            })
            .ok_or_else(|| {
                invalid(
                    "multiline regex program/cache resource formula overflows u64; remove or \
                     simplify framing.multiline.line_start_pattern or \
                     framing.multiline.line_end_pattern",
                )
            })?
    } else {
        0
    };

    // Recovery happens before reader, discovery, framer, batch, lease, and
    // worker tables are constructed, so its numeric phase is compared with
    // rather than added to the steady-runtime subtotal. The compiled regex
    // remains resident in both phases.
    let numeric_recovery_subtotal_bytes = checked_resource_sum(
        "resource-admission recovery numeric subtotal",
        "checkpoint.compact_after_bytes, checkpoint.compact_after_transactions, \
         limits.max_tracked_files, identity.fingerprint_bytes, or the multiline pattern",
        &[
            checkpoint_limits.max_recovery_working_bytes,
            regex_program_cache_bytes,
        ],
    )?;
    let numeric_runtime_subtotal_bytes = checked_resource_sum(
        "resource-admission runtime numeric subtotal",
        "limits.max_pending_candidates, limits.max_open_files, limits.max_tracked_files, \
         limits.max_read_bytes_per_turn, identity.fingerprint_bytes, \
         framing.max_line_bytes, framing.max_record_bytes, batch.max_bytes, \
         batch.max_records, or the multiline pattern",
        &[
            candidate_identity_bytes,
            reader_state_bytes,
            framer_payload_bytes,
            retained_batch_bytes,
            carry_over_bytes,
            regex_program_cache_bytes,
        ],
    )?;

    let modeled = |bytes| ResourceAdmissionTerm {
        bytes,
        named_provisional_ceiling_bytes: None,
    };
    Ok(ResourceAdmissionReport {
        candidate_identity_state: ResourceAdmissionTerm {
            bytes: candidate_identity_bytes,
            named_provisional_ceiling_bytes: Some(IDENTITY_RECONCILIATION_BYTES_CEILING),
        },
        reader_state: modeled(reader_state_bytes),
        framer_payload_per_reader: modeled(framer_payload_per_reader_bytes),
        framer_payload: modeled(framer_payload_bytes),
        retained_batch: modeled(retained_batch_bytes),
        carry_over: modeled(carry_over_bytes),
        checkpoint_recovery: ResourceAdmissionTerm {
            bytes: checkpoint_limits.max_recovery_working_bytes,
            named_provisional_ceiling_bytes: Some(RECOVERY_WORKING_BYTES_CEILING),
        },
        regex_program_cache: modeled(regex_program_cache_bytes),
        checkpoint_limits,
        numeric_recovery_subtotal_bytes,
        numeric_runtime_subtotal_bytes,
        numeric_peak_subtotal_bytes: numeric_recovery_subtotal_bytes
            .max(numeric_runtime_subtotal_bytes),
    })
}

fn invalid_re2(field: &str, detail: &str) -> otap_df_config::error::Error {
    invalid(&format!(
        "{field} is not a valid re2-v1 regular expression: {detail}"
    ))
}

fn append_re2_escape(
    output: &mut Vec<u8>,
    field: &str,
    escaped: u8,
    in_character_class: bool,
) -> Result<(), otap_df_config::error::Error> {
    let replacement: &[u8] = match (escaped, in_character_class) {
        (b'd', false) => br"[0-9]",
        (b'D', false) => br"[^0-9]",
        (b's', false) => br"[\t\n\f\r ]",
        (b'S', false) => br"[^\t\n\f\r ]",
        (b'w', false) => br"[0-9A-Za-z_]",
        (b'W', false) => br"[^0-9A-Za-z_]",
        (b'b', false) => br"(?-u:\b)",
        (b'B', false) => br"(?-u:\B)",
        (b'd', true) => br"0-9",
        (b'D', true) => br"[^0-9]",
        (b's', true) => br"\t\n\f\r ",
        (b'S', true) => br"[^\t\n\f\r ]",
        (b'w', true) => br"0-9A-Za-z_",
        (b'W', true) => br"[^0-9A-Za-z_]",
        (b'b' | b'B', true) => {
            return Err(invalid_re2(
                field,
                "word-boundary escapes are not supported inside character classes",
            ));
        }
        (b'p' | b'P', _) => {
            return Err(invalid_re2(
                field,
                "Unicode property escapes are not supported by the re2-v1 subset",
            ));
        }
        (b'u' | b'U', _) => {
            return Err(invalid_re2(
                field,
                "Rust Unicode escapes are not supported; use an RE2 hex escape or a literal",
            ));
        }
        (punctuation, _) if punctuation.is_ascii_punctuation() => {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            output.extend_from_slice(br"\x");
            output.push(HEX[usize::from(punctuation >> 4)]);
            output.push(HEX[usize::from(punctuation & 0x0f)]);
            return Ok(());
        }
        (letter, in_class) if letter.is_ascii_alphabetic() => {
            let supported = matches!(letter, b'a' | b'f' | b'n' | b'r' | b't' | b'v' | b'x')
                || (!in_class && matches!(letter, b'A' | b'z'));
            if !supported {
                return Err(invalid_re2(
                    field,
                    "escape is not supported by the re2-v1 executable subset",
                ));
            }
            output.push(b'\\');
            output.push(letter);
            return Ok(());
        }
        (digit, _) if digit.is_ascii_digit() => {
            output.push(b'\\');
            output.push(digit);
            return Ok(());
        }
        _ => {
            return Err(invalid_re2(
                field,
                "escape is not supported by the re2-v1 executable subset",
            ));
        }
    };
    output.extend_from_slice(replacement);
    Ok(())
}

fn validate_re2_inline_flags(
    field: &str,
    bytes: &[u8],
    group_start: usize,
) -> Result<(), otap_df_config::error::Error> {
    let mut index = group_start + 2;
    while let Some(flag) = bytes.get(index) {
        match flag {
            b'i' | b'm' | b's' | b'U' | b'-' => index += 1,
            b'u' | b'R' | b'x' => {
                return Err(invalid_re2(
                    field,
                    "only RE2 inline flags i, m, s, and U are supported",
                ));
            }
            _ => break,
        }
    }
    Ok(())
}

fn parse_repetition_bound(bytes: &[u8], index: &mut usize) -> (bool, bool) {
    let start = *index;
    let mut value = 0u16;
    let mut exceeds_limit = false;
    while let Some(digit) = bytes.get(*index).and_then(|byte| byte.checked_sub(b'0')) {
        if digit > 9 {
            break;
        }
        if !exceeds_limit {
            value = value.saturating_mul(10).saturating_add(u16::from(digit));
            exceeds_limit = value > RE2_MAX_COUNTED_REPETITION;
        }
        *index += 1;
    }
    (*index != start, exceeds_limit)
}

fn validate_re2_counted_repetition(
    field: &str,
    bytes: &[u8],
    opening_brace: usize,
) -> Result<(), otap_df_config::error::Error> {
    let mut index = opening_brace + 1;
    let (has_minimum, minimum_exceeds) = parse_repetition_bound(bytes, &mut index);
    if !has_minimum {
        if bytes.get(index) == Some(&b',') {
            index += 1;
            let (has_maximum, _) = parse_repetition_bound(bytes, &mut index);
            if has_maximum && bytes.get(index) == Some(&b'}') {
                return Err(invalid_re2(
                    field,
                    "counted repetitions require a minimum bound",
                ));
            }
        }
        return Ok(());
    }

    let mut maximum_exceeds = false;
    let valid = match bytes.get(index) {
        Some(b'}') => true,
        Some(b',') => {
            index += 1;
            let (_, exceeds) = parse_repetition_bound(bytes, &mut index);
            maximum_exceeds = exceeds;
            bytes.get(index) == Some(&b'}')
        }
        _ => false,
    };
    if valid && (minimum_exceeds || maximum_exceeds) {
        return Err(invalid_re2(
            field,
            "counted repetition bounds must not exceed 1000",
        ));
    }
    Ok(())
}

/// Validates the lexical differences between RE2 and Rust `regex`, and
/// rewrites RE2's ASCII-only Perl classes and word boundaries into equivalent
/// Rust syntax. The regex compiler remains authoritative for the shared
/// grammar and rejects unsupported RE2 constructs that Rust cannot execute.
fn normalize_re2_pattern(
    field: &str,
    pattern: &str,
) -> Result<String, otap_df_config::error::Error> {
    if pattern.is_empty() {
        return Err(invalid(&format!("{field} must not be empty when set")));
    }
    if pattern.len() > FRAMING_PATTERN_MAX_BYTES {
        return Err(invalid(&format!(
            "{field} is {} bytes, exceeding the {FRAMING_PATTERN_MAX_BYTES}-byte maximum",
            pattern.len()
        )));
    }

    let bytes = pattern.as_bytes();
    let mut output = Vec::with_capacity(pattern.len());
    let mut index = 0;
    let mut character_class_phase = None::<u8>;

    while index < bytes.len() {
        if let Some(phase) = character_class_phase {
            match bytes[index] {
                b'\\' => {
                    let Some(escaped) = bytes.get(index + 1).copied() else {
                        output.push(b'\\');
                        index += 1;
                        continue;
                    };
                    append_re2_escape(&mut output, field, escaped, true)?;
                    character_class_phase = Some(2);
                    index += 2;
                    continue;
                }
                b'[' if bytes.get(index + 1).is_some_and(|byte| *byte == b':') => {
                    let mut end = index + 2;
                    while end + 1 < bytes.len() && !(bytes[end] == b':' && bytes[end + 1] == b']') {
                        end += 1;
                    }
                    if end + 1 >= bytes.len() {
                        return Err(invalid_re2(field, "unterminated POSIX character class"));
                    }
                    output.extend_from_slice(&bytes[index..end + 2]);
                    character_class_phase = Some(2);
                    index = end + 2;
                    continue;
                }
                b'[' => {
                    return Err(invalid_re2(
                        field,
                        "nested character classes are not supported by the re2-v1 subset",
                    ));
                }
                b'^' if phase == 0 => {
                    output.push(b'^');
                    character_class_phase = Some(1);
                    index += 1;
                    continue;
                }
                b']' if phase < 2 => {
                    output.push(b']');
                    character_class_phase = Some(2);
                    index += 1;
                    continue;
                }
                b']' => {
                    output.push(b']');
                    character_class_phase = None;
                    index += 1;
                    continue;
                }
                b'&' | b'-' | b'~'
                    if bytes
                        .get(index + 1)
                        .is_some_and(|next| *next == bytes[index]) =>
                {
                    return Err(invalid_re2(
                        field,
                        "Rust character-class set operators are not supported",
                    ));
                }
                _ => {
                    output.push(bytes[index]);
                    character_class_phase = Some(2);
                    index += 1;
                    continue;
                }
            }
        }

        match bytes[index] {
            b'\\' => {
                let Some(escaped) = bytes.get(index + 1).copied() else {
                    output.push(b'\\');
                    index += 1;
                    continue;
                };
                append_re2_escape(&mut output, field, escaped, false)?;
                index += 2;
                continue;
            }
            b'[' => {
                output.push(b'[');
                character_class_phase = Some(0);
            }
            b'(' if bytes.get(index + 1).is_some_and(|byte| *byte == b'?') => {
                validate_re2_inline_flags(field, bytes, index)?;
                output.push(b'(');
            }
            b'{' => {
                validate_re2_counted_repetition(field, bytes, index)?;
                output.push(b'{');
            }
            _ => output.push(bytes[index]),
        }
        index += 1;
    }

    String::from_utf8(output).map_err(|_| {
        invalid_re2(
            field,
            "internal normalization did not preserve UTF-8 pattern source",
        )
    })
}

/// Validates and normalizes the `re2-v1` subset, then returns the matcher
/// compiled for the configured encoding.
fn compile_re2_pattern(
    field: &str,
    pattern: &str,
    encoding: Encoding,
) -> Result<CompiledMultilinePattern, otap_df_config::error::Error> {
    let normalized = normalize_re2_pattern(field, pattern)?;
    let compiled = match encoding {
        Encoding::Raw => regex::bytes::RegexBuilder::new(&normalized)
            .unicode(false)
            .size_limit(MULTILINE_REGEX_SIZE_LIMIT_BYTES)
            .dfa_size_limit(MULTILINE_REGEX_DFA_SIZE_LIMIT_BYTES)
            .build()
            .map(CompiledMultilinePattern::Raw),
        Encoding::Utf8 | Encoding::Ascii | Encoding::Utf16Le | Encoding::Utf16Be => {
            regex::RegexBuilder::new(&normalized)
                .size_limit(MULTILINE_REGEX_SIZE_LIMIT_BYTES)
                .dfa_size_limit(MULTILINE_REGEX_DFA_SIZE_LIMIT_BYTES)
                .build()
                .map(CompiledMultilinePattern::Text)
        }
    };
    compiled.map_err(|source| {
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

const fn minimum_framing_bound(encoding: Encoding, on_decode_error: OnDecodeError) -> u64 {
    match (encoding, on_decode_error) {
        (Encoding::Raw, _) | (Encoding::Ascii, OnDecodeError::Fail) => 1,
        (Encoding::Ascii, OnDecodeError::PreserveRaw | OnDecodeError::Replace) => 3,
        (
            Encoding::Utf8 | Encoding::Utf16Le | Encoding::Utf16Be,
            OnDecodeError::PreserveRaw | OnDecodeError::Replace | OnDecodeError::Fail,
        ) => 4,
    }
}

const fn framer_payload_copy_count(encoding: Encoding, on_decode_error: OnDecodeError) -> usize {
    if !matches!(encoding, Encoding::Raw) && matches!(on_decode_error, OnDecodeError::PreserveRaw) {
        2
    } else {
        1
    }
}

/// Computes the conservative peak retained framer payload:
///
/// `4 * copies * (min(max_line_bytes, max_record_bytes) + max_record_bytes)
///  + 16 * copies + 16`.
///
/// Returns `None` if any operation is not representable as `usize`.
pub(crate) fn peak_framer_payload_bytes(
    max_line_bytes: usize,
    max_record_bytes: usize,
    copies: usize,
) -> Option<usize> {
    let retained = max_line_bytes
        .min(max_record_bytes)
        .checked_add(max_record_bytes)?;
    4usize
        .checked_mul(copies)?
        .checked_mul(retained)?
        .checked_add(16usize.checked_mul(copies)?)?
        .checked_add(16)
}

fn validate_peak_framer_payload_bytes(
    framing: &FramingConfig,
    encoding: Encoding,
    on_decode_error: OnDecodeError,
) -> Result<u64, otap_df_config::error::Error> {
    let overflow = || {
        invalid(&format!(
            "framing bounds max_line_bytes={} and max_record_bytes={} overflow usize in the \
             conservative peak framer payload formula; reduce framing.max_line_bytes or \
             framing.max_record_bytes",
            framing.max_line_bytes, framing.max_record_bytes
        ))
    };
    let max_line_bytes = usize::try_from(framing.max_line_bytes).map_err(|_| overflow())?;
    let max_record_bytes = usize::try_from(framing.max_record_bytes).map_err(|_| overflow())?;
    let copies = framer_payload_copy_count(encoding, on_decode_error);
    let bytes =
        peak_framer_payload_bytes(max_line_bytes, max_record_bytes, copies).ok_or_else(overflow)?;
    u64::try_from(bytes).map_err(|_| overflow())
}

fn validate_framing(
    framing: FramingConfig,
    encoding: Encoding,
    on_decode_error: OnDecodeError,
    identity: &IdentityConfig,
) -> Result<
    (
        FramingConfig,
        [u8; 32],
        Option<CompiledMultilinePattern>,
        u64,
    ),
    otap_df_config::error::Error,
> {
    if framing.max_line_bytes == 0 {
        return Err(invalid("framing.max_line_bytes must be greater than zero"));
    }
    if framing.max_record_bytes == 0 {
        return Err(invalid(
            "framing.max_record_bytes must be greater than zero",
        ));
    }
    let minimum_bound = minimum_framing_bound(encoding, on_decode_error);
    if framing.max_line_bytes < minimum_bound {
        return Err(invalid(&format!(
            "framing.max_line_bytes must be at least {minimum_bound} for encoding {encoding:?} \
             with on_decode_error {on_decode_error:?}"
        )));
    }
    if framing.max_record_bytes < minimum_bound {
        return Err(invalid(&format!(
            "framing.max_record_bytes must be at least {minimum_bound} for encoding {encoding:?} \
             with on_decode_error {on_decode_error:?}"
        )));
    }
    if framing.max_multiline_lines == 0 {
        return Err(invalid(
            "framing.max_multiline_lines must be greater than zero",
        ));
    }
    let framer_payload_per_reader_bytes =
        validate_peak_framer_payload_bytes(&framing, encoding, on_decode_error)?;

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
            let compiled =
                compile_re2_pattern("framing.multiline.line_start_pattern", pattern, encoding)?;
            (
                framing_profile::MultilineMode::StartPattern {
                    regex_profile_version: framing.multiline.regex_profile.version(),
                    pattern: pattern.clone(),
                },
                Some(compiled),
            )
        }
        (None, Some(pattern)) => {
            let compiled =
                compile_re2_pattern("framing.multiline.line_end_pattern", pattern, encoding)?;
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
        on_decode_error: on_decode_error.to_framing_profile(),
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

    Ok((
        framing,
        digest,
        compiled_pattern,
        framer_payload_per_reader_bytes,
    ))
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
    decode_policy: OnDecodeError,
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
    // `configured_logical_record_size` uses checked arithmetic and reports overflow
    // rather than saturating, so a `body_bytes`/`batch.max_bytes` pair that
    // could only "validate" via saturation is rejected here instead.
    let line_bound = configured_logical_record_size(
        framing.max_line_bytes,
        framing.max_log_size_behavior,
        decode_policy,
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
    let record_bound = configured_logical_record_size(
        framing.max_record_bytes,
        framing.max_log_size_behavior,
        decode_policy,
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
) -> Result<StoreLimits, otap_df_config::error::Error> {
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
    if u64::try_from(checkpoint.retention.as_nanos()).is_err() {
        return Err(invalid(
            "checkpoint.retention must be zero or fit positive u64 nanoseconds",
        ));
    }
    // The durable checkpoint store derives its artifact and combined
    // recovery-working-set caps from exactly these four knobs, and enforces
    // the same artifact caps when it writes. Running the derivation here
    // rejects an unrecoverable configuration at build time, with the knobs
    // to reduce, rather than at the first compaction or reopen.
    StoreLimits::derive(
        checkpoint.compact_after_bytes,
        checkpoint.compact_after_transactions,
        limits.max_tracked_files,
        identity.fingerprint_bytes,
    )
    .map_err(|error| invalid(&error.to_string()))
}

fn resolve_checkpoint_id(
    configured: Option<&str>,
    default_checkpoint_id: &str,
) -> Result<String, otap_df_config::error::Error> {
    let id = configured.unwrap_or(default_checkpoint_id);
    CheckpointNamespace::validate_id(id).map_err(|error| match error {
        CheckpointNamespaceError::EmptyId => invalid(
            "checkpoint.id must not be empty; set checkpoint.id explicitly or ensure the \
             receiver node has a configured identity to default to",
        ),
        other => invalid(&other.to_string()),
    })?;
    Ok(id.to_owned())
}

/// Computes the stable Phase 1 checkpoint namespace directory for
/// `checkpoint_id`: `${engine.state_dir}/filelog/@v1/<lowercase hex>/`
/// (Appendix B, "Namespace layout"). The version directory makes this
/// namespace disjoint from the earlier flat percent-encoded draft. Every
/// byte of the exact id is encoded, so distinct ids remain distinct on
/// case-insensitive filesystems.
///
/// Normalizes away a leading `CurDir` (`.`) path component so a
/// `${engine.state_dir}` expansion that happens to start with `./` (for
/// example an `OTAP_DF_STATE_DIR` of `./state`) resolves to the exact same
/// [`PathBuf`] as the equivalent path without it; this keeps the namespace
/// side of the direct-include collision check
/// ([`include_targets_checkpoint_namespace`]) consistent with the same
/// normalization applied to include patterns via [`glob_literal_prefix`].
fn checkpoint_namespace_dir(checkpoint_id: &str) -> PathBuf {
    CheckpointNamespace::derive(checkpoint_state_dir(), checkpoint_id)
        .expect("a validated checkpoint id has a derivable namespace")
        .into_directory()
}

fn checkpoint_state_dir() -> PathBuf {
    std::env::var_os("OTAP_DF_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".otap-state"))
}

fn strip_leading_curdir(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    if matches!(components.peek(), Some(Component::CurDir)) {
        let _ = components.next();
    }
    components.collect()
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

pub(super) fn glob_pattern_has_meta(pattern: &str) -> bool {
    Path::new(pattern)
        .components()
        .any(|component| component_has_glob_meta(&component.as_os_str().to_string_lossy()))
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
    use crate::receivers::filelog_receiver::checkpoint::namespace::{
        CHECKPOINT_NAMESPACE_COMPONENT_MAX_BYTES, CHECKPOINT_NAMESPACE_ID_MAX_BYTES,
        CHECKPOINT_NAMESPACE_VERSION, FILELOG_NAMESPACE_DIRECTORY,
    };
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
        assert_eq!(cfg.discovery.reconcile_interval, Duration::from_secs(5));
        assert_eq!(cfg.discovery.reconcile_jitter_percent, 10);
        assert_eq!(cfg.reader.eof_reprobe_interval, Duration::from_millis(250));
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
        assert_eq!(runtime.descriptor_budget.owned, 522);
        assert!(
            runtime
                .descriptor_budget
                .soft_limit
                .is_none_or(|limit| limit >= runtime.descriptor_budget.owned)
        );
        assert!(runtime.compiled_multiline_pattern.is_none());
    }

    /// Scenario: an unreleased metadata object requests resolved path, generic
    /// record offset, or process-local record number attributes.
    /// Guarantees: the exact Phase 1 schema rejects the unsupported surface
    /// instead of silently emitting non-registry attributes.
    #[test]
    fn unsupported_metadata_surface_is_rejected() {
        let config = parse(serde_json::json!({
            "include": ["/var/log/app/*.log"],
            "metadata": {
                "include_file_path_resolved": true,
                "include_file_record_offset": true,
                "include_file_record_number": true
            }
        }));
        assert!(config.is_err());
    }

    /// Scenario: the Phase 1 project-owned provenance registry is inspected
    /// independently from record projection.
    /// Guarantees: every path/fragment key and native-kind value retains its
    /// exact frozen spelling; underscore-era experimental names cannot return
    /// through a coordinated producer/test rename.
    #[test]
    fn provenance_registry_spellings_are_frozen() {
        assert_eq!(ATTR_KEY_PATH_KIND, "otel.arrow.filelog.path.kind");
        assert_eq!(ATTR_KEY_PATH_NATIVE, "otel.arrow.filelog.path.native");
        assert_eq!(ATTR_KEY_PATH_TRUNCATED, "otel.arrow.filelog.path.truncated");
        assert_eq!(ATTR_KEY_PATH_SHA256, "otel.arrow.filelog.path.sha256");
        assert_eq!(ATTR_KEY_FRAGMENT_ID, "otel.arrow.filelog.fragment.id");
        assert_eq!(ATTR_KEY_FRAGMENT_INDEX, "otel.arrow.filelog.fragment.index");
        assert_eq!(
            ATTR_KEY_FRAGMENT_IS_LAST,
            "otel.arrow.filelog.fragment.is_last"
        );
        assert_eq!(
            ATTR_KEY_FRAGMENT_BODY_START,
            "otel.arrow.filelog.fragment.body.start"
        );
        assert_eq!(
            ATTR_KEY_FRAGMENT_BODY_END,
            "otel.arrow.filelog.fragment.body.end"
        );
        assert_eq!(
            ATTR_KEY_FRAGMENT_FRAME_START,
            "otel.arrow.filelog.fragment.frame.start"
        );
        assert_eq!(
            ATTR_KEY_FRAGMENT_FRAME_END,
            "otel.arrow.filelog.fragment.frame.end"
        );
        assert_eq!(
            ATTR_KEY_TERMINAL_UNTERMINATED,
            "otel.arrow.filelog.terminal_unterminated"
        );
        assert_eq!(PATH_KIND_UNIX_BYTES, "unix_bytes");
        assert_eq!(PATH_KIND_WINDOWS_UTF16LE, "windows_utf16le");
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

    /// Scenario: reconciliation and EOF intervals are exercised exactly at
    /// their inclusive bounds and one step outside, with jitter at 0, 25,
    /// and 26 percent.
    /// Guarantees: only `100ms..=24h`, `10ms..=1h`, and jitter `0..=25` are
    /// accepted before any worker or discovery thread starts.
    #[test]
    fn discovery_and_reader_cadence_bounds_are_exact() {
        let mut cfg = minimal_config();
        cfg.discovery.reconcile_interval = MIN_RECONCILE_INTERVAL;
        cfg.discovery.reconcile_jitter_percent = 0;
        cfg.reader.eof_reprobe_interval = MIN_EOF_REPROBE_INTERVAL;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.discovery.reconcile_interval = MAX_RECONCILE_INTERVAL;
        cfg.discovery.reconcile_jitter_percent = MAX_RECONCILE_JITTER_PERCENT;
        cfg.reader.eof_reprobe_interval = MAX_EOF_REPROBE_INTERVAL;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.discovery.reconcile_interval = MIN_RECONCILE_INTERVAL - Duration::from_nanos(1);
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(error.to_string().contains("100ms..=24h"), "{error}");

        let mut cfg = minimal_config();
        cfg.discovery.reconcile_interval = MAX_RECONCILE_INTERVAL
            .checked_add(Duration::from_nanos(1))
            .unwrap();
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(error.to_string().contains("100ms..=24h"), "{error}");

        let mut cfg = minimal_config();
        cfg.discovery.reconcile_jitter_percent = MAX_RECONCILE_JITTER_PERCENT + 1;
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(error.to_string().contains("0..=25"), "{error}");

        let mut cfg = minimal_config();
        cfg.reader.eof_reprobe_interval = MIN_EOF_REPROBE_INTERVAL - Duration::from_nanos(1);
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(error.to_string().contains("10ms..=1h"), "{error}");

        let mut cfg = minimal_config();
        cfg.reader.eof_reprobe_interval = MAX_EOF_REPROBE_INTERVAL
            .checked_add(Duration::from_nanos(1))
            .unwrap();
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(error.to_string().contains("10ms..=1h"), "{error}");
    }

    /// Scenario: a config uses the superseded filelog
    /// `discovery.poll_interval` field.
    /// Guarantees: the unreleased implementation accepts only the
    /// authoritative `reconcile_interval` shape and reports the old key as
    /// unknown rather than silently coupling discovery and EOF cadence.
    #[test]
    fn superseded_discovery_poll_interval_is_rejected() {
        let error = serde_json::from_value::<Config>(serde_json::json!({
            "include": ["app.log"],
            "discovery": { "poll_interval": "5s" }
        }))
        .unwrap_err();
        assert!(error.to_string().contains("poll_interval"), "{error}");
    }

    /// Scenario: checked reconciliation jitter arithmetic is evaluated at
    /// zero jitter, the default 10 percent, and an unrepresentable duration.
    /// Guarantees: exact floor-based symmetric bounds are derived without
    /// saturation and overflow is reported as `None`.
    #[test]
    fn reconciliation_jitter_bounds_use_checked_nanoseconds() {
        assert_eq!(
            reconciliation_delay_bounds_ns(Duration::from_secs(5), 0),
            Some((5_000_000_000, 5_000_000_000))
        );
        assert_eq!(
            reconciliation_delay_bounds_ns(Duration::from_secs(5), 10),
            Some((4_500_000_000, 5_500_000_000))
        );
        assert_eq!(reconciliation_delay_bounds_ns(Duration::MAX, 25), None);
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
        assert!(err.to_string().contains("checkpoint recovery"), "{err}");

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

    /// Scenario: each decode-error policy is validated with otherwise
    /// identical default framing and identity configuration.
    /// Guarantees: preserve-raw, replace, and fail map into three distinct
    /// runtime framing-profile digests.
    #[test]
    fn on_decode_error_changes_runtime_profile_digest() {
        let digest_for = |on_decode_error| {
            let mut cfg = minimal_config();
            cfg.on_decode_error = on_decode_error;
            RuntimeConfig::from_config(cfg, "node-1")
                .expect("decode-error policy must validate")
                .framing_profile_digest
        };

        let preserve_raw = digest_for(OnDecodeError::PreserveRaw);
        let replace = digest_for(OnDecodeError::Replace);
        let fail = digest_for(OnDecodeError::Fail);

        assert_ne!(preserve_raw, replace);
        assert_ne!(preserve_raw, fail);
        assert_ne!(replace, fail);
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

    /// Scenario: every encoding and decode-error policy is configured at the
    /// maximum atomic emitted-unit width, then each bound is reduced by one
    /// where that is possible.
    /// Guarantees: raw and ASCII/fail accept one byte, ASCII replacement and
    /// preserve-raw require three bytes, and UTF-8/UTF-16 require four bytes
    /// without imposing an unnecessary four-byte ASCII/fail minimum.
    #[test]
    fn framing_bounds_follow_maximum_atomic_emitted_unit() {
        for (encoding, on_decode_error, minimum) in [
            (Encoding::Raw, OnDecodeError::PreserveRaw, 1),
            (Encoding::Raw, OnDecodeError::Replace, 1),
            (Encoding::Raw, OnDecodeError::Fail, 1),
            (Encoding::Ascii, OnDecodeError::PreserveRaw, 3),
            (Encoding::Ascii, OnDecodeError::Replace, 3),
            (Encoding::Ascii, OnDecodeError::Fail, 1),
            (Encoding::Utf8, OnDecodeError::PreserveRaw, 4),
            (Encoding::Utf8, OnDecodeError::Replace, 4),
            (Encoding::Utf8, OnDecodeError::Fail, 4),
            (Encoding::Utf16Le, OnDecodeError::PreserveRaw, 4),
            (Encoding::Utf16Le, OnDecodeError::Replace, 4),
            (Encoding::Utf16Le, OnDecodeError::Fail, 4),
            (Encoding::Utf16Be, OnDecodeError::PreserveRaw, 4),
            (Encoding::Utf16Be, OnDecodeError::Replace, 4),
            (Encoding::Utf16Be, OnDecodeError::Fail, 4),
        ] {
            let mut cfg = minimal_config();
            cfg.encoding = encoding;
            cfg.on_decode_error = on_decode_error;
            cfg.framing.max_line_bytes = minimum;
            cfg.framing.max_record_bytes = minimum;
            let _runtime = RuntimeConfig::from_config(cfg.clone(), "node-1")
                .expect("the exact atomic-unit framing bounds must validate");

            if minimum > 1 {
                cfg.framing.max_line_bytes = minimum - 1;
                let err = RuntimeConfig::from_config(cfg.clone(), "node-1").unwrap_err();
                assert!(err.to_string().contains("max_line_bytes"), "{err}");
                assert!(
                    err.to_string().contains(&format!("at least {minimum}")),
                    "{err}"
                );

                cfg.framing.max_line_bytes = minimum;
                cfg.framing.max_record_bytes = minimum - 1;
                let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
                assert!(err.to_string().contains("max_record_bytes"), "{err}");
                assert!(
                    err.to_string().contains(&format!("at least {minimum}")),
                    "{err}"
                );
            }
        }
    }

    /// Scenario: the peak framer payload helper is evaluated for one and two
    /// retained payload copies with different line and record bounds.
    /// Guarantees: it implements the complete conservative formula, including
    /// the minimum bound, fourfold growth factor, copy count, and fixed terms.
    #[test]
    fn peak_framer_payload_formula_is_complete() {
        assert_eq!(peak_framer_payload_bytes(8, 12, 1), Some(112));
        assert_eq!(peak_framer_payload_bytes(8, 12, 2), Some(208));
        assert_eq!(
            framer_payload_copy_count(Encoding::Raw, OnDecodeError::PreserveRaw),
            1
        );
        assert_eq!(
            framer_payload_copy_count(Encoding::Utf8, OnDecodeError::PreserveRaw),
            2
        );
        assert_eq!(
            framer_payload_copy_count(Encoding::Utf8, OnDecodeError::Replace),
            1
        );
    }

    /// Scenario: both framing byte bounds are the largest value representable
    /// by `usize`, so the complete conservative peak formula cannot be
    /// represented even though each individual bound can.
    /// Guarantees: RuntimeConfig construction rejects arithmetic overflow
    /// with a framing-bounds error before runtime allocation is possible.
    #[test]
    fn peak_framer_payload_overflow_is_rejected_during_config_build() {
        let mut cfg = minimal_config();
        cfg.encoding = Encoding::Raw;
        cfg.framing.max_line_bytes = usize::MAX as u64;
        cfg.framing.max_record_bytes = usize::MAX as u64;
        cfg.batch.max_bytes = u64::MAX;

        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("framing bounds"), "{err}");
        assert!(err.to_string().contains("peak framer payload"), "{err}");
        assert!(err.to_string().contains("overflow usize"), "{err}");
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

    /// Scenario: text and raw multiline end patterns use a backreference
    /// (`\1`), lookahead (`(?=`), or lookbehind (`(?<=`) construct.
    /// Guarantees: both compiled matcher variants surface every
    /// RE2-incompatible construct as structured `InvalidUserConfig` instead
    /// of panicking or silently accepting it.
    #[test]
    fn re2_incompatible_constructs_are_rejected() {
        for encoding in [Encoding::Utf8, Encoding::Raw] {
            for pattern in [r"(foo)\1", r"(?=foo)", r"(?!foo)", r"(?<=foo)", r"(?<!foo)"] {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                let err = RuntimeConfig::from_config(cfg, "node-1")
                    .expect_err(&format!("pattern '{pattern}' must be rejected"));
                assert!(err.to_string().contains("RE2-compatible"));
            }
        }
    }

    /// Scenario: Text and raw re2-v1 patterns use Rust-only `u`, `R`, or `x`
    /// inline flags in global, disabled, scoped, and mixed flag groups.
    /// Guarantees: only RE2's `i`, `m`, `s`, and `U` flags are accepted,
    /// while escaped text, POSIX classes, and character-class literals are
    /// not mistaken for inline flag syntax.
    #[test]
    fn rust_only_inline_flags_are_rejected_for_re2_v1() {
        for encoding in [Encoding::Utf8, Encoding::Raw] {
            for pattern in [
                r"(?u)^x$",
                r"(?-u:^x$)",
                r"(?R:^x$)",
                r"(?-R)^x$",
                r"(?x)^x$",
                r"(?im-x:^x$)",
            ] {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                let err = RuntimeConfig::from_config(cfg, "node-1")
                    .expect_err(&format!("pattern '{pattern}' must be rejected"));
                assert!(err.to_string().contains("re2-v1"), "{err}");
                assert!(
                    err.to_string()
                        .contains("only RE2 inline flags i, m, s, and U"),
                    "{err}"
                );
            }

            for pattern in [r"^\(\?u\)$", r"^[?u]+$", r"[[:alpha:](?u)]"] {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                let _runtime = RuntimeConfig::from_config(cfg, "node-1").unwrap_or_else(|err| {
                    panic!("literal pattern '{pattern}' must validate: {err}")
                });
            }

            let mut cfg = minimal_config();
            cfg.encoding = encoding;
            cfg.framing.multiline.line_end_pattern = Some(r"(?ims-U:^x$)".to_owned());
            let _runtime = RuntimeConfig::from_config(cfg, "node-1")
                .expect("all and only RE2 inline flags must validate");
        }
    }

    /// Scenario: Text and raw re2-v1 patterns use Perl digit, whitespace,
    /// word, negated-class, and word-boundary escapes on non-ASCII input.
    /// Guarantees: The compiled matcher uses RE2's ASCII-only semantics
    /// instead of Rust regex's default Unicode Perl-class semantics.
    #[test]
    fn re2_perl_classes_and_boundaries_are_ascii_only() {
        for encoding in [Encoding::Utf8, Encoding::Raw] {
            let matcher_for = |pattern: &str| {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                RuntimeConfig::from_config(cfg, "node-1")
                    .expect("RE2 Perl-class pattern must validate")
                    .compiled_multiline_pattern
                    .expect("configured pattern must compile")
            };

            let digits = matcher_for(r"^\d+$");
            assert!(digits.is_match(b"123").unwrap());
            assert!(!digits.is_match("\u{0661}".as_bytes()).unwrap());

            let not_ascii_digits = matcher_for(r"^[\D]+$");
            assert!(not_ascii_digits.is_match(b"abc").unwrap());
            assert!(not_ascii_digits.is_match("\u{0661}".as_bytes()).unwrap());

            let whitespace = matcher_for(r"^\s+$");
            assert!(whitespace.is_match(b"\t \r").unwrap());
            assert!(!whitespace.is_match("\u{00a0}".as_bytes()).unwrap());

            let words = matcher_for(r"^\w+$");
            assert!(words.is_match(b"Az_09").unwrap());
            assert!(!words.is_match("\u{00e9}".as_bytes()).unwrap());

            let boundary = matcher_for(r"^A\b");
            assert!(boundary.is_match(b"A").unwrap());
            assert!(boundary.is_match("A\u{00e9}".as_bytes()).unwrap());

            let not_boundary = matcher_for(r"^A\B");
            assert!(!not_boundary.is_match("A\u{00e9}".as_bytes()).unwrap());
        }
    }

    /// Scenario: Text and raw patterns escape angle brackets outside and
    /// inside character classes using RE2's escaped-punctuation syntax.
    /// Guarantees: Escaped punctuation remains literal and cannot inherit
    /// Rust regex's `\<` and `\>` word-boundary assertion semantics.
    #[test]
    fn re2_escaped_punctuation_is_always_literal() {
        for encoding in [Encoding::Utf8, Encoding::Raw] {
            let matcher_for = |pattern: &str| {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                RuntimeConfig::from_config(cfg, "node-1")
                    .expect("escaped RE2 punctuation must validate")
                    .compiled_multiline_pattern
                    .expect("configured pattern must compile")
            };

            let angles = matcher_for(r"^\<START\>$");
            assert!(angles.is_match(b"<START>").unwrap());
            assert!(!angles.is_match(b"START").unwrap());

            let angle_class = matcher_for(r"^[\<\>]+$");
            assert!(angle_class.is_match(b"><>").unwrap());
            assert!(!angle_class.is_match(b"START").unwrap());
        }
    }

    /// Scenario: Text and raw patterns use Unicode property escapes, class
    /// set operations, or letter escapes that re2-v1 does not execute.
    /// Guarantees: Constructs whose accepted grammar or semantics would
    /// differ from the executable RE2 subset fail config validation.
    #[test]
    fn re2_v1_rejects_unsupported_properties_class_sets_and_escapes() {
        for encoding in [Encoding::Utf8, Encoding::Raw] {
            for pattern in [
                r"^\p{Greek}+$",
                r"[a&&b]",
                r"[a--b]",
                r"[a~~b]",
                r"\C",
                r"\Qliteral\E",
                r"\e",
                r"\m",
                r"[\A]",
            ] {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                let err = RuntimeConfig::from_config(cfg, "node-1")
                    .expect_err(&format!("pattern '{pattern}' must be rejected"));
                assert!(err.to_string().contains("re2-v1"), "{err}");
            }
        }
    }

    /// Scenario: Text and raw patterns use counted repetitions at RE2's
    /// 1000-count ceiling, above it, without a minimum, and as escaped text.
    /// Guarantees: Valid bounds through 1000 compile, larger or missing
    /// minimum bounds are rejected, and escaped braces are not misclassified.
    #[test]
    fn re2_counted_repetition_bounds_are_enforced() {
        for encoding in [Encoding::Utf8, Encoding::Raw] {
            for pattern in [r"^a{1000}$", r"^a{0,1000}$", r"^\{1001\}$"] {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                let _runtime = RuntimeConfig::from_config(cfg, "node-1")
                    .unwrap_or_else(|err| panic!("pattern '{pattern}' must validate: {err}"));
            }

            for pattern in [r"a{1001}", r"a{1,1001}", r"a{1001,}", r"a{,10}"] {
                let mut cfg = minimal_config();
                cfg.encoding = encoding;
                cfg.framing.multiline.line_end_pattern = Some(pattern.to_owned());
                let err = RuntimeConfig::from_config(cfg, "node-1")
                    .expect_err(&format!("pattern '{pattern}' must be rejected"));
                assert!(err.to_string().contains("re2-v1"), "{err}");
            }
        }
    }

    /// Scenario: Text and raw multiline patterns are configured at exactly the durable 4096-byte limit and one byte beyond it, with the oversized input also syntactically invalid.
    /// Guarantees: The exact limit compiles and profiles successfully, while the oversized pattern is rejected by the byte bound before regex scanning or compilation.
    #[test]
    fn multiline_pattern_length_is_bounded_before_compilation() {
        for encoding in [Encoding::Utf8, Encoding::Raw] {
            let mut exact = minimal_config();
            exact.encoding = encoding;
            exact.framing.multiline.line_end_pattern = Some("a".repeat(FRAMING_PATTERN_MAX_BYTES));
            let _runtime = RuntimeConfig::from_config(exact, "node-1")
                .expect("an exact-bound literal pattern must validate");

            let mut oversized = minimal_config();
            oversized.encoding = encoding;
            oversized.framing.multiline.line_end_pattern =
                Some("(".repeat(FRAMING_PATTERN_MAX_BYTES + 1));
            let err = RuntimeConfig::from_config(oversized, "node-1")
                .expect_err("an oversized pattern must fail before compilation");
            assert!(
                err.to_string()
                    .contains(&format!("{FRAMING_PATTERN_MAX_BYTES}-byte maximum")),
                "{err}"
            );
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
        assert!(
            runtime
                .compiled_multiline_pattern
                .as_ref()
                .expect("pattern must be compiled")
                .is_match(b"END request")
                .expect("decoded text is valid UTF-8")
        );

        let default_runtime =
            RuntimeConfig::from_config(minimal_config(), "node-1").expect("must validate");
        assert_ne!(
            runtime.framing_profile_digest,
            default_runtime.framing_profile_digest
        );
    }

    /// Scenario: raw multiline uses plain RE2-compatible `\xFF` to match an
    /// invalid UTF-8 byte, without a Rust-only inline Unicode flag change.
    /// Guarantees: the globally non-Unicode byte regex sees the original byte
    /// and returns `Ok`, rather than requiring `(?-u:...)` or lossy conversion.
    #[test]
    fn raw_multiline_matches_invalid_utf8_bytes() {
        let mut cfg = minimal_config();
        cfg.encoding = Encoding::Raw;
        cfg.framing.multiline.line_start_pattern = Some(r"^\xFF$".to_owned());
        let runtime = RuntimeConfig::from_config(cfg, "node-1").expect("must validate");
        let pattern = runtime
            .compiled_multiline_pattern
            .as_ref()
            .expect("raw pattern must be compiled");

        assert_eq!(pattern.is_match(&[0xff]), Ok(true));
        assert_eq!(pattern.is_match("\u{fffd}".as_bytes()), Ok(false));
    }

    /// Scenario: a compiled text multiline matcher receives invalid UTF-8,
    /// while an equivalent raw matcher receives the same bytes.
    /// Guarantees: text matching returns the explicit UTF-8 error in every
    /// build mode, and raw matching remains byte-oriented and returns `Ok`.
    #[test]
    fn multiline_match_reports_invalid_text_utf8_but_raw_accepts_bytes() {
        let mut text_cfg = minimal_config();
        text_cfg.framing.multiline.line_start_pattern = Some("^.$".to_owned());
        let text_runtime =
            RuntimeConfig::from_config(text_cfg, "node-1").expect("text config must validate");
        let text_pattern = text_runtime
            .compiled_multiline_pattern
            .as_ref()
            .expect("text pattern must be compiled");
        let error = text_pattern
            .is_match(&[0xff])
            .expect_err("invalid text input must return its UTF-8 error");
        assert_eq!(error.valid_up_to(), 0);
        assert_eq!(error.error_len(), Some(1));

        let mut raw_cfg = minimal_config();
        raw_cfg.encoding = Encoding::Raw;
        raw_cfg.framing.multiline.line_start_pattern = Some("^.$".to_owned());
        let raw_runtime =
            RuntimeConfig::from_config(raw_cfg, "node-1").expect("raw config must validate");
        let raw_pattern = raw_runtime
            .compiled_multiline_pattern
            .as_ref()
            .expect("raw pattern must be compiled");
        assert_eq!(raw_pattern.is_match(&[0xff]), Ok(true));
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

    /// Scenario: receiver-owned descriptor budgets are below, exactly at,
    /// and above 80 percent and the process soft limit.
    /// Guarantees: equality with 80 percent does not warn, values above it
    /// warn, equality with the soft limit is accepted, and exceedance is
    /// rejected before any handle is opened.
    #[test]
    fn descriptor_budget_enforces_soft_limit_and_warning_boundary() {
        let exact_eighty = descriptor_budget_against_soft_limit(70, Some(100)).unwrap();
        assert_eq!(exact_eighty.owned, 80);
        assert!(!exact_eighty.warning);

        let above_eighty = descriptor_budget_against_soft_limit(71, Some(100)).unwrap();
        assert_eq!(above_eighty.owned, 81);
        assert!(above_eighty.warning);

        let exact_limit = descriptor_budget_against_soft_limit(90, Some(100)).unwrap();
        assert_eq!(exact_limit.owned, 100);
        assert!(exact_limit.warning);

        let error = descriptor_budget_against_soft_limit(91, Some(100)).unwrap_err();
        assert!(error.contains("exceeds process soft limit"));

        let unlimited = descriptor_budget_against_soft_limit(u32::MAX, None).unwrap();
        assert_eq!(unlimited.owned, u64::from(u32::MAX) + 10);
        assert!(!unlimited.warning);
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
        cfg.checkpoint.compact_after_bytes = store_limits::minimum_compact_after_bytes().unwrap();

        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("identity reconciliation worst-case working set"),
            "{error}"
        );
    }

    /// Scenario: a small synthetic identity population is evaluated against
    /// the reviewed candidate, open-candidate, checkpoint-record, and
    /// discovery-tracked formulas.
    /// Guarantees: every path copy includes the fixed 44-byte AdvisoryPath
    /// fields, and checkpoint records include the explicit 34-byte committed
    /// frontier guard rather than hiding either value in another coefficient.
    #[test]
    fn identity_reconciliation_formula_matches_reviewed_terms() {
        let identity = IdentityConfig {
            fingerprint_bytes: 32,
            ..IdentityConfig::default()
        };
        let limits = LimitsConfig {
            max_tracked_files: 5,
            max_pending_candidates: 2,
            max_open_files: 3,
            max_read_bytes_per_turn: 7,
        };
        const PATH_BYTES: u64 = 44 + 4096;
        let expected = (2 + 3) * (5 * 32 + 4 * PATH_BYTES + 2048)
            + 3 * (10 * 32 + 10 * PATH_BYTES + 4096)
            + 5 * (32 + PATH_BYTES + 34 + 384)
            + 5 * 1024;

        assert_eq!(
            identity_reconciliation_bytes(&identity, &limits).unwrap(),
            expected
        );
        assert_eq!(ADVISORY_PATH_MODELED_BYTES, PATH_BYTES);
        assert_eq!(COMMITTED_FRONTIER_GUARD_LEN, 34);
    }

    /// Scenario: the reader-table formula is evaluated with small exact
    /// values and then with a fingerprint width that makes its first checked
    /// sum unrepresentable.
    /// Guarantees: one shared turn buffer is added after the per-reader
    /// product, every reader includes two complete AdvisoryPath values, and
    /// overflow reports all knobs that can reduce the bound.
    #[test]
    fn reader_table_formula_is_exact_and_checked() {
        let identity = IdentityConfig {
            fingerprint_bytes: 32,
            ..IdentityConfig::default()
        };
        let limits = LimitsConfig {
            max_tracked_files: 5,
            max_pending_candidates: 2,
            max_open_files: 3,
            max_read_bytes_per_turn: 7,
        };
        let expected = 5 * (32 + 2 * (44 + 4096) + 1024) + 7;
        assert_eq!(
            reader_table_payload_bytes(&identity, &limits).unwrap(),
            expected
        );

        let overflowing = IdentityConfig {
            fingerprint_bytes: u64::MAX,
            ..IdentityConfig::default()
        };
        let error = reader_table_payload_bytes(&overflowing, &limits).unwrap_err();
        assert!(error.to_string().contains("reader-table"), "{error}");
        assert!(
            error.to_string().contains("identity.fingerprint_bytes"),
            "{error}"
        );
    }

    /// Scenario: a small validated configuration derives every numeric
    /// resource term, both sequential phase subtotals, and the explicit list
    /// of implementation/library terms that still require measurement.
    /// Guarantees: the integrated report covers all reviewed categories,
    /// counts the carry-over framer only in the resident-framer population,
    /// retains named ceilings only where the design defines them, and does
    /// not claim a complete RSS ceiling.
    #[test]
    fn aggregate_resource_report_is_phase_aware_and_complete() {
        let mut cfg = minimal_config();
        cfg.encoding = Encoding::Raw;
        cfg.on_decode_error = OnDecodeError::Fail;
        cfg.identity.fingerprint_bytes = 32;
        cfg.framing.max_line_bytes = 8;
        cfg.framing.max_record_bytes = 12;
        cfg.limits = LimitsConfig {
            max_tracked_files: 5,
            max_pending_candidates: 2,
            max_open_files: 3,
            max_read_bytes_per_turn: 7,
        };
        cfg.batch.max_records = 4;
        cfg.batch.max_bytes = 1024 * 1024;
        cfg.checkpoint.compact_after_bytes = store_limits::minimum_compact_after_bytes().unwrap();
        cfg.checkpoint.compact_after_transactions = 1;

        let runtime = RuntimeConfig::from_config(cfg, "node-1").unwrap();
        let report = runtime.resource_admission;
        const PATH_BYTES: u64 = 44 + 4096;
        let expected_identity = (2 + 3) * (5 * 32 + 4 * PATH_BYTES + 2048)
            + 3 * (10 * 32 + 10 * PATH_BYTES + 4096)
            + 5 * (32 + PATH_BYTES + 34 + 384)
            + 5 * 1024;
        let expected_reader = 5 * (32 + 2 * PATH_BYTES + 1024) + 7;
        let expected_framer_per_reader = 4 * (8 + 12) + 16 + 16;
        let expected_framers = 3 * expected_framer_per_reader;
        let expected_progress_transaction = 36 + 4 * 109 + 4;
        let expected_retained = 1024 * 1024 + expected_progress_transaction;
        let expected_carry_over = 1024 * 1024 + 109;
        let expected_runtime = expected_identity
            + expected_reader
            + expected_framers
            + expected_retained
            + expected_carry_over;
        let expected_recovery = report.checkpoint_limits.max_recovery_working_bytes;

        assert_eq!(report.candidate_identity_state.bytes, expected_identity);
        assert_eq!(
            report
                .candidate_identity_state
                .named_provisional_ceiling_bytes,
            Some(IDENTITY_RECONCILIATION_BYTES_CEILING)
        );
        assert_eq!(report.reader_state.bytes, expected_reader);
        assert_eq!(
            report.framer_payload_per_reader.bytes,
            expected_framer_per_reader
        );
        assert_eq!(report.framer_payload.bytes, expected_framers);
        assert_eq!(report.retained_batch.bytes, expected_retained);
        assert_eq!(report.carry_over.bytes, expected_carry_over);
        assert_eq!(report.regex_program_cache.bytes, 0);
        assert_eq!(
            report.checkpoint_recovery.named_provisional_ceiling_bytes,
            Some(RECOVERY_WORKING_BYTES_CEILING)
        );
        assert_eq!(report.numeric_runtime_subtotal_bytes, expected_runtime);
        assert_eq!(report.numeric_recovery_subtotal_bytes, expected_recovery);
        assert_eq!(
            report.numeric_peak_subtotal_bytes,
            expected_runtime.max(expected_recovery)
        );
        assert_eq!(
            report.unmeasured_terms(),
            &RESOURCE_ADMISSION_UNMEASURED_TERMS
        );
        assert!(
            report
                .unmeasured_terms()
                .iter()
                .any(|term| term.contains("checkpoint runtime maintenance"))
        );
        assert!(
            report
                .unmeasured_terms()
                .iter()
                .any(|term| term.contains("bounded channel"))
        );
        assert_eq!(report.complete_rss_ceiling_bytes(), None);
    }

    /// Scenario: individually representable configured terms overflow first
    /// a retained-batch addition and then the integrated runtime subtotal.
    /// Guarantees: aggregate admission uses checked arithmetic at both term
    /// and phase boundaries and reports the exact formula plus actionable
    /// knobs instead of wrapping or applying a universal ceiling.
    #[test]
    fn aggregate_resource_report_rejects_checked_overflow() {
        let identity = IdentityConfig {
            fingerprint_bytes: 16,
            ..IdentityConfig::default()
        };
        let limits = LimitsConfig {
            max_tracked_files: 1,
            max_pending_candidates: 1,
            max_open_files: 1,
            max_read_bytes_per_turn: 1,
        };
        let checkpoint_limits = StoreLimits::derive(
            store_limits::minimum_compact_after_bytes().unwrap(),
            1,
            1,
            16,
        )
        .unwrap();
        let overflowing_batch = BatchConfig {
            max_records: 1,
            max_bytes: u64::MAX,
            ..BatchConfig::default()
        };
        let error = validate_resource_admission(
            &identity,
            &limits,
            &overflowing_batch,
            checkpoint_limits,
            1,
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("retained-batch resource formula"),
            "{error}"
        );
        assert!(error.to_string().contains("batch.max_bytes"), "{error}");

        let small_batch = BatchConfig {
            max_records: 1,
            max_bytes: 1,
            ..BatchConfig::default()
        };
        let error = validate_resource_admission(
            &identity,
            &limits,
            &small_batch,
            checkpoint_limits,
            u64::MAX,
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("resource-admission runtime numeric subtotal"),
            "{error}"
        );
        assert!(
            error.to_string().contains("limits.max_open_files"),
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
    /// `batch.max_bytes` that the shared logical-size overhead
    /// pushes it over the batch bound.
    /// Guarantees: the same logical-size function used by runtime flushing
    /// rejects the configuration at build time instead of deferring the
    /// failure to a runtime record that can never be flushed.
    #[test]
    fn oversized_record_bound_relative_to_batch_bytes_is_rejected() {
        let small_line_bytes = 10u64;
        let batch_bytes = configured_logical_record_size(
            small_line_bytes,
            MaxLogSizeBehavior::Split,
            OnDecodeError::PreserveRaw,
        )
        .expect("small body_bytes must not overflow");

        let mut cfg = minimal_config();
        cfg.batch.max_bytes = batch_bytes;
        cfg.framing.max_line_bytes = small_line_bytes;
        // One byte more than `small_line_bytes` pushes only the record bound
        // over `batch_bytes`, since configured sizing is monotonic in
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
        let small_record_bytes = 10u64;
        let batch_bytes = configured_logical_record_size(
            small_record_bytes,
            MaxLogSizeBehavior::Split,
            OnDecodeError::PreserveRaw,
        )
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

    /// Scenario: bounded native and registered provenance alternatives, split
    /// policy, the longest flush marker, and preserve-raw decode evidence
    /// contribute to the configuration worst case.
    /// Guarantees: the high-level bound is exactly the shared primitive over
    /// every common field plus the larger provenance alternative.
    #[test]
    fn configured_logical_size_uses_exact_worst_case_attribute_formula() {
        let common = [
            LogicalAttributeSize::new(ATTR_KEY_FRAGMENT_ID, FRAGMENT_ID_VALUE_BYTES).unwrap(),
            LogicalAttributeSize::new(
                ATTR_KEY_FRAGMENT_INDEX,
                logical_int_value_len(i64::from(u32::MAX)),
            )
            .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_FRAGMENT_IS_LAST, logical_bool_value_len(false))
                .unwrap(),
            LogicalAttributeSize::new(
                ATTR_KEY_FRAGMENT_BODY_START,
                logical_int_value_len(i64::MAX),
            )
            .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_FRAGMENT_BODY_END, logical_int_value_len(i64::MAX))
                .unwrap(),
            LogicalAttributeSize::new(
                ATTR_KEY_FRAGMENT_FRAME_START,
                logical_int_value_len(i64::MAX),
            )
            .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_FRAGMENT_FRAME_END, logical_int_value_len(i64::MAX))
                .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_FLUSH_REASON, "oversize_line_boundary".len() as u64)
                .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_TERMINAL_UNTERMINATED, logical_bool_value_len(true))
                .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_DECODE_ERROR_POLICY, "preserve_raw".len() as u64)
                .unwrap(),
            LogicalAttributeSize::new(
                ATTR_KEY_DECODE_ERROR_COUNT,
                RESERVED_DECIMAL_U64_VALUE_BYTES,
            )
            .unwrap(),
        ];
        let registered = [
            LogicalAttributeSize::new(ATTR_KEY_LOG_FILE_PATH, MAX_REGISTERED_PATH_TEXT_VALUE_BYTES)
                .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_LOG_FILE_NAME, MAX_REGISTERED_PATH_TEXT_VALUE_BYTES)
                .unwrap(),
        ];
        let native = [
            LogicalAttributeSize::new(ATTR_KEY_PATH_KIND, MAX_PATH_KIND_VALUE_BYTES).unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_PATH_NATIVE, MAX_NATIVE_PATH_ATTRIBUTE_VALUE_BYTES)
                .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_PATH_TRUNCATED, logical_bool_value_len(true))
                .unwrap(),
            LogicalAttributeSize::new(ATTR_KEY_PATH_SHA256, SHA256_HEX_VALUE_BYTES).unwrap(),
        ];
        let expected_registered =
            checked_logical_record_size(123, common.into_iter().chain(registered)).unwrap();
        let expected_native =
            checked_logical_record_size(123, common.into_iter().chain(native)).unwrap();
        assert!(expected_registered > expected_native);
        let expected = expected_registered.max(expected_native);
        let actual = configured_logical_record_size(
            123,
            MaxLogSizeBehavior::Split,
            OnDecodeError::PreserveRaw,
        )
        .unwrap();
        assert_eq!(actual, expected);

        let fail_decode =
            configured_logical_record_size(123, MaxLogSizeBehavior::Split, OnDecodeError::Fail)
                .unwrap();
        assert!(fail_decode < actual);
    }

    /// Scenario: `MaxLogSizeBehavior::Truncate` is configured instead of the
    /// default `Split`.
    /// Guarantees: configured sizing counts the truncate marker
    /// attribute's overhead rather than the (larger) fragment-id/index/last
    /// attribute set, so the two policies are not conflated.
    #[test]
    fn oversize_behavior_changes_logical_record_size_overhead() {
        let split = configured_logical_record_size(
            10,
            MaxLogSizeBehavior::Split,
            OnDecodeError::PreserveRaw,
        )
        .expect("small body_bytes must not overflow");
        let truncate = configured_logical_record_size(
            10,
            MaxLogSizeBehavior::Truncate,
            OnDecodeError::PreserveRaw,
        )
        .expect("small body_bytes must not overflow");
        assert!(split > truncate);
    }

    /// Scenario: `body_bytes` is `u64::MAX`, so adding any fixed attribute
    /// overhead on top of it would overflow `u64`.
    /// Guarantees: configured logical sizing reports
    /// `LogicalSizeError::Overflow` via checked arithmetic instead of
    /// silently saturating to `u64::MAX`, which would let a `body_bytes` /
    /// `batch.max_bytes` pair both clamped to `u64::MAX` "validate"
    /// successfully despite being unrepresentable and unallocatable.
    #[test]
    fn logical_record_size_reports_overflow_instead_of_saturating() {
        let err = configured_logical_record_size(
            u64::MAX,
            MaxLogSizeBehavior::Split,
            OnDecodeError::PreserveRaw,
        )
        .expect_err("u64::MAX body_bytes plus any overhead must overflow");
        assert_eq!(err, LogicalSizeError::Overflow);
    }

    /// Scenario: `framing.max_record_bytes` is `u64::MAX` and
    /// `batch.max_bytes` is also `u64::MAX`.
    /// Guarantees: config validation rejects this configuration through
    /// checked framing arithmetic rather than accepting it because a
    /// saturating comparison would coincidentally clamp both sides to the
    /// same value.
    #[test]
    fn max_u64_record_bytes_and_batch_bytes_is_rejected() {
        let mut cfg = minimal_config();
        cfg.framing.max_record_bytes = u64::MAX;
        cfg.framing.max_line_bytes = 10;
        cfg.batch.max_bytes = u64::MAX;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("max_record_bytes"), "{err}");
        assert!(err.to_string().contains("overflow"), "{err}");
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

    /// Scenario: the complete-WAL byte threshold is exactly the header plus
    /// one maximum transaction, then one byte smaller.
    /// Guarantees: the exact minimum validates and the smaller value is
    /// rejected before namespace creation with a specific threshold error.
    #[test]
    fn checkpoint_byte_threshold_must_fit_one_maximum_transaction() {
        let minimum = store_limits::minimum_compact_after_bytes().unwrap();
        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_bytes = minimum;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_bytes = minimum - 1;
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            error.to_string().contains("smaller than the minimum"),
            "{error}"
        );
    }

    /// Scenario: checkpoint retention is zero, the largest duration
    /// representable as u64 nanoseconds, then one nanosecond larger.
    /// Guarantees: disabled and exactly representable retention validate,
    /// while an interval that cannot participate in checked runtime deadline
    /// arithmetic is rejected before namespace creation.
    #[test]
    fn checkpoint_retention_must_fit_u64_nanoseconds() {
        let mut cfg = minimal_config();
        cfg.checkpoint.retention = Duration::ZERO;
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.checkpoint.retention = Duration::from_nanos(u64::MAX);
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.checkpoint.retention = Duration::from_nanos(u64::MAX)
            .checked_add(Duration::from_nanos(1))
            .unwrap();
        let error = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("retention must be zero or fit positive u64 nanoseconds"),
            "{error}"
        );
    }

    /// The exact remedies the store's size formulas attach to a bound that
    /// cannot be honored, asserted verbatim so a rejection always names the
    /// knob an operator has to change.
    const WAL_REMEDY: &str =
        "reduce checkpoint.compact_after_bytes or checkpoint.compact_after_transactions";
    const RECOVERY_REMEDY: &str = "reduce limits.max_tracked_files, \
                                   checkpoint.compact_after_bytes, or \
                                   checkpoint.compact_after_transactions, or \
                                   identity.fingerprint_bytes";

    /// Scenario: the checkpoint size knobs that jointly determine artifact
    /// sizes and recovery memory -- `compact_after_bytes`,
    /// `compact_after_transactions`, `limits.max_tracked_files`, and
    /// `identity.fingerprint_bytes` -- are
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
            DEFAULT_COMPACT_AFTER_TRANSACTIONS,
            DEFAULT_MAX_TRACKED_FILES,
            DEFAULT_FINGERPRINT_BYTES,
        )
        .expect("the shipped defaults are recoverable");
        assert!(defaults.max_snapshot_bytes <= store_limits::ARTIFACT_BYTES_CEILING);
        assert!(defaults.max_wal_bytes <= store_limits::ARTIFACT_BYTES_CEILING);
        assert!(defaults.max_recovery_working_bytes <= RECOVERY_WORKING_BYTES_CEILING);
        assert!(RuntimeConfig::from_config(minimal_config(), "node-1").is_ok());

        // Largest tracked-file population whose complete recovery working
        // set remains bounded at the other defaults.
        let boundary_files = u32::try_from(largest_accepted(
            u64::from(DEFAULT_MAX_TRACKED_FILES),
            u64::from(u32::MAX),
            |candidate| {
                StoreLimits::derive(
                    DEFAULT_COMPACT_AFTER_BYTES,
                    DEFAULT_COMPACT_AFTER_TRANSACTIONS,
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
        assert!(err.to_string().contains("checkpoint recovery"), "{err}");
        assert!(err.to_string().contains(RECOVERY_REMEDY), "{err}");

        // Largest compaction threshold whose complete recovery working set
        // remains bounded at the other defaults.
        let boundary_bytes = largest_accepted(
            DEFAULT_COMPACT_AFTER_BYTES,
            store_limits::ARTIFACT_BYTES_CEILING,
            |candidate| {
                StoreLimits::derive(
                    candidate,
                    DEFAULT_COMPACT_AFTER_TRANSACTIONS,
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
        assert!(err.to_string().contains("checkpoint recovery"), "{err}");
        assert!(err.to_string().contains(RECOVERY_REMEDY), "{err}");

        // Nothing saturates: the transaction threshold still bounds the WAL
        // when the byte threshold is maximal, and the resulting artifact is
        // rejected against the fixed ceiling.
        let mut cfg = minimal_config();
        cfg.checkpoint.compact_after_bytes = u64::MAX;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("checkpoint WAL size"), "{err}");
        assert!(err.to_string().contains("exceeding"), "{err}");
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
                    DEFAULT_COMPACT_AFTER_TRANSACTIONS,
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
        assert!(err.to_string().contains("checkpoint recovery"), "{err}");
        assert!(err.to_string().contains(RECOVERY_REMEDY), "{err}");

        // The format's widest representable fingerprint is consequently
        // refused at the shipped population and WAL defaults.
        let mut cfg = minimal_config();
        cfg.identity.fingerprint_bytes = MAX_FINGERPRINT_BYTES;
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("checkpoint recovery"), "{err}");
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
    /// caller-supplied factory default.
    /// Guarantees: the default is used verbatim to resolve `checkpoint_id`
    /// and its namespace.
    #[test]
    fn missing_checkpoint_id_uses_supplied_default() {
        let cfg = minimal_config();
        let runtime = RuntimeConfig::from_config(cfg, "my-node").expect("must validate");
        assert_eq!(runtime.checkpoint_id, "my-node");
    }

    /// Scenario: `checkpoint.id` contains a character outside the accepted
    /// ASCII alphanumeric / `_` / `-` / `.` set.
    /// Guarantees: an unsupported checkpoint id is rejected before it can
    /// reach namespace-path derivation.
    #[test]
    fn checkpoint_id_rejects_unsupported_characters() {
        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("app logs/../etc".to_owned());
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_err());
    }

    /// Scenario: two accepted checkpoint ids differ only by ASCII case on a
    /// filesystem that may compare path names case-insensitively.
    /// Guarantees: byte-wise lowercase-hex encoding keeps their path
    /// segments distinct even after ASCII case folding, and the exact
    /// version-1 path vectors remain stable.
    #[test]
    fn checkpoint_id_encoding_is_case_insensitive_filesystem_injective() {
        let mixed = CheckpointNamespace::derive(checkpoint_state_dir(), "AppLogs").unwrap();
        let lower = CheckpointNamespace::derive(checkpoint_state_dir(), "applogs").unwrap();

        assert_eq!(mixed.encoded_component(), "4170704c6f6773");
        assert_eq!(lower.encoded_component(), "6170706c6f6773");
        assert_eq!(
            mixed.encoded_component(),
            mixed.encoded_component().to_ascii_lowercase()
        );
        assert_eq!(
            lower.encoded_component(),
            lower.encoded_component().to_ascii_lowercase()
        );
        assert_ne!(mixed.directory(), lower.directory());
        assert_eq!(checkpoint_namespace_dir("AppLogs"), mixed.directory());
        assert_eq!(checkpoint_namespace_dir("applogs"), lower.directory());
        let new_a = checkpoint_namespace_dir("a");
        let legacy_version_id = checkpoint_state_dir()
            .join(FILELOG_NAMESPACE_DIRECTORY)
            .join("v1");
        assert_ne!(new_a, legacy_version_id);
        assert!(
            !CHECKPOINT_NAMESPACE_VERSION
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')),
            "the version component must be outside the legacy ID alphabet"
        );
        assert_eq!(
            new_a,
            CheckpointNamespace::derive(checkpoint_state_dir(), "a")
                .unwrap()
                .into_directory()
        );
    }

    /// Scenario: `checkpoint.id` is the longest byte string whose
    /// version-prefix-plus-two-hex-digits-per-byte path segment fits the
    /// common 255-byte `NAME_MAX`, and then one byte longer.
    /// Guarantees: the exact boundary validates, the next byte is rejected,
    /// and the length formula includes the encoding prefix without overflow
    /// or off-by-one loss.
    #[test]
    fn checkpoint_id_encoded_segment_boundary_is_enforced() {
        assert_eq!(CHECKPOINT_NAMESPACE_COMPONENT_MAX_BYTES, 255);
        assert_eq!(CHECKPOINT_NAMESPACE_ID_MAX_BYTES, 127);

        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("a".repeat(CHECKPOINT_NAMESPACE_ID_MAX_BYTES));
        assert!(RuntimeConfig::from_config(cfg, "node-1").is_ok());

        let mut cfg = minimal_config();
        cfg.checkpoint.id = Some("a".repeat(CHECKPOINT_NAMESPACE_ID_MAX_BYTES + 1));
        let err = RuntimeConfig::from_config(cfg, "node-1").unwrap_err();
        assert!(err.to_string().contains("lowercase hexadecimal encoding"));
    }

    /// Scenario: runtime configuration uses `.` and `..` as raw
    /// `checkpoint.id` values.
    /// Guarantees: both logical IDs are accepted and resolve through the
    /// shared namespace helper to safe hexadecimal path components.
    #[test]
    fn checkpoint_id_dot_values_use_shared_namespace_derivation() {
        for (id, encoded) in [(".", "2e"), ("..", "2e2e")] {
            let mut cfg = minimal_config();
            cfg.checkpoint.id = Some(id.to_owned());
            let runtime = RuntimeConfig::from_config(cfg, "node-1").unwrap();
            let expected = CheckpointNamespace::derive(checkpoint_state_dir(), id).unwrap();
            assert_eq!(runtime.checkpoint_id, id);
            assert_eq!(runtime.checkpoint_namespace_dir, expected.directory());
            assert_eq!(expected.encoded_component(), encoded);
        }
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
