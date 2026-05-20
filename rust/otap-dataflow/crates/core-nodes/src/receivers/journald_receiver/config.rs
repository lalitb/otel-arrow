// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Configuration types for the journald receiver.
//!
//! See [`docs/journald-receiver.md`](../../../../../../docs/journald-receiver.md)
//! for the design document this implementation follows.

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Default stable OTAP source identifier for the local system journal.
pub const DEFAULT_SOURCE_ID: &str = "system";

/// Default per-batch record limit handed off from the worker to the async task.
const DEFAULT_BATCH_MAX_RECORDS: usize = 1024;
/// Default maximum time the worker waits before flushing a partial batch.
const DEFAULT_BATCH_MAX_FLUSH_PERIOD: Duration = Duration::from_millis(200);
/// Default `sd_journal_wait` timeout. Bounds shutdown / pause responsiveness.
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound for `sd_journal_wait` while shutdown is command-channel based.
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default drain deadline budget; must exceed `wait_timeout`.
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Default in-flight bound; v1 uses 1 to keep checkpoint advancement simple.
const DEFAULT_MAX_IN_FLIGHT_BATCHES: usize = 1;
/// Default consecutive checkpoint failure threshold before failing the source.
const DEFAULT_MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Default checkpoint root directory; the receiver appends a stable suffix.
const DEFAULT_CHECKPOINT_DIR: &str = "${engine.state_dir}/journald";
/// Default host root used by libsystemd for local journal access.
const DEFAULT_JOURNAL_ROOT_PATH: &str = "/";
/// Default maximum copied bytes per journal entry.
const DEFAULT_MAX_ENTRY_BYTES: usize = 1024 * 1024;
/// Default maximum copied bytes per journal field value.
const DEFAULT_MAX_FIELD_BYTES: usize = 256 * 1024;
/// Default maximum field count per journal entry.
const DEFAULT_MAX_FIELDS_PER_ENTRY: usize = 256;

fn default_source_id() -> String {
    DEFAULT_SOURCE_ID.to_owned()
}

fn default_priorities() -> Vec<u8> {
    (0..=7).collect()
}

/// Where the receiver should start when no checkpoint exists.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartAt {
    /// Start at the tail of the journal (default).
    #[default]
    End,
    /// Start at the head of the journal.
    Beginning,
}

/// Behavior when the downstream Nacks a batch.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnNack {
    /// Rewind to the last committed cursor and replay (default).
    #[default]
    Rewind,
    /// Fail the receiver source.
    Fail,
}

/// Shorthand for the maximum journald `PRIORITY` to accept.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxPriority {
    /// `PRIORITY=0`.
    Emergency,
    /// `PRIORITY=1`.
    Alert,
    /// `PRIORITY=2`.
    Critical,
    /// `PRIORITY=3`.
    Error,
    /// `PRIORITY=4`.
    Warning,
    /// `PRIORITY=5`.
    Notice,
    /// `PRIORITY=6`.
    Info,
    /// `PRIORITY=7`.
    Debug,
}

impl MaxPriority {
    const fn level(self) -> u8 {
        match self {
            MaxPriority::Emergency => 0,
            MaxPriority::Alert => 1,
            MaxPriority::Critical => 2,
            MaxPriority::Error => 3,
            MaxPriority::Warning => 4,
            MaxPriority::Notice => 5,
            MaxPriority::Info => 6,
            MaxPriority::Debug => 7,
        }
    }
}

/// Batch shaping for the worker -> async handoff.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatchConfig {
    /// Maximum records per emitted batch.
    #[serde(default = "BatchConfig::default_max_records")]
    pub max_records: usize,
    /// Maximum time the worker holds a partial batch before flushing.
    #[serde(
        default = "BatchConfig::default_max_flush_period",
        with = "humantime_serde"
    )]
    pub max_flush_period: Duration,
}

impl BatchConfig {
    const fn default_max_records() -> usize {
        DEFAULT_BATCH_MAX_RECORDS
    }
    const fn default_max_flush_period() -> Duration {
        DEFAULT_BATCH_MAX_FLUSH_PERIOD
    }
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_records: Self::default_max_records(),
            max_flush_period: Self::default_max_flush_period(),
        }
    }
}

/// Checkpoint configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointConfig {
    /// Root directory for checkpoint files. The receiver appends
    /// `<pipeline_group>/<pipeline_id>/<receiver_name>/<source_id>.cursor`.
    #[serde(default = "CheckpointConfig::default_directory")]
    pub directory: PathBuf,
    /// Maximum number of in-flight (un-Acked) batches.
    #[serde(default = "CheckpointConfig::default_max_in_flight_batches")]
    pub max_in_flight_batches: usize,
    /// Behavior on Nack.
    #[serde(default)]
    pub on_nack: OnNack,
    /// Maximum consecutive checkpoint commit failures before failing the source.
    #[serde(default = "CheckpointConfig::default_max_consecutive_failures")]
    pub max_consecutive_failures: u32,
}

impl CheckpointConfig {
    fn default_directory() -> PathBuf {
        PathBuf::from(DEFAULT_CHECKPOINT_DIR)
    }
    const fn default_max_in_flight_batches() -> usize {
        DEFAULT_MAX_IN_FLIGHT_BATCHES
    }
    const fn default_max_consecutive_failures() -> u32 {
        DEFAULT_MAX_CONSECUTIVE_FAILURES
    }
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            directory: Self::default_directory(),
            max_in_flight_batches: Self::default_max_in_flight_batches(),
            on_nack: OnNack::default(),
            max_consecutive_failures: Self::default_max_consecutive_failures(),
        }
    }
}

/// Concrete systemd journal source selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JournalConfig {
    /// Optional named systemd journal namespace. `None` selects the default
    /// namespace by passing NULL to systemd APIs. Named namespaces are rejected
    /// in v1 until `sd_journal_open_namespace` is wired.
    #[serde(default)]
    pub namespace: Option<String>,
    /// OS root for host journal access. `/` opens the local root;
    /// containerized host access can set this to a mounted host root such as
    /// `/host`.
    #[serde(default = "JournalConfig::default_root_path")]
    pub root_path: PathBuf,
}

impl JournalConfig {
    fn default_root_path() -> PathBuf {
        PathBuf::from(DEFAULT_JOURNAL_ROOT_PATH)
    }
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            namespace: None,
            root_path: Self::default_root_path(),
        }
    }
}

/// Policy applied when extraction limits are exceeded.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LargeFieldPolicy {
    /// Omit the field and count the drop.
    #[default]
    DropAndCount,
}

/// Hard limits for copying data out of a journald entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionConfig {
    /// Maximum total copied journal field value bytes per entry.
    #[serde(
        default = "ExtractionConfig::default_max_entry_bytes",
        deserialize_with = "deserialize_usize_bytes"
    )]
    pub max_entry_bytes: usize,
    /// Maximum copied bytes for one journal field value.
    #[serde(
        default = "ExtractionConfig::default_max_field_bytes",
        deserialize_with = "deserialize_usize_bytes"
    )]
    pub max_field_bytes: usize,
    /// Maximum number of journal fields copied from one entry.
    #[serde(default = "ExtractionConfig::default_max_fields_per_entry")]
    pub max_fields_per_entry: usize,
    /// Policy used when any extraction limit is exceeded.
    #[serde(default)]
    pub large_field_policy: LargeFieldPolicy,
}

impl ExtractionConfig {
    const fn default_max_entry_bytes() -> usize {
        DEFAULT_MAX_ENTRY_BYTES
    }
    const fn default_max_field_bytes() -> usize {
        DEFAULT_MAX_FIELD_BYTES
    }
    const fn default_max_fields_per_entry() -> usize {
        DEFAULT_MAX_FIELDS_PER_ENTRY
    }
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            max_entry_bytes: Self::default_max_entry_bytes(),
            max_field_bytes: Self::default_max_field_bytes(),
            max_fields_per_entry: Self::default_max_fields_per_entry(),
            large_field_policy: LargeFieldPolicy::default(),
        }
    }
}

/// User-facing journald receiver configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Stable OTAP source identifier used for telemetry and checkpoints.
    #[serde(default = "default_source_id")]
    pub source_id: String,

    /// Concrete systemd journal selection.
    #[serde(default)]
    pub journal: JournalConfig,

    /// Optional list of `_SYSTEMD_UNIT` matches.
    #[serde(default)]
    pub units: Vec<String>,

    /// Optional list of `SYSLOG_IDENTIFIER` matches.
    #[serde(default)]
    pub identifiers: Vec<String>,

    /// Exact-match priority set. Defaults to all levels (0..=7) when omitted.
    #[serde(default)]
    pub priorities: Option<Vec<u8>>,

    /// Shorthand for "include all priorities <= max_priority". Expanded into
    /// `priorities` during validation. Mutually exclusive with an explicit
    /// non-default `priorities` list.
    #[serde(default)]
    pub max_priority: Option<MaxPriority>,

    /// Where to start when no checkpoint exists.
    #[serde(default)]
    pub start_at: StartAt,

    /// Batch shaping.
    #[serde(default)]
    pub batch: BatchConfig,

    /// Checkpoint configuration.
    #[serde(default)]
    pub checkpoint: CheckpointConfig,

    /// Hard limits for entry/field extraction.
    #[serde(default)]
    pub extraction: ExtractionConfig,

    /// `sd_journal_wait` timeout; bounds pause/shutdown responsiveness.
    #[serde(default = "Config::default_wait_timeout", with = "humantime_serde")]
    pub wait_timeout: Duration,

    /// Drain deadline budget. Must be greater than `wait_timeout`.
    #[serde(default = "Config::default_drain_timeout", with = "humantime_serde")]
    pub drain_timeout: Duration,
}

impl Config {
    const fn default_wait_timeout() -> Duration {
        DEFAULT_WAIT_TIMEOUT
    }
    const fn default_drain_timeout() -> Duration {
        DEFAULT_DRAIN_TIMEOUT
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source_id: default_source_id(),
            journal: JournalConfig::default(),
            units: Vec::new(),
            identifiers: Vec::new(),
            priorities: None,
            max_priority: None,
            start_at: StartAt::default(),
            batch: BatchConfig::default(),
            checkpoint: CheckpointConfig::default(),
            extraction: ExtractionConfig::default(),
            wait_timeout: Self::default_wait_timeout(),
            drain_timeout: Self::default_drain_timeout(),
        }
    }
}

/// Validated, runtime-ready form of the journald receiver configuration.
///
/// Parsing produces this from `Config` once and the receiver hot path consumes
/// it without re-validating.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeConfig {
    /// Stable OTAP source identifier.
    pub(crate) source_id: String,
    /// Concrete systemd journal selection.
    pub(crate) journal: JournalConfig,
    /// Optional `_SYSTEMD_UNIT` matches (deduplicated, order preserved).
    pub(crate) units: Vec<String>,
    /// Optional `SYSLOG_IDENTIFIER` matches (deduplicated, order preserved).
    pub(crate) identifiers: Vec<String>,
    /// Sorted, deduplicated set of accepted journald `PRIORITY` levels.
    pub(crate) priorities: Vec<u8>,
    /// Where to start when there is no committed cursor.
    pub(crate) start_at: StartAt,
    /// Batch shaping.
    pub(crate) batch: BatchConfig,
    /// Checkpoint configuration.
    pub(crate) checkpoint: CheckpointConfig,
    /// Hard extraction limits.
    pub(crate) extraction: ExtractionConfig,
    /// `sd_journal_wait` timeout.
    pub(crate) wait_timeout: Duration,
    /// Drain deadline budget.
    pub(crate) drain_timeout: Duration,
}

impl TryFrom<Config> for RuntimeConfig {
    type Error = otap_df_config::error::Error;

    fn try_from(config: Config) -> Result<Self, Self::Error> {
        let Config {
            source_id,
            journal,
            units,
            identifiers,
            priorities,
            max_priority,
            start_at,
            batch,
            checkpoint,
            extraction,
            wait_timeout,
            drain_timeout,
        } = config;

        let source_id = validate_source_id(source_id)?;
        let journal = validate_journal(journal)?;
        let units = dedup_non_empty(units, "units")?;
        let identifiers = dedup_non_empty(identifiers, "identifiers")?;
        let priorities = resolve_priorities(priorities, max_priority)?;

        if batch.max_records == 0 {
            return Err(invalid("batch.max_records must be greater than zero"));
        }
        if batch.max_records > u16::MAX as usize {
            return Err(invalid("batch.max_records must be <= u16::MAX"));
        }
        if batch.max_flush_period.is_zero() {
            return Err(invalid("batch.max_flush_period must be greater than zero"));
        }
        if checkpoint.max_in_flight_batches == 0 {
            return Err(invalid(
                "checkpoint.max_in_flight_batches must be greater than zero",
            ));
        }
        if checkpoint.max_in_flight_batches != DEFAULT_MAX_IN_FLIGHT_BATCHES {
            return Err(invalid(
                "checkpoint.max_in_flight_batches must be 1 for journald v1",
            ));
        }
        if checkpoint.max_consecutive_failures == 0 {
            return Err(invalid(
                "checkpoint.max_consecutive_failures must be greater than zero",
            ));
        }
        validate_extraction(&extraction)?;
        if wait_timeout.is_zero() {
            return Err(invalid("wait_timeout must be greater than zero"));
        }
        if wait_timeout > MAX_WAIT_TIMEOUT {
            return Err(invalid("wait_timeout must be <= 5s"));
        }
        if drain_timeout <= wait_timeout {
            return Err(invalid("drain_timeout must be greater than wait_timeout"));
        }
        Ok(Self {
            source_id,
            journal,
            units,
            identifiers,
            priorities,
            start_at,
            batch,
            checkpoint,
            extraction,
            wait_timeout,
            drain_timeout,
        })
    }
}

fn invalid(msg: &str) -> otap_df_config::error::Error {
    otap_df_config::error::Error::InvalidUserConfig {
        error: msg.to_owned(),
    }
}

fn deserialize_usize_bytes<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let value = otap_df_config::byte_units::deserialize_u64(deserializer)?
        .ok_or_else(|| D::Error::custom("byte size value must not be null"))?;
    usize::try_from(value)
        .map_err(|_| D::Error::custom(format!("byte size {value} exceeds usize::MAX")))
}

fn validate_source_id(source_id: String) -> Result<String, otap_df_config::error::Error> {
    if source_id.is_empty() {
        return Err(invalid("source_id must not be empty"));
    }
    // Stable identifier requirements: ASCII, no path separators or whitespace,
    // so the on-disk checkpoint path stays trivially safe.
    if !source_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(invalid(
            "source_id must contain only ASCII alphanumerics, '_', '-', or '.'",
        ));
    }
    Ok(source_id)
}

fn validate_journal(
    mut journal: JournalConfig,
) -> Result<JournalConfig, otap_df_config::error::Error> {
    if let Some(namespace) = &journal.namespace {
        if namespace.is_empty() {
            return Err(invalid("journal.namespace must not be empty when set"));
        }
        return Err(invalid(
            "journal.namespace support is not implemented for journald v1; use null",
        ));
    }
    if journal.root_path.as_os_str().is_empty() {
        return Err(invalid("journal.root_path must not be empty"));
    }
    if journal.root_path.as_os_str().to_str().is_none() {
        return Err(invalid("journal.root_path must be valid UTF-8"));
    }
    let root_path_text = journal
        .root_path
        .as_os_str()
        .to_str()
        .expect("journal.root_path UTF-8 checked above");
    if !journal.root_path.is_absolute() && !root_path_text.starts_with('/') {
        return Err(invalid(&format!(
            "journal.root_path must be absolute: {}",
            journal.root_path.display()
        )));
    }
    if journal
        .root_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid("journal.root_path must not contain '..'"));
    }
    journal.root_path = normalize_path(&journal.root_path);
    Ok(journal)
}

pub(crate) fn journal_source_lease_key(journal: &JournalConfig) -> String {
    format!(
        "journald:root={}:namespace={}",
        stable_path_key(&journal.root_path),
        journal.namespace.as_deref().unwrap_or("<default>")
    )
}

fn stable_path_key(path: &Path) -> String {
    let normalized = normalize_path(path);
    let mut parts = Vec::new();
    let mut absolute = false;
    for component in normalized.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    let joined = parts.join("/");
    if absolute {
        if joined.is_empty() {
            "/".to_owned()
        } else {
            format!("/{joined}")
        }
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            _ => normalized.push(component.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn validate_extraction(extraction: &ExtractionConfig) -> Result<(), otap_df_config::error::Error> {
    if extraction.max_entry_bytes == 0 {
        return Err(invalid(
            "extraction.max_entry_bytes must be greater than zero",
        ));
    }
    if extraction.max_field_bytes == 0 {
        return Err(invalid(
            "extraction.max_field_bytes must be greater than zero",
        ));
    }
    if extraction.max_fields_per_entry == 0 {
        return Err(invalid(
            "extraction.max_fields_per_entry must be greater than zero",
        ));
    }
    if extraction.max_field_bytes > extraction.max_entry_bytes {
        return Err(invalid(
            "extraction.max_field_bytes must be <= extraction.max_entry_bytes",
        ));
    }
    Ok(())
}

fn dedup_non_empty(
    values: Vec<String>,
    field: &str,
) -> Result<Vec<String>, otap_df_config::error::Error> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        if value.is_empty() {
            return Err(invalid(&format!("{field} entries must not be empty")));
        }
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    Ok(out)
}

fn resolve_priorities(
    priorities: Option<Vec<u8>>,
    max_priority: Option<MaxPriority>,
) -> Result<Vec<u8>, otap_df_config::error::Error> {
    if let Some(max) = max_priority {
        if priorities.is_some() {
            return Err(invalid(
                "priorities and max_priority are mutually exclusive; specify one",
            ));
        }
        return Ok((0..=max.level()).collect());
    }

    let priorities = priorities.unwrap_or_else(default_priorities);
    for p in &priorities {
        if *p > 7 {
            return Err(invalid("priorities entries must be in the range 0..=7"));
        }
    }
    let set: BTreeSet<u8> = priorities.into_iter().collect();
    if set.is_empty() {
        return Err(invalid("priorities must contain at least one value"));
    }
    Ok(set.into_iter().collect())
}

/// Maps a journald `PRIORITY` value (0..=7) to an OTel severity number.
///
/// Mapping follows the design table:
/// emergency/alert/critical -> FATAL4/3/2, error -> ERROR (17), warning -> WARN
/// (13), notice -> INFO2 (10), info -> INFO (9), debug -> DEBUG (5).
#[must_use]
pub fn severity_number_from_priority(priority: u8) -> Option<u8> {
    Some(match priority {
        0 => 24, // FATAL4
        1 => 23, // FATAL3
        2 => 22, // FATAL2
        3 => 17, // ERROR
        4 => 13, // WARN
        5 => 10, // INFO2
        6 => 9,  // INFO
        7 => 5,  // DEBUG
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config::default()
    }

    #[test]
    fn defaults_round_trip_into_runtime_config() {
        let runtime = RuntimeConfig::try_from(base()).expect("default config must validate");
        assert_eq!(runtime.source_id, DEFAULT_SOURCE_ID);
        assert_eq!(runtime.journal.namespace, None);
        assert_eq!(runtime.journal.root_path, PathBuf::from("/"));
        assert_eq!(runtime.priorities, (0..=7).collect::<Vec<u8>>());
        assert_eq!(runtime.start_at, StartAt::End);
        assert_eq!(runtime.batch.max_records, DEFAULT_BATCH_MAX_RECORDS);
    }

    #[test]
    fn rejects_empty_source_id() {
        let cfg = Config {
            source_id: String::new(),
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_source_id_with_path_separator() {
        let cfg = Config {
            source_id: "system/etc".to_owned(),
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_named_namespace_until_systemd_namespace_open_is_supported() {
        let cfg = Config {
            journal: JournalConfig {
                namespace: Some("audit".to_owned()),
                ..JournalConfig::default()
            },
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_priorities_out_of_range() {
        let cfg = Config {
            priorities: Some(vec![0, 8]),
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_empty_explicit_priorities() {
        let cfg = Config {
            priorities: Some(vec![]),
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn dedups_and_sorts_priorities() {
        let cfg = Config {
            priorities: Some(vec![3, 1, 3, 0]),
            ..base()
        };
        let runtime = RuntimeConfig::try_from(cfg).expect("must validate");
        assert_eq!(runtime.priorities, vec![0, 1, 3]);
    }

    #[test]
    fn max_priority_expands_to_inclusive_range() {
        let cfg = Config {
            max_priority: Some(MaxPriority::Warning),
            ..base()
        };
        let runtime = RuntimeConfig::try_from(cfg).expect("must validate");
        assert_eq!(runtime.priorities, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn max_priority_conflicts_with_explicit_priorities() {
        let cfg = Config {
            priorities: Some(default_priorities()),
            max_priority: Some(MaxPriority::Warning),
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn accepts_and_normalizes_journal_root_path() {
        let cfg = Config {
            journal: JournalConfig {
                root_path: PathBuf::from("/host/./"),
                ..JournalConfig::default()
            },
            ..base()
        };
        let runtime = RuntimeConfig::try_from(cfg).expect("must validate");
        assert_eq!(runtime.journal.root_path, PathBuf::from("/host"));
        assert_eq!(
            journal_source_lease_key(&runtime.journal),
            "journald:root=/host:namespace=<default>"
        );
    }

    #[test]
    fn lease_key_uses_stable_path_separators() {
        let journal = JournalConfig {
            root_path: PathBuf::from("/host/journal"),
            ..JournalConfig::default()
        };
        assert_eq!(
            journal_source_lease_key(&journal),
            "journald:root=/host/journal:namespace=<default>"
        );
    }

    #[test]
    fn rejects_relative_journal_root_path() {
        let cfg = Config {
            journal: JournalConfig {
                root_path: PathBuf::from("host"),
                ..JournalConfig::default()
            },
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_parent_dir_in_journal_root_path() {
        let cfg = Config {
            journal: JournalConfig {
                root_path: PathBuf::from("../host"),
                ..JournalConfig::default()
            },
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_drain_timeout_not_greater_than_wait_timeout() {
        let cfg = Config {
            wait_timeout: Duration::from_secs(2),
            drain_timeout: Duration::from_secs(2),
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_zero_batch_records() {
        let cfg = Config {
            batch: BatchConfig {
                max_records: 0,
                ..BatchConfig::default()
            },
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_batch_records_above_log_id_capacity() {
        let cfg = Config {
            batch: BatchConfig {
                max_records: u16::MAX as usize + 1,
                ..BatchConfig::default()
            },
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_wait_timeout_above_shutdown_responsiveness_ceiling() {
        let cfg = Config {
            wait_timeout: Duration::from_secs(6),
            drain_timeout: Duration::from_secs(7),
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_multiple_in_flight_batches_for_v1() {
        let cfg = Config {
            checkpoint: CheckpointConfig {
                max_in_flight_batches: 2,
                ..CheckpointConfig::default()
            },
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn rejects_empty_unit_entry() {
        let cfg = Config {
            units: vec!["nginx.service".to_owned(), String::new()],
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn dedups_units_preserving_first_occurrence() {
        let cfg = Config {
            units: vec![
                "nginx.service".to_owned(),
                "ssh.service".to_owned(),
                "nginx.service".to_owned(),
            ],
            ..base()
        };
        let runtime = RuntimeConfig::try_from(cfg).expect("must validate");
        assert_eq!(
            runtime.units,
            vec!["nginx.service".to_owned(), "ssh.service".to_owned()]
        );
    }

    #[test]
    fn rejects_invalid_extraction_limits() {
        let cfg = Config {
            extraction: ExtractionConfig {
                max_entry_bytes: 10,
                max_field_bytes: 11,
                ..ExtractionConfig::default()
            },
            ..base()
        };
        assert!(RuntimeConfig::try_from(cfg).is_err());
    }

    #[test]
    fn parses_extraction_byte_units() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "extraction": {
                "max_entry_bytes": "1 MiB",
                "max_field_bytes": "256 KiB"
            }
        }))
        .expect("byte units must parse");
        let runtime = RuntimeConfig::try_from(cfg).expect("config must validate");
        assert_eq!(runtime.extraction.max_entry_bytes, 1024 * 1024);
        assert_eq!(runtime.extraction.max_field_bytes, 256 * 1024);
    }

    #[test]
    fn severity_mapping_matches_design_table() {
        assert_eq!(severity_number_from_priority(0), Some(24));
        assert_eq!(severity_number_from_priority(3), Some(17));
        assert_eq!(severity_number_from_priority(4), Some(13));
        assert_eq!(severity_number_from_priority(5), Some(10));
        assert_eq!(severity_number_from_priority(6), Some(9));
        assert_eq!(severity_number_from_priority(7), Some(5));
        assert_eq!(severity_number_from_priority(8), None);
    }
}
