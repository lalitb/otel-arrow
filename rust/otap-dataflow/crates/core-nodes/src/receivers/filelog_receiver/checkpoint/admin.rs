// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Exclusive, non-mutating administration for one existing checkpoint
//! namespace.
//!
//! Opening a session validates the exact version-1 namespace path and raw ID,
//! requires an existing namespace, ownership lock, and `CURRENT`, acquires the
//! runtime's exclusive operating-system lock, and reuses the store's bounded
//! snapshot/WAL recovery decoder without its repair steps. The session never
//! creates source artifacts, changes permissions, truncates a torn WAL tail,
//! adopts markers, publishes generations, or opens a WAL for append.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::current_marker::decode_current_marker;
use super::error::EncodeError;
use super::namespace::{CheckpointNamespace, CheckpointNamespaceError};
use super::primitives::{AdvisoryPath, AdvisoryPathKind, FramingResume, LifecycleState};
use super::store::error::StoreError;
use super::store::fsio;
use super::store::layout::{
    self, ArtifactForm, CURRENT_FILE_NAME, MAX_GENERATIONS_ON_DISK, MAX_TEMP_FILES,
    NamespaceArtifactKind, canonical_artifact_name_ignoring_ascii_case,
    classify_namespace_artifact, snapshot_file_name, wal_file_name,
};
use super::store::limits::StoreLimits;
use super::store::lock::NamespaceLock;
use super::store::{CheckpointStore, MARKER_READ_MAX_BYTES, StoreOptions};

/// File name of the machine-readable evidence-backup manifest.
pub const EVIDENCE_BACKUP_MANIFEST_FILE_NAME: &str = "manifest.json";
/// Version of the evidence-backup manifest schema.
pub const EVIDENCE_BACKUP_MANIFEST_VERSION: u16 = 1;

/// A read-only administration or evidence-backup failure.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointAdminError {
    /// The raw checkpoint namespace ID violated the shared namespace
    /// contract.
    #[error(transparent)]
    Namespace(#[from] CheckpointNamespaceError),
    /// The supplied namespace path did not use the exact version-1 derived
    /// suffix for its raw ID.
    #[error(
        "checkpoint administration path {path} does not end with the required derived suffix \
         {expected_suffix}"
    )]
    NamespacePathMismatch {
        /// Supplied namespace directory.
        path: PathBuf,
        /// Required `filelog/@v1/<checkpoint-id-hex>` suffix.
        expected_suffix: PathBuf,
    },
    /// A bounded store validation or filesystem safety check failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A required already-published checkpoint artifact was absent.
    #[error("checkpoint administration requires existing {artifact} at {path}")]
    RequiredArtifactMissing {
        /// Required artifact role.
        artifact: &'static str,
        /// Expected artifact path.
        path: PathBuf,
    },
    /// A decoded quarantined row lacked its required immutable evidence.
    #[error("quarantined checkpoint file {file_id} has no quarantine evidence")]
    MissingQuarantineEvidence {
        /// Lowercase-hex file ID.
        file_id: String,
    },
    /// A bounded count could not be represented in the stable report
    /// schema.
    #[error("checkpoint {field} count does not fit u64")]
    CountOverflow {
        /// Report field that overflowed.
        field: &'static str,
    },
    /// The requested backup destination already existed.
    #[error("checkpoint evidence-backup destination already exists: {path}")]
    BackupDestinationExists {
        /// Existing path that was refused.
        path: PathBuf,
    },
    /// The requested backup destination was inside the source namespace.
    #[error(
        "checkpoint evidence-backup destination {destination} must not be inside source namespace \
         {source_namespace}"
    )]
    BackupDestinationInsideNamespace {
        /// Source namespace held by this session.
        source_namespace: PathBuf,
        /// Refused destination.
        destination: PathBuf,
    },
    /// A recognized source artifact disappeared after the locked inventory.
    #[error("recognized checkpoint backup artifact disappeared before it could be copied: {path}")]
    BackupArtifactDisappeared {
        /// Source artifact path.
        path: PathBuf,
    },
    /// A destination-side filesystem operation failed.
    #[error("failed to {operation} at {path}: {source}")]
    BackupIo {
        /// Backup step that failed.
        operation: &'static str,
        /// Destination or source directory involved.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The bounded backup manifest could not be serialized.
    #[error("failed to serialize checkpoint evidence-backup manifest: {source}")]
    ManifestEncode {
        /// JSON serialization failure.
        #[source]
        source: serde_json::Error,
    },
    /// A native path could not be converted into its bounded report form.
    #[error("failed to encode native checkpoint path {path}: {source}")]
    NativePathEncode {
        /// Internal path that could not be represented.
        path: PathBuf,
        /// Native-path evidence encoding failure.
        #[source]
        source: EncodeError,
    },
    /// The target does not expose one of the supported native path
    /// representations.
    #[error("native checkpoint path reporting is unsupported on this platform: {path}")]
    NativePathUnsupported {
        /// Internal path that could not be represented.
        path: PathBuf,
    },
    /// A recognized artifact used ASCII case that did not exactly match the
    /// checkpoint format's canonical spelling.
    #[error(
        "checkpoint artifact {path} is not canonically named; expected file name \
         {canonical_name}"
    )]
    NonCanonicalArtifactName {
        /// Noncanonical source artifact path.
        path: PathBuf,
        /// Required byte-for-byte canonical ASCII file name.
        canonical_name: String,
    },
}

/// Native path encoding used by bounded administration reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePathKindReport {
    /// Native Unix `OsStr` bytes.
    UnixBytes,
    /// Native Windows UTF-16 code units serialized little-endian.
    WindowsUtf16Le,
}

/// Bounded native path included in serializable administration reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePathReport {
    /// Native representation used for the path.
    pub kind: NativePathKindReport,
    /// Whether only the bounded suffix is stored.
    pub truncated: bool,
    /// Full native representation length in bytes.
    pub full_path_len: u64,
    /// Lowercase-hex stored bytes, bounded by the checkpoint path limit.
    pub stored_path_hex: String,
    /// Lowercase-hex digest of the complete native path representation.
    pub full_path_digest: String,
}

/// Bounded validation summary for one authoritative checkpoint generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceValidationReport {
    /// Exact raw `checkpoint.id`.
    pub namespace_id: String,
    /// Bounded native representation of the canonical version-1 namespace
    /// path.
    pub derived_namespace_path: NativePathReport,
    /// Generation selected by the valid existing `CURRENT`.
    pub selected_generation: u64,
    /// Records decoded from the authoritative snapshot.
    pub snapshot_record_count: u64,
    /// Complete authoritative WAL transactions replayed.
    pub wal_transaction_count: u64,
    /// Records present after WAL replay.
    pub tracked_file_count: u64,
    /// Quarantined records present after WAL replay.
    pub quarantine_count: u64,
    /// Structurally incomplete bytes in the allowed final WAL tail.
    pub torn_wal_tail_bytes: u64,
    /// Other recognized final generation numbers present in the namespace.
    pub retired_generations: Vec<u64>,
}

/// Serializable framing-resume evidence for a quarantined record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FramingResumeReport {
    /// No split logical record is in progress.
    Clean,
    /// A split logical record must resume from the recorded fragment index.
    Continuation {
        /// Original logical-record start offset.
        record_start_offset: u64,
        /// Known record end, or zero for scan-to-LF continuation.
        record_end_offset: u64,
        /// Next fragment index.
        next_fragment_index: u32,
    },
}

/// Serializable platform-neutral locator evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocatorReport {
    /// No locator was recorded.
    Unspecified,
    /// POSIX device and inode.
    PosixDevIno {
        /// Device identifier.
        dev: u64,
        /// Inode number.
        ino: u64,
    },
    /// Windows volume serial and 128-bit file ID.
    WindowsVolumeFileId {
        /// Volume serial number.
        volume_serial: u64,
        /// Lowercase-hex 128-bit file ID.
        file_id: String,
    },
}

/// Serializable advisory-path encoding kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisoryPathKindReport {
    /// No advisory path is available.
    Unavailable,
    /// Native Unix path bytes.
    UnixBytes,
    /// Native Windows UTF-16LE bytes.
    WindowsUtf16Le,
}

/// Bounded advisory-path evidence exposed by administration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisoryPathReport {
    /// Native path encoding kind.
    pub kind: AdvisoryPathKindReport,
    /// Whether only the bounded suffix is stored.
    pub truncated: bool,
    /// Full native representation length.
    pub full_path_len: u64,
    /// Lowercase-hex stored bytes, bounded by the checkpoint format.
    pub stored_path_hex: String,
    /// Lowercase-hex domain-separated digest of the complete native path.
    pub full_path_digest: String,
}

/// Bounded inspection report for one quarantined checkpoint record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineInspectionReport {
    /// Lowercase-hex opaque file ID.
    pub file_id: String,
    /// Current file epoch.
    pub epoch: u32,
    /// Ack-committed source-byte offset.
    pub committed_offset: u64,
    /// Durable framing-resume state.
    pub framing_resume: FramingResumeReport,
    /// Immutable runtime locator.
    pub locator: LocatorReport,
    /// Opaque quarantine reason code.
    pub reason_code: u16,
    /// File size observed when quarantine was recorded.
    pub observed_size: u64,
    /// Epoch recorded in immutable quarantine evidence.
    pub quarantine_epoch: u32,
    /// Quarantine timestamp in Unix nanoseconds.
    pub quarantine_time_unix_nano: u64,
    /// Bounded current advisory-path evidence.
    pub advisory_path: AdvisoryPathReport,
}

/// Complete read-only checkpoint inspection result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInspectionReport {
    /// Namespace validation and recovery summary.
    pub validation: NamespaceValidationReport,
    /// Quarantined rows ordered by lowercase-hex file ID.
    pub quarantines: Vec<QuarantineInspectionReport>,
}

/// Role of one copied evidence-backup artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceArtifactRole {
    /// Published `CURRENT`.
    Current,
    /// `CURRENT.tmp`.
    CurrentTemporary,
    /// `CURRENT.bak`.
    CurrentBackup,
    /// Published generation snapshot.
    Snapshot,
    /// Snapshot temporary.
    SnapshotTemporary,
    /// Snapshot replacement backup.
    SnapshotBackup,
    /// Published generation WAL.
    Wal,
    /// WAL temporary.
    WalTemporary,
    /// WAL replacement backup.
    WalBackup,
}

/// Manifest entry for one copied checkpoint artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifact {
    /// Recognized artifact role.
    pub role: EvidenceArtifactRole,
    /// Exact ASCII source file name.
    pub name: String,
    /// Generation encoded in snapshot/WAL names.
    pub generation: Option<u64>,
    /// Copied byte length.
    pub length: u64,
    /// Lowercase-hex SHA-256 over the copied bytes.
    pub sha256: String,
}

/// Machine-readable evidence-backup manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBackupManifest {
    /// Manifest schema version.
    pub manifest_version: u16,
    /// Exact raw `checkpoint.id`.
    pub namespace_id: String,
    /// Bounded native representation of the source namespace held under the
    /// retained exclusive lock.
    pub source_namespace: NativePathReport,
    /// Generation selected by `CURRENT`.
    pub selected_generation: u64,
    /// Copied recognized artifacts, ordered by file name.
    pub artifacts: Vec<EvidenceArtifact>,
    /// Validation summary, including any allowed torn WAL tail.
    pub validation: NamespaceValidationReport,
}

/// Exclusive read-only administration session for one existing namespace.
#[derive(Debug)]
pub struct CheckpointAdminSession {
    options: StoreOptions,
    limits: StoreLimits,
    namespace: fsio::DirectoryPathBinding,
    lock: NamespaceLock,
    inspection: CheckpointInspectionReport,
}

impl CheckpointAdminSession {
    /// Opens and boundedly validates an existing namespace without repairing
    /// or mutating any source artifact.
    pub fn open(mut options: StoreOptions) -> Result<Self, CheckpointAdminError> {
        let namespace_suffix =
            CheckpointNamespace::derive(Path::new(""), &options.namespace_id)?.into_directory();
        if !options.namespace_dir.ends_with(&namespace_suffix) {
            return Err(CheckpointAdminError::NamespacePathMismatch {
                path: options.namespace_dir.clone(),
                expected_suffix: namespace_suffix.clone(),
            });
        }
        let limits = options.limits()?;
        let namespace = fsio::DirectoryPathBinding::open_canonical(
            &options.namespace_dir,
            "resolve the existing checkpoint namespace directory",
        )?;
        options.namespace_dir = namespace.path().to_path_buf();
        if !options.namespace_dir.ends_with(&namespace_suffix) {
            return Err(CheckpointAdminError::NamespacePathMismatch {
                path: options.namespace_dir.clone(),
                expected_suffix: namespace_suffix,
            });
        }
        let lock = NamespaceLock::acquire_existing(
            &options.namespace_dir,
            options.ownership_timeout,
            options.ownership_retry_interval,
        )?;

        let marker_path = options.namespace_dir.join(CURRENT_FILE_NAME);
        let marker_bytes = with_verified_source(&namespace, &lock, || {
            fsio::read_file_bounded_read_only(
                &marker_path,
                "CURRENT marker",
                MARKER_READ_MAX_BYTES,
            )?
            .ok_or_else(|| CheckpointAdminError::RequiredArtifactMissing {
                artifact: "CURRENT marker",
                path: marker_path.clone(),
            })
        })?;
        let generation =
            decode_current_marker(&marker_bytes).map_err(|source| StoreError::Decode {
                artifact: "CURRENT marker",
                path: marker_path,
                source,
            })?;

        let loaded = with_verified_source(&namespace, &lock, || {
            Ok(CheckpointStore::load_generation_read_only(
                &options.namespace_dir,
                generation,
                &options.namespace_id,
                &limits,
                options.max_tracked_files,
                options.fingerprint_bytes,
            )?)
        })?;
        let generations = with_verified_source(&namespace, &lock, || {
            Ok(layout::scan_generations_read_only(&options.namespace_dir)?)
        })?;
        let retired_generations = generations
            .into_keys()
            .filter(|found| *found != generation)
            .collect();
        let quarantines = quarantine_reports(&loaded.table)?;
        let validation = NamespaceValidationReport {
            namespace_id: options.namespace_id.clone(),
            derived_namespace_path: native_path_report(&options.namespace_dir)?,
            selected_generation: generation,
            snapshot_record_count: report_count(loaded.snapshot_records, "snapshot record")?,
            wal_transaction_count: report_count(loaded.transactions_replayed, "WAL transaction")?,
            tracked_file_count: report_count(loaded.table.len(), "tracked file")?,
            quarantine_count: report_count(loaded.table.quarantined_len(), "quarantine")?,
            torn_wal_tail_bytes: report_count(loaded.torn_tail_bytes, "torn WAL tail byte")?,
            retired_generations,
        };
        verify_source_binding(&namespace, &lock)?;

        Ok(Self {
            options,
            limits,
            namespace,
            lock,
            inspection: CheckpointInspectionReport {
                validation,
                quarantines,
            },
        })
    }

    /// Namespace validation summary produced during session open.
    #[must_use]
    pub fn validation(&self) -> &NamespaceValidationReport {
        &self.inspection.validation
    }

    /// Complete bounded inspection report produced during session open.
    #[must_use]
    pub fn inspection(&self) -> &CheckpointInspectionReport {
        &self.inspection
    }

    /// Copies recognized bounded checkpoint artifacts to a new destination
    /// and writes a synced machine-readable manifest.
    ///
    /// The retained namespace lock remains held for the complete inventory,
    /// copy, hashing, and manifest sequence. `ownership.lock` and unrelated
    /// directory entries are never copied.
    ///
    /// The completed destination directory is synced before its canonical
    /// parent. On Windows, both directory syncs retain the existing
    /// documented no-op limitation because `std::fs` exposes no supported
    /// directory-sync operation there; every copied file is still synced.
    pub fn backup(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<EvidenceBackupManifest, CheckpointAdminError> {
        let sources = with_verified_source(&self.namespace, &self.lock, || {
            inventory_backup_artifacts(
                &self.options.namespace_dir,
                &self.limits,
                self.inspection.validation.selected_generation,
            )
        })?;
        let destination = with_verified_source(&self.namespace, &self.lock, || {
            PreparedBackupDestination::create(&self.options.namespace_dir, destination.as_ref())
        })?;

        let mut artifacts = Vec::with_capacity(sources.len());
        for source in sources {
            let source_path = self.options.namespace_dir.join(&source.name);
            let bytes = with_verified_source(&self.namespace, &self.lock, || {
                fsio::read_file_bounded_read_only(&source_path, source.artifact, source.max_bytes)?
                    .ok_or_else(|| CheckpointAdminError::BackupArtifactDisappeared {
                        path: source_path.clone(),
                    })
            })?;
            destination.write_file(&source.name, &bytes)?;
            artifacts.push(EvidenceArtifact {
                role: source.role,
                name: source.name,
                generation: source.generation,
                length: u64::try_from(bytes.len()).map_err(|_| {
                    CheckpointAdminError::CountOverflow {
                        field: "backup artifact byte",
                    }
                })?,
                sha256: hex::encode(Sha256::digest(&bytes)),
            });
        }

        verify_source_binding(&self.namespace, &self.lock)?;
        let manifest = EvidenceBackupManifest {
            manifest_version: EVIDENCE_BACKUP_MANIFEST_VERSION,
            namespace_id: self.options.namespace_id.clone(),
            source_namespace: self.inspection.validation.derived_namespace_path.clone(),
            selected_generation: self.inspection.validation.selected_generation,
            artifacts,
            validation: self.inspection.validation.clone(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|source| CheckpointAdminError::ManifestEncode { source })?;
        destination.write_file(EVIDENCE_BACKUP_MANIFEST_FILE_NAME, &manifest_bytes)?;
        sync_backup_directories(&destination)?;
        verify_source_binding(&self.namespace, &self.lock)?;
        Ok(manifest)
    }

    /// Releases the exclusive namespace lock and reports an unlock failure.
    pub fn release(self) -> Result<(), CheckpointAdminError> {
        self.lock.release().map_err(CheckpointAdminError::from)
    }
}

fn verify_source_binding(
    namespace: &fsio::DirectoryPathBinding,
    lock: &NamespaceLock,
) -> Result<(), CheckpointAdminError> {
    namespace.verify("verify the checkpoint namespace path binding")?;
    lock.verify_path_binding()?;
    namespace.verify("reverify the checkpoint namespace path binding")?;
    Ok(())
}

fn with_verified_source<T>(
    namespace: &fsio::DirectoryPathBinding,
    lock: &NamespaceLock,
    operation: impl FnOnce() -> Result<T, CheckpointAdminError>,
) -> Result<T, CheckpointAdminError> {
    verify_source_binding(namespace, lock)?;
    let result = operation();
    verify_source_binding(namespace, lock)?;
    result
}

fn native_path_report(path: &Path) -> Result<NativePathReport, CheckpointAdminError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let evidence =
            AdvisoryPath::from_unix_bytes(path.as_os_str().as_bytes()).map_err(|source| {
                CheckpointAdminError::NativePathEncode {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        Ok(NativePathReport {
            kind: NativePathKindReport::UnixBytes,
            truncated: evidence.is_truncated(),
            full_path_len: evidence.full_path_len(),
            stored_path_hex: hex::encode(evidence.stored_path_bytes()),
            full_path_digest: hex::encode(evidence.full_path_digest()),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;

        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        let evidence = AdvisoryPath::from_windows_utf16_units(&units).map_err(|source| {
            CheckpointAdminError::NativePathEncode {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Ok(NativePathReport {
            kind: NativePathKindReport::WindowsUtf16Le,
            truncated: evidence.is_truncated(),
            full_path_len: evidence.full_path_len(),
            stored_path_hex: hex::encode(evidence.stored_path_bytes()),
            full_path_digest: hex::encode(evidence.full_path_digest()),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(CheckpointAdminError::NativePathUnsupported {
            path: path.to_path_buf(),
        })
    }
}

#[derive(Debug)]
struct PreparedBackupDestination {
    parent: fsio::DirectoryPathBinding,
    directory: fsio::DirectoryPathBinding,
}

impl PreparedBackupDestination {
    fn create(source_namespace: &Path, requested: &Path) -> Result<Self, CheckpointAdminError> {
        let Some(file_name) = requested.file_name() else {
            let resolved = std::fs::canonicalize(requested).map_err(|source| {
                CheckpointAdminError::BackupIo {
                    operation: "resolve an existing checkpoint evidence-backup destination",
                    path: requested.to_path_buf(),
                    source,
                }
            })?;
            return Err(CheckpointAdminError::BackupDestinationExists { path: resolved });
        };
        let requested_parent = requested
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fsio::DirectoryPathBinding::open_canonical_resolving(
            requested_parent,
            "resolve the checkpoint evidence-backup destination parent",
        )?;
        let resolved = parent.path().join(file_name);
        if resolved.starts_with(source_namespace) {
            return Err(CheckpointAdminError::BackupDestinationInsideNamespace {
                source_namespace: source_namespace.to_path_buf(),
                destination: resolved,
            });
        }

        parent.verify("verify the checkpoint evidence-backup destination parent")?;
        let existing = std::fs::symlink_metadata(&resolved);
        parent.verify("reverify the checkpoint evidence-backup destination parent")?;
        match existing {
            Ok(_) => {
                return Err(CheckpointAdminError::BackupDestinationExists { path: resolved });
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CheckpointAdminError::BackupIo {
                    operation: "inspect the checkpoint evidence-backup destination",
                    path: resolved,
                    source,
                });
            }
        }

        parent.verify("verify the checkpoint evidence-backup parent before creation")?;
        create_backup_directory(&resolved)?;
        parent.verify("verify the checkpoint evidence-backup parent after creation")?;
        let directory = fsio::DirectoryPathBinding::open_canonical(
            &resolved,
            "open the new checkpoint evidence-backup destination",
        )?;
        parent.verify("reverify the checkpoint evidence-backup parent after opening")?;
        directory.verify("verify the new checkpoint evidence-backup destination")?;
        if directory.path().starts_with(source_namespace) {
            return Err(CheckpointAdminError::BackupDestinationInsideNamespace {
                source_namespace: source_namespace.to_path_buf(),
                destination: directory.path().to_path_buf(),
            });
        }
        Ok(Self { parent, directory })
    }

    fn verify(&self) -> Result<(), CheckpointAdminError> {
        self.parent
            .verify("verify the checkpoint evidence-backup destination parent")?;
        self.directory
            .verify("verify the checkpoint evidence-backup destination directory")?;
        self.parent
            .verify("reverify the checkpoint evidence-backup destination parent")?;
        Ok(())
    }

    fn write_file(&self, name: &str, bytes: &[u8]) -> Result<(), CheckpointAdminError> {
        self.verify()?;
        let result = write_backup_file(&self.directory.path().join(name), bytes);
        self.verify()?;
        result
    }
}

fn sync_backup_directories(
    destination: &PreparedBackupDestination,
) -> Result<(), CheckpointAdminError> {
    sync_backup_directories_with(destination, |directory, operation| {
        directory
            .sync(operation)
            .map_err(CheckpointAdminError::from)
    })
}

fn sync_backup_directories_with(
    destination: &PreparedBackupDestination,
    mut sync: impl FnMut(&fsio::DirectoryPathBinding, &'static str) -> Result<(), CheckpointAdminError>,
) -> Result<(), CheckpointAdminError> {
    destination.verify()?;
    let destination_sync = sync(
        &destination.directory,
        "sync the completed checkpoint evidence-backup destination",
    );
    destination.verify()?;
    destination_sync?;

    let parent_sync = sync(
        &destination.parent,
        "sync the checkpoint evidence-backup destination parent",
    );
    destination.verify()?;
    parent_sync
}

fn report_count(value: usize, field: &'static str) -> Result<u64, CheckpointAdminError> {
    u64::try_from(value).map_err(|_| CheckpointAdminError::CountOverflow { field })
}

fn quarantine_reports(
    table: &super::apply::CheckpointTable,
) -> Result<Vec<QuarantineInspectionReport>, CheckpointAdminError> {
    let mut reports = Vec::with_capacity(table.quarantined_len());
    for (file_id, record) in table.iter() {
        if record.lifecycle_state != LifecycleState::Quarantined {
            continue;
        }
        let file_id = hex::encode(file_id.0);
        let evidence = record.quarantine_evidence.as_ref().ok_or_else(|| {
            CheckpointAdminError::MissingQuarantineEvidence {
                file_id: file_id.clone(),
            }
        })?;
        reports.push(QuarantineInspectionReport {
            file_id,
            epoch: record.file_epoch,
            committed_offset: record.committed_offset,
            framing_resume: record.framing_resume.into(),
            locator: record.locator.into(),
            reason_code: evidence.reason_code,
            observed_size: evidence.observed_size,
            quarantine_epoch: evidence.quarantine_epoch,
            quarantine_time_unix_nano: evidence.quarantine_time_unix_nano,
            advisory_path: (&record.advisory_path).into(),
        });
    }
    reports.sort_by(|left, right| left.file_id.cmp(&right.file_id));
    Ok(reports)
}

impl From<FramingResume> for FramingResumeReport {
    fn from(value: FramingResume) -> Self {
        match value {
            FramingResume::Clean => Self::Clean,
            FramingResume::Continuation {
                record_start_offset,
                record_end_offset,
                next_fragment_index,
            } => Self::Continuation {
                record_start_offset,
                record_end_offset,
                next_fragment_index,
            },
        }
    }
}

impl From<super::primitives::Locator> for LocatorReport {
    fn from(value: super::primitives::Locator) -> Self {
        match value {
            super::primitives::Locator::Unspecified => Self::Unspecified,
            super::primitives::Locator::PosixDevIno { dev, ino } => Self::PosixDevIno { dev, ino },
            super::primitives::Locator::WindowsVolumeFileId {
                volume_serial,
                file_id,
            } => Self::WindowsVolumeFileId {
                volume_serial,
                file_id: hex::encode(file_id),
            },
        }
    }
}

impl From<&AdvisoryPath> for AdvisoryPathReport {
    fn from(value: &AdvisoryPath) -> Self {
        let kind = match value.kind() {
            AdvisoryPathKind::Unavailable => AdvisoryPathKindReport::Unavailable,
            AdvisoryPathKind::UnixBytes => AdvisoryPathKindReport::UnixBytes,
            AdvisoryPathKind::WindowsUtf16Le => AdvisoryPathKindReport::WindowsUtf16Le,
        };
        Self {
            kind,
            truncated: value.is_truncated(),
            full_path_len: value.full_path_len(),
            stored_path_hex: hex::encode(value.stored_path_bytes()),
            full_path_digest: hex::encode(value.full_path_digest()),
        }
    }
}

#[derive(Debug)]
struct BackupSourceArtifact {
    name: String,
    role: EvidenceArtifactRole,
    generation: Option<u64>,
    artifact: &'static str,
    max_bytes: u64,
}

fn inventory_backup_artifacts(
    namespace_dir: &Path,
    limits: &StoreLimits,
    selected_generation: u64,
) -> Result<Vec<BackupSourceArtifact>, CheckpointAdminError> {
    let entries =
        std::fs::read_dir(namespace_dir).map_err(|source| CheckpointAdminError::BackupIo {
            operation: "list the checkpoint namespace for evidence backup",
            path: namespace_dir.to_path_buf(),
            source,
        })?;
    let mut sources = Vec::new();
    let mut temporary_count = 0usize;
    let mut final_generations = BTreeSet::new();
    let mut has_current = false;
    let mut has_selected_snapshot = false;
    let mut has_selected_wal = false;
    for entry in entries {
        let entry = entry.map_err(|source| CheckpointAdminError::BackupIo {
            operation: "read a checkpoint namespace entry for evidence backup",
            path: namespace_dir.to_path_buf(),
            source,
        })?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let classification = match classify_namespace_artifact(name) {
            Some(classification) => classification,
            None => {
                if let Some(canonical_name) = canonical_artifact_name_ignoring_ascii_case(name) {
                    return Err(CheckpointAdminError::NonCanonicalArtifactName {
                        path: namespace_dir.join(name),
                        canonical_name,
                    });
                }
                continue;
            }
        };
        if classification.kind == NamespaceArtifactKind::OwnershipLock {
            continue;
        }
        match (
            classification.kind,
            classification.form,
            classification.generation,
        ) {
            (NamespaceArtifactKind::Current, ArtifactForm::Final, None) => {
                has_current = true;
            }
            (NamespaceArtifactKind::Snapshot, ArtifactForm::Final, Some(generation))
                if generation == selected_generation =>
            {
                has_selected_snapshot = true;
            }
            (NamespaceArtifactKind::Wal, ArtifactForm::Final, Some(generation))
                if generation == selected_generation =>
            {
                has_selected_wal = true;
            }
            _ => {}
        }
        if classification.form != ArtifactForm::Final {
            if temporary_count >= MAX_TEMP_FILES {
                return Err(StoreError::TooManyTemporaryFiles {
                    dir: namespace_dir.to_path_buf(),
                    max: MAX_TEMP_FILES,
                }
                .into());
            }
            temporary_count += 1;
        }
        if classification.form == ArtifactForm::Final
            && let Some(generation) = classification.generation
            && final_generations.insert(generation)
            && final_generations.len() > MAX_GENERATIONS_ON_DISK
        {
            return Err(StoreError::TooManyGenerations {
                dir: namespace_dir.to_path_buf(),
                max: MAX_GENERATIONS_ON_DISK,
            }
            .into());
        }

        let (role, artifact, max_bytes) = match (classification.kind, classification.form) {
            (NamespaceArtifactKind::Current, ArtifactForm::Final) => (
                EvidenceArtifactRole::Current,
                "CURRENT marker",
                MARKER_READ_MAX_BYTES,
            ),
            (NamespaceArtifactKind::Current, ArtifactForm::Temporary) => (
                EvidenceArtifactRole::CurrentTemporary,
                "CURRENT temporary marker",
                MARKER_READ_MAX_BYTES,
            ),
            (NamespaceArtifactKind::Current, ArtifactForm::Backup) => (
                EvidenceArtifactRole::CurrentBackup,
                "CURRENT backup marker",
                MARKER_READ_MAX_BYTES,
            ),
            (NamespaceArtifactKind::Snapshot, ArtifactForm::Final) => (
                EvidenceArtifactRole::Snapshot,
                "snapshot",
                limits.max_snapshot_bytes,
            ),
            (NamespaceArtifactKind::Snapshot, ArtifactForm::Temporary) => (
                EvidenceArtifactRole::SnapshotTemporary,
                "snapshot temporary",
                limits.max_snapshot_bytes,
            ),
            (NamespaceArtifactKind::Snapshot, ArtifactForm::Backup) => (
                EvidenceArtifactRole::SnapshotBackup,
                "snapshot backup",
                limits.max_snapshot_bytes,
            ),
            (NamespaceArtifactKind::Wal, ArtifactForm::Final) => {
                (EvidenceArtifactRole::Wal, "WAL", limits.max_wal_bytes)
            }
            (NamespaceArtifactKind::Wal, ArtifactForm::Temporary) => (
                EvidenceArtifactRole::WalTemporary,
                "WAL temporary",
                limits.max_wal_bytes,
            ),
            (NamespaceArtifactKind::Wal, ArtifactForm::Backup) => (
                EvidenceArtifactRole::WalBackup,
                "WAL backup",
                limits.max_wal_bytes,
            ),
            (NamespaceArtifactKind::OwnershipLock, _) => continue,
        };
        sources.push(BackupSourceArtifact {
            name: name.to_owned(),
            role,
            generation: classification.generation,
            artifact,
            max_bytes,
        });
    }
    if !has_current {
        return Err(CheckpointAdminError::RequiredArtifactMissing {
            artifact: "canonical CURRENT marker",
            path: namespace_dir.join(CURRENT_FILE_NAME),
        });
    }
    if !has_selected_snapshot {
        return Err(CheckpointAdminError::RequiredArtifactMissing {
            artifact: "selected canonical snapshot",
            path: namespace_dir.join(snapshot_file_name(selected_generation)),
        });
    }
    if !has_selected_wal {
        return Err(CheckpointAdminError::RequiredArtifactMissing {
            artifact: "selected canonical WAL",
            path: namespace_dir.join(wal_file_name(selected_generation)),
        });
    }
    sources.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sources)
}

fn create_backup_directory(path: &Path) -> Result<(), CheckpointAdminError> {
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let _ = builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CheckpointAdminError::BackupDestinationExists {
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(CheckpointAdminError::BackupIo {
            operation: "create the checkpoint evidence-backup destination",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_backup_file(path: &Path, bytes: &[u8]) -> Result<(), CheckpointAdminError> {
    let mut options = OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let _ = options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(path)
        .map_err(|source| CheckpointAdminError::BackupIo {
            operation: "create a checkpoint evidence-backup file",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| CheckpointAdminError::BackupIo {
            operation: "write a checkpoint evidence-backup file",
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| CheckpointAdminError::BackupIo {
            operation: "sync a checkpoint evidence-backup file",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests;
