// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use super::*;
use crate::receivers::filelog_receiver::checkpoint::current_marker::encode_current_marker;
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    CommittedFrontierGuard, FRAMING_PROFILE_VERSION, FileId, Locator, namespace_digest,
};
use crate::receivers::filelog_receiver::checkpoint::snapshot::{
    QuarantineEvidence, SnapshotRecord, encode_snapshot,
};
use crate::receivers::filelog_receiver::checkpoint::store::fault::FaultPoint;
use crate::receivers::filelog_receiver::checkpoint::store::layout::{
    CURRENT_COMPACT_TEMP_FILE_NAME, CURRENT_CREATE_TEMP_FILE_NAME, OWNERSHIP_LOCK_FILE_NAME,
    PublicationRole, snapshot_file_name, temp_file_name, wal_file_name,
};
use crate::receivers::filelog_receiver::checkpoint::wal::{
    QuarantineFile, RegisterFile, UpdateProgress, decode_wal, encode_wal,
};
use crate::receivers::filelog_receiver::identity::platform::open_locator_for_stability_check_cancellable;

fn options(state_dir: &Path, namespace_id: &str) -> StoreOptions {
    fs::create_dir_all(state_dir).unwrap();
    let mut options = StoreOptions::from_state_dir(state_dir, namespace_id).unwrap();
    options.ownership_timeout = Duration::from_millis(200);
    options.ownership_retry_interval = Duration::from_millis(10);
    options
}

fn direct_options(namespace_dir: PathBuf, namespace_id: &str) -> StoreOptions {
    let mut options = StoreOptions::new(namespace_dir, namespace_id.to_owned());
    options.ownership_timeout = Duration::from_millis(200);
    options.ownership_retry_interval = Duration::from_millis(10);
    options
}

fn guard(committed_offset: u64, byte: u8) -> CommittedFrontierGuard {
    let window_len = committed_offset.min(64) as usize;
    CommittedFrontierGuard::compute(committed_offset, &vec![byte; window_len]).unwrap()
}

fn registration(seed: u8, advisory_path: AdvisoryPath) -> RegisterFile {
    RegisterFile {
        file_id: FileId([seed; 16]),
        file_epoch: 1,
        committed_offset: 0,
        committed_frontier_guard: guard(0, seed),
        fingerprint: vec![seed; 8],
        ignored_header_bytes: 0,
        locator: Locator::PosixDevIno {
            dev: 7,
            ino: u64::from(seed),
        },
        framing_profile_version: FRAMING_PROFILE_VERSION,
        framing_profile_digest: [seed; 32],
        framing_resume: FramingResume::Clean,
        last_seen_time_unix_nano: 1_000,
        advisory_path,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilesystemState {
    directory_mode: Option<u32>,
    directory_modified: SystemTime,
    entries: Vec<EntryState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryState {
    name: OsString,
    kind: EntryKind,
    bytes: Vec<u8>,
    link_target: Option<PathBuf>,
    len: u64,
    mode: Option<u32>,
    modified: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

fn filesystem_state(directory: &Path) -> FilesystemState {
    let directory_metadata = fs::symlink_metadata(directory).unwrap();
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let file_type = metadata.file_type();
        let kind = if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        entries.push(EntryState {
            name: entry.file_name(),
            kind,
            bytes: if kind == EntryKind::File {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            },
            link_target: if kind == EntryKind::Symlink {
                Some(fs::read_link(&path).unwrap())
            } else {
                None
            },
            len: metadata.len(),
            mode: metadata_mode(&metadata),
            modified: metadata.modified().unwrap(),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    FilesystemState {
        directory_mode: metadata_mode(&directory_metadata),
        directory_modified: directory_metadata.modified().unwrap(),
        entries,
    }
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;

    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn metadata_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

fn directory_names(directory: &Path) -> BTreeSet<String> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn valid_backup_validation(manifest: &EvidenceBackupManifest) -> &NamespaceValidationReport {
    match &manifest.authority {
        NamespaceAuthorityReport::Valid { validation } => validation,
        NamespaceAuthorityReport::Invalid { failure } => {
            panic!("expected valid backup authority, got {failure:?}")
        }
    }
}

/// Scenario: administration constructs StoreOptions for mixed-case, dot,
/// and maximum-length raw checkpoint IDs.
/// Guarantees: the administration entry point uses the same exact lowercase
/// hex vectors and 127-byte boundary as runtime namespace validation.
#[test]
fn admin_store_options_use_shared_namespace_vectors() {
    let mixed = StoreOptions::from_state_dir("state", "AppLogs").unwrap();
    let lower = StoreOptions::from_state_dir("state", "applogs").unwrap();
    assert_eq!(
        mixed.namespace_dir,
        Path::new("state/filelog/@v1/4170704c6f6773")
    );
    assert_eq!(
        lower.namespace_dir,
        Path::new("state/filelog/@v1/6170706c6f6773")
    );
    assert_ne!(mixed.namespace_dir, lower.namespace_dir);
    assert_eq!(
        StoreOptions::from_state_dir("state", ".")
            .unwrap()
            .namespace_dir,
        Path::new("state/filelog/@v1/2e")
    );
    assert_eq!(
        StoreOptions::from_state_dir("state", "..")
            .unwrap()
            .namespace_dir,
        Path::new("state/filelog/@v1/2e2e")
    );
    assert_eq!(
        StoreOptions::from_state_dir("state", &"a".repeat(127))
            .unwrap()
            .namespace_dir
            .file_name()
            .unwrap()
            .len(),
        254
    );
    assert!(StoreOptions::from_state_dir("state", &"a".repeat(128)).is_err());
}

/// Scenario: a valid derived namespace and a populated direct-ID sibling
/// coexist below the filelog state root.
/// Guarantees: administration opens only the `@v1` lowercase-hex namespace
/// and never searches or adopts the older direct-ID sibling.
#[test]
fn populated_direct_id_sibling_is_ignored() {
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let derived = options(&state_dir, "AppLogs");
    let mut store = CheckpointStore::open(derived.clone()).unwrap();
    let _ = store
        .register_files(vec![registration(1, AdvisoryPath::unavailable())])
        .unwrap();
    drop(store);

    let direct_namespace = state_dir.join("filelog").join("AppLogs");
    let mut sibling =
        CheckpointStore::open(direct_options(direct_namespace.clone(), "AppLogs")).unwrap();
    let _ = sibling
        .register_files(vec![
            registration(2, AdvisoryPath::unavailable()),
            registration(3, AdvisoryPath::unavailable()),
        ])
        .unwrap();
    drop(sibling);
    assert!(matches!(
        CheckpointAdminSession::open(direct_options(direct_namespace.clone(), "AppLogs")),
        Err(CheckpointAdminError::NamespacePathMismatch { .. })
    ));

    let expected_namespace =
        native_path_report(&fs::canonicalize(&derived.namespace_dir).unwrap()).unwrap();
    let direct_namespace_report =
        native_path_report(&fs::canonicalize(&direct_namespace).unwrap()).unwrap();
    let session = CheckpointAdminSession::open(derived).unwrap();
    assert_eq!(session.validation().namespace_id, "AppLogs");
    assert_eq!(session.validation().tracked_file_count, 1);
    assert_eq!(
        session.validation().derived_namespace_path,
        expected_namespace
    );
    assert_ne!(
        session.validation().derived_namespace_path,
        direct_namespace_report
    );
}

/// Scenario: a runtime store owns the namespace, then an administration
/// session owns it while a second administration session attempts to open.
/// Guarantees: administration uses the same exclusive OS lock as runtime
/// stores and retains that lock for the complete session lifetime.
#[test]
fn administration_session_is_exclusive_with_runtime_and_other_sessions() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "exclusive-admin");
    let runtime = CheckpointStore::open(store_options.clone()).unwrap();
    assert!(matches!(
        CheckpointAdminSession::open(store_options.clone()),
        Err(CheckpointAdminError::Store(
            StoreError::NamespaceLocked { .. }
        ))
    ));
    drop(runtime);

    let session = CheckpointAdminSession::open(store_options.clone()).unwrap();
    assert!(matches!(
        CheckpointAdminSession::open(store_options.clone()),
        Err(CheckpointAdminError::Store(
            StoreError::NamespaceLocked { .. }
        ))
    ));
    session.release().unwrap();
    drop(CheckpointAdminSession::open(store_options).unwrap());
}

/// Scenario: an administration session is opened through a relative state
/// path, then the process working directory changes before backup.
/// Guarantees: the session uses its canonical source namespace and does not
/// rebind later reads to the same relative path under the new directory.
#[test]
fn relative_admin_session_survives_working_directory_change() {
    const CHILD_ENV: &str = "OTAP_FILELOG_ADMIN_CWD_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        struct RestoreCurrentDirectory(PathBuf);

        impl Drop for RestoreCurrentDirectory {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }

        let root = tempfile::tempdir().unwrap();
        let original = std::env::current_dir().unwrap();
        let _restore = RestoreCurrentDirectory(original);
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        std::env::set_current_dir(&first).unwrap();

        let relative_options = options(Path::new("state"), "relative-admin");
        let mut store = CheckpointStore::open(relative_options.clone()).unwrap();
        let _ = store
            .register_files(vec![registration(1, AdvisoryPath::unavailable())])
            .unwrap();
        drop(store);
        let session = CheckpointAdminSession::open(relative_options).unwrap();

        std::env::set_current_dir(&second).unwrap();
        let destination = root.path().join("evidence");
        let manifest = session.backup(&destination).unwrap();
        assert_eq!(valid_backup_validation(&manifest).tracked_file_count, 1);
        assert!(destination.join(CURRENT_FILE_NAME).is_file());
        assert!(!second.join("state").exists());
        session.release().unwrap();
        return;
    }

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("relative_admin_session_survives_working_directory_change")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .status()
        .unwrap();
    assert!(status.success(), "isolated cwd regression test failed");
}

/// Scenario: a locked source namespace is renamed away and a different valid
/// namespace is created at its canonical pathname before backup.
/// Guarantees: backup rejects the persistent directory/lock rebinding before
/// combining the original validation report with replacement artifacts.
#[cfg(unix)]
#[test]
fn backup_rejects_persistent_source_namespace_replacement() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "source-replacement");
    let namespace = store_options.namespace_dir.clone();
    let mut original = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = original
        .register_files(vec![registration(1, AdvisoryPath::unavailable())])
        .unwrap();
    drop(original);
    let session = CheckpointAdminSession::open(store_options.clone()).unwrap();

    let displaced = namespace.parent().unwrap().join("displaced-namespace");
    fs::rename(&namespace, &displaced).unwrap();
    let mut replacement = CheckpointStore::open(store_options).unwrap();
    let _ = replacement
        .register_files(vec![
            registration(2, AdvisoryPath::unavailable()),
            registration(3, AdvisoryPath::unavailable()),
        ])
        .unwrap();
    drop(replacement);
    let displaced_before = filesystem_state(&displaced);
    let replacement_before = filesystem_state(&namespace);
    let destination = root.path().join("evidence");

    assert!(matches!(
        session.backup(&destination),
        Err(CheckpointAdminError::Store(
            StoreError::UnsafeFilesystemObject { .. }
        ))
    ));
    assert!(!destination.exists());
    assert_eq!(filesystem_state(&displaced), displaced_before);
    assert_eq!(filesystem_state(&namespace), replacement_before);
    session.release().unwrap();
}

/// Scenario: an existing namespace has deliberately non-default Unix modes,
/// a recognized temporary artifact, and an unrelated entry.
/// Guarantees: validation, inspection, and backup preserve every source
/// entry, byte, mode, and modification time instead of chmodding, cleaning,
/// or rewriting state.
#[cfg(unix)]
#[test]
fn inspection_preserves_namespace_bytes_modes_and_mtimes() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "inspect-state");
    let namespace = store_options.namespace_dir.clone();
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store
        .register_files(vec![registration(1, AdvisoryPath::unavailable())])
        .unwrap();
    drop(store);

    fs::write(namespace.join("offsets-9.wal.tmp"), b"partial").unwrap();
    fs::write(namespace.join("unrelated.bin"), b"leave-me").unwrap();
    fs::set_permissions(&namespace, fs::Permissions::from_mode(0o750)).unwrap();
    for entry in fs::read_dir(&namespace).unwrap() {
        let path = entry.unwrap().path();
        fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    fs::set_permissions(
        namespace.join(OWNERSHIP_LOCK_FILE_NAME),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    let before = filesystem_state(&namespace);

    let session = CheckpointAdminSession::open(store_options).unwrap();
    let _ = serde_json::to_vec(session.inspection()).unwrap();
    let _ = session.backup(root.path().join("evidence")).unwrap();
    session.release().unwrap();

    assert_eq!(filesystem_state(&namespace), before);
}

/// Scenario: administration targets a missing namespace, a namespace whose
/// ownership lock was removed, and one whose `CURRENT` was removed while a
/// valid `CURRENT.tmp` remains.
/// Guarantees: none of the missing artifacts is created, repaired, adopted,
/// or published, and each existing namespace remains byte-identical.
#[test]
fn missing_namespace_lock_and_current_are_not_repaired() {
    let root = tempfile::tempdir().unwrap();

    let missing = options(&root.path().join("missing-state"), "missing");
    let missing_path = missing.namespace_dir.clone();
    assert!(CheckpointAdminSession::open(missing).is_err());
    assert!(!missing_path.exists());

    let missing_lock = options(&root.path().join("lock-state"), "missing-lock");
    drop(CheckpointStore::open(missing_lock.clone()).unwrap());
    fs::remove_file(missing_lock.namespace_dir.join(OWNERSHIP_LOCK_FILE_NAME)).unwrap();
    let before = filesystem_state(&missing_lock.namespace_dir);
    assert!(CheckpointAdminSession::open(missing_lock.clone()).is_err());
    assert_eq!(filesystem_state(&missing_lock.namespace_dir), before);
    assert!(
        !missing_lock
            .namespace_dir
            .join(OWNERSHIP_LOCK_FILE_NAME)
            .exists()
    );

    let missing_current = options(&root.path().join("marker-state"), "missing-current");
    drop(CheckpointStore::open(missing_current.clone()).unwrap());
    let current = missing_current.namespace_dir.join(CURRENT_FILE_NAME);
    let current_temp = missing_current
        .namespace_dir
        .join(CURRENT_CREATE_TEMP_FILE_NAME);
    let _ = fs::copy(&current, &current_temp).unwrap();
    fs::remove_file(&current).unwrap();
    let before = filesystem_state(&missing_current.namespace_dir);
    assert!(CheckpointAdminSession::open(missing_current.clone()).is_err());
    assert_eq!(filesystem_state(&missing_current.namespace_dir), before);
    assert!(!current.exists());
    assert!(current_temp.exists());
}

/// Scenario: an authoritative WAL ends with three bytes that cannot form a
/// complete transaction header.
/// Guarantees: inspection and backup manifest report the allowed torn final
/// tail while preserving its bytes and leaving the source WAL unchanged.
#[test]
fn torn_wal_tail_is_reported_without_truncation() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "torn-tail");
    let namespace = store_options.namespace_dir.clone();
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store
        .register_files(vec![registration(1, AdvisoryPath::unavailable())])
        .unwrap();
    let wal_path = namespace.join(wal_file_name(store.generation()));
    drop(store);

    let mut wal = OpenOptions::new().append(true).open(&wal_path).unwrap();
    wal.write_all(&[0x00, 0x00, 0x01]).unwrap();
    wal.sync_all().unwrap();
    drop(wal);
    let before = filesystem_state(&namespace);
    let wal_len = fs::metadata(&wal_path).unwrap().len();

    let session = CheckpointAdminSession::open(store_options).unwrap();
    assert_eq!(session.validation().torn_wal_tail_bytes, 3);
    assert_eq!(session.validation().wal_transaction_count, 1);
    let manifest = session.backup(root.path().join("torn-evidence")).unwrap();
    assert_eq!(valid_backup_validation(&manifest).torn_wal_tail_bytes, 3);
    assert_eq!(
        fs::read(root.path().join("torn-evidence").join(wal_file_name(0))).unwrap(),
        fs::read(&wal_path).unwrap()
    );
    session.release().unwrap();

    assert_eq!(fs::metadata(&wal_path).unwrap().len(), wal_len);
    assert_eq!(filesystem_state(&namespace), before);
}

/// Scenario: a file advances with continuation framing and is then
/// quarantined with exact locator, reason, size, epoch, time, and path
/// evidence.
/// Guarantees: bounded inspection reports every required field exactly and
/// round-trips through the serializable administration schema.
#[test]
fn inspection_reports_exact_quarantine_evidence() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "quarantine-report");
    let advisory_path = AdvisoryPath::from_unix_bytes(b"/var/log/app.log").unwrap();
    let locator = Locator::PosixDevIno { dev: 7, ino: 9 };
    let mut registration = registration(9, advisory_path.clone());
    registration.locator = locator;
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store.register_files(vec![registration]).unwrap();
    let _ = store
        .commit_progress(vec![UpdateProgress {
            file_id: FileId([9; 16]),
            expected_committed_offset: 0,
            expected_file_epoch: 1,
            new_committed_offset: 64,
            new_committed_frontier_guard: guard(64, 9),
            new_framing_resume: FramingResume::Continuation {
                record_start_offset: 8,
                record_end_offset: 100,
                next_fragment_index: 3,
            },
            new_last_seen_time_unix_nano: 2_000,
            finalize: false,
        }])
        .unwrap();
    let _ = store
        .quarantine_files(vec![QuarantineFile {
            file_id: FileId([9; 16]),
            expected_file_epoch: 1,
            reason_code: 0x0003,
            locator,
            observed_size: 91,
            quarantine_epoch: 1,
            quarantine_time_unix_nano: 12_345,
        }])
        .unwrap();
    drop(store);

    let session = CheckpointAdminSession::open(store_options).unwrap();
    let report = session.inspection();
    assert_eq!(report.validation.tracked_file_count, 1);
    assert_eq!(report.validation.quarantine_count, 1);
    assert_eq!(report.quarantines.len(), 1);
    let quarantine = &report.quarantines[0];
    assert_eq!(quarantine.file_id, hex::encode([9; 16]));
    assert_eq!(quarantine.epoch, 1);
    assert_eq!(quarantine.committed_offset, 64);
    assert_eq!(
        quarantine.framing_resume,
        FramingResumeReport::Continuation {
            record_start_offset: 8,
            record_end_offset: 100,
            next_fragment_index: 3,
        }
    );
    assert_eq!(
        quarantine.locator,
        LocatorReport::PosixDevIno { dev: 7, ino: 9 }
    );
    assert_eq!(quarantine.reason_code, 0x0003);
    assert_eq!(quarantine.observed_size, 91);
    assert_eq!(quarantine.quarantine_epoch, 1);
    assert_eq!(quarantine.quarantine_time_unix_nano, 12_345);
    assert_eq!(
        quarantine.advisory_path.kind,
        AdvisoryPathKindReport::UnixBytes
    );
    assert!(!quarantine.advisory_path.truncated);
    assert_eq!(quarantine.advisory_path.full_path_len, 16);
    assert_eq!(
        quarantine.advisory_path.stored_path_hex,
        hex::encode(b"/var/log/app.log")
    );
    assert_eq!(
        quarantine.advisory_path.full_path_digest,
        hex::encode(advisory_path.full_path_digest())
    );

    let encoded = serde_json::to_vec(report).unwrap();
    let decoded: CheckpointInspectionReport = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, *report);
}

/// Scenario: a valid checkpoint namespace lives below a Unix path component
/// containing a byte that is not valid UTF-8.
/// Guarantees: inspection and backup manifests serialize the canonical
/// native path as bounded tagged bytes without lossy PathBuf conversion.
#[cfg(unix)]
#[test]
fn manifests_encode_non_utf8_unix_source_paths() {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let root = tempfile::tempdir().unwrap();
    let state_component = OsString::from_vec(b"state-\xff".to_vec());
    let store_options = options(&root.path().join(state_component), "native-path");
    let namespace = store_options.namespace_dir.clone();
    drop(CheckpointStore::open(store_options.clone()).unwrap());

    let session = CheckpointAdminSession::open(store_options).unwrap();
    let canonical_namespace = fs::canonicalize(&namespace).unwrap();
    let report = &session.validation().derived_namespace_path;
    assert_eq!(report.kind, NativePathKindReport::UnixBytes);
    assert!(!report.truncated);
    assert_eq!(
        hex::decode(&report.stored_path_hex).unwrap(),
        canonical_namespace.as_os_str().as_bytes()
    );
    assert_eq!(
        report.full_path_len,
        canonical_namespace.as_os_str().as_bytes().len() as u64
    );
    let inspection_bytes = serde_json::to_vec(session.inspection()).unwrap();
    let inspection_json: serde_json::Value = serde_json::from_slice(&inspection_bytes).unwrap();
    assert_eq!(
        inspection_json["validation"]["derived_namespace_path"]["kind"],
        "unix_bytes"
    );
    let decoded_inspection: CheckpointInspectionReport =
        serde_json::from_slice(&inspection_bytes).unwrap();
    assert_eq!(decoded_inspection, *session.inspection());

    let destination = root.path().join("native-path-evidence");
    let manifest = session.backup(&destination).unwrap();
    assert_eq!(
        manifest.source_namespace,
        session.validation().derived_namespace_path
    );
    let manifest_bytes = fs::read(destination.join(EVIDENCE_BACKUP_MANIFEST_FILE_NAME)).unwrap();
    let decoded_manifest: EvidenceBackupManifest = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(decoded_manifest, manifest);
    session.release().unwrap();
}

/// Scenario: a compacted namespace contains active and retired generations,
/// marker and generation temporary forms, and unrelated
/// directory entries.
/// Guarantees: backup copies only recognized bounded artifacts, records exact
/// hashes and validation in its manifest, preserves the source, and refuses
/// to overwrite the completed destination.
#[test]
fn backup_copies_only_recognized_artifacts_with_manifest_hashes() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "backup-report");
    let namespace = store_options.namespace_dir.clone();
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store
        .register_files(vec![registration(1, AdvisoryPath::unavailable())])
        .unwrap();
    store.compact().unwrap();
    assert_eq!(store.generation(), 1);
    drop(store);

    let current = namespace.join(CURRENT_FILE_NAME);
    let _ = fs::copy(&current, namespace.join(CURRENT_COMPACT_TEMP_FILE_NAME)).unwrap();
    fs::write(
        namespace.join(temp_file_name(
            &snapshot_file_name(2),
            PublicationRole::Compact,
        )),
        b"partial-snapshot",
    )
    .unwrap();
    fs::write(
        namespace.join(temp_file_name(&wal_file_name(2), PublicationRole::Compact)),
        b"partial-wal",
    )
    .unwrap();
    fs::write(namespace.join("notes.txt"), b"unrelated").unwrap();
    fs::write(namespace.join("offsets-01.wal"), b"invalid-generation").unwrap();

    let source_before = filesystem_state(&namespace);
    let session = CheckpointAdminSession::open(store_options).unwrap();
    assert_eq!(session.validation().selected_generation, 1);
    assert_eq!(session.validation().retired_generations, vec![0]);
    let inside_destination = namespace.join("evidence");
    assert!(matches!(
        session.backup(&inside_destination),
        Err(CheckpointAdminError::BackupDestinationInsideNamespace { .. })
    ));
    assert!(!inside_destination.exists());
    assert_eq!(filesystem_state(&namespace), source_before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let namespace_alias = root.path().join("namespace-alias");
        symlink(&namespace, &namespace_alias).unwrap();
        assert!(matches!(
            session.backup(namespace_alias.join("evidence")),
            Err(CheckpointAdminError::BackupDestinationInsideNamespace { .. })
        ));
        assert!(!namespace.join("evidence").exists());
        assert_eq!(filesystem_state(&namespace), source_before);
    }

    let destination = root.path().join("evidence");
    let manifest = session.backup(&destination).unwrap();
    assert_eq!(filesystem_state(&namespace), source_before);

    let expected_artifacts: BTreeSet<String> = [
        CURRENT_FILE_NAME.to_owned(),
        CURRENT_COMPACT_TEMP_FILE_NAME.to_owned(),
        snapshot_file_name(0),
        wal_file_name(0),
        snapshot_file_name(1),
        wal_file_name(1),
        temp_file_name(&snapshot_file_name(2), PublicationRole::Compact),
        temp_file_name(&wal_file_name(2), PublicationRole::Compact),
    ]
    .into_iter()
    .collect();
    let manifest_names: BTreeSet<String> = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.name.clone())
        .collect();
    assert_eq!(manifest_names, expected_artifacts);

    let mut expected_destination_names = expected_artifacts.clone();
    let _ = expected_destination_names.insert(EVIDENCE_BACKUP_MANIFEST_FILE_NAME.to_owned());
    assert_eq!(directory_names(&destination), expected_destination_names);
    assert!(!destination.join(OWNERSHIP_LOCK_FILE_NAME).exists());
    assert!(!destination.join("notes.txt").exists());
    assert!(!destination.join("offsets-01.wal").exists());

    for artifact in &manifest.artifacts {
        let source_bytes = fs::read(namespace.join(&artifact.name)).unwrap();
        assert_eq!(
            fs::read(destination.join(&artifact.name)).unwrap(),
            source_bytes
        );
        assert_eq!(artifact.length, source_bytes.len() as u64);
        assert_eq!(artifact.sha256, hex::encode(Sha256::digest(&source_bytes)));
    }
    let role_for = |name: &str| {
        manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.name == name)
            .map(|artifact| (artifact.role, artifact.generation))
            .unwrap()
    };
    assert_eq!(
        role_for(CURRENT_FILE_NAME),
        (EvidenceArtifactRole::Current, None)
    );
    assert_eq!(
        role_for(CURRENT_COMPACT_TEMP_FILE_NAME),
        (EvidenceArtifactRole::CurrentTemporary, None)
    );
    assert_eq!(
        role_for(&snapshot_file_name(1)),
        (EvidenceArtifactRole::Snapshot, Some(1))
    );
    assert_eq!(
        role_for(&temp_file_name(
            &snapshot_file_name(2),
            PublicationRole::Compact
        )),
        (EvidenceArtifactRole::SnapshotTemporary, Some(2))
    );
    assert_eq!(
        role_for(&wal_file_name(1)),
        (EvidenceArtifactRole::Wal, Some(1))
    );
    assert_eq!(
        role_for(&temp_file_name(&wal_file_name(2), PublicationRole::Compact)),
        (EvidenceArtifactRole::WalTemporary, Some(2))
    );
    assert_eq!(manifest.namespace_id, "backup-report");
    assert_eq!(
        manifest.source_namespace,
        valid_backup_validation(&manifest).derived_namespace_path
    );
    assert_eq!(valid_backup_validation(&manifest).selected_generation, 1);
    assert_eq!(valid_backup_validation(&manifest).torn_wal_tail_bytes, 0);
    let manifest_from_disk: EvidenceBackupManifest = serde_json::from_slice(
        &fs::read(destination.join(EVIDENCE_BACKUP_MANIFEST_FILE_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest_from_disk, manifest);

    let destination_before = filesystem_state(&destination);
    assert!(matches!(
        session.backup(&destination),
        Err(CheckpointAdminError::BackupDestinationExists { .. })
    ));
    assert_eq!(filesystem_state(&destination), destination_before);
    assert_eq!(filesystem_state(&namespace), source_before);
}

/// Scenario: each authoritative artifact is case-renamed after validation
/// while the administration session retains the namespace lock.
/// Guarantees: backup rejects every case-insensitive recognized spelling
/// that is not byte-for-byte canonical instead of silently omitting it.
#[test]
fn backup_rejects_noncanonical_case_variants() {
    for (canonical_name, variant_name) in [
        (CURRENT_FILE_NAME.to_owned(), "current".to_owned()),
        (snapshot_file_name(0), "OFFSETS-0.SNAPSHOT".to_owned()),
        (wal_file_name(0), "Offsets-0.WAL".to_owned()),
    ] {
        let root = tempfile::tempdir().unwrap();
        let store_options = options(&root.path().join("state"), "case-variant");
        let namespace = store_options.namespace_dir.clone();
        drop(CheckpointStore::open(store_options.clone()).unwrap());
        let session = CheckpointAdminSession::open(store_options).unwrap();

        let canonical_path = namespace.join(&canonical_name);
        let intermediate = namespace.join("case-rename-intermediate");
        let variant_path = namespace.join(&variant_name);
        fs::rename(&canonical_path, &intermediate).unwrap();
        fs::rename(&intermediate, &variant_path).unwrap();
        let source_before = filesystem_state(&namespace);
        let destination = root.path().join("evidence");

        match session.backup(&destination).unwrap_err() {
            CheckpointAdminError::NonCanonicalArtifactName {
                path,
                canonical_name: expected,
            } => {
                assert_eq!(path, variant_path);
                assert_eq!(expected, canonical_name);
            }
            other => panic!("unexpected case-variant error: {other}"),
        }
        assert!(!destination.exists());
        assert_eq!(filesystem_state(&namespace), source_before);
        session.release().unwrap();
    }
}

/// Scenario: canonical CURRENT, the selected snapshot, or the selected WAL
/// disappears after session validation but before backup inventory.
/// Guarantees: no successful backup can omit an authoritative artifact and
/// the refused inventory neither creates a destination nor changes source.
#[test]
fn backup_requires_all_canonical_authoritative_artifacts() {
    for required_name in [
        CURRENT_FILE_NAME.to_owned(),
        snapshot_file_name(0),
        wal_file_name(0),
    ] {
        let root = tempfile::tempdir().unwrap();
        let store_options = options(&root.path().join("state"), "required-artifacts");
        let namespace = store_options.namespace_dir.clone();
        drop(CheckpointStore::open(store_options.clone()).unwrap());
        let session = CheckpointAdminSession::open(store_options).unwrap();

        fs::remove_file(namespace.join(&required_name)).unwrap();
        let source_before = filesystem_state(&namespace);
        let destination = root.path().join("evidence");
        match session.backup(&destination).unwrap_err() {
            CheckpointAdminError::RequiredArtifactMissing { path, .. } => {
                assert_eq!(path, namespace.join(&required_name));
            }
            other => panic!("unexpected missing-artifact error: {other}"),
        }
        assert!(!destination.exists());
        assert_eq!(filesystem_state(&namespace), source_before);
        session.release().unwrap();
    }
}

/// Scenario: the canonical destination parent or the newly created backup
/// directory is persistently replaced before a file write.
/// Guarantees: retained directory identities reject both substitutions and
/// no file is written through the rebound pathname.
#[cfg(unix)]
#[test]
fn prepared_backup_rejects_parent_and_directory_substitution() {
    for replace_parent in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let parent = root.path().join("backup-parent");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&parent).unwrap();
        let requested = parent.join("evidence");
        let prepared = PreparedBackupDestination::create(&source, &requested).unwrap();

        if replace_parent {
            let displaced_parent = root.path().join("displaced-parent");
            fs::rename(&parent, &displaced_parent).unwrap();
            fs::create_dir(&parent).unwrap();
            assert!(matches!(
                prepared.write_file("probe", b"must-not-write"),
                Err(CheckpointAdminError::Store(
                    StoreError::UnsafeFilesystemObject { .. }
                ))
            ));
            assert!(!displaced_parent.join("evidence").join("probe").exists());
            assert!(!parent.join("evidence").exists());
        } else {
            let displaced_directory = parent.join("displaced-evidence");
            fs::rename(&requested, &displaced_directory).unwrap();
            fs::create_dir(&requested).unwrap();
            assert!(matches!(
                prepared.write_file("probe", b"must-not-write"),
                Err(CheckpointAdminError::Store(
                    StoreError::UnsafeFilesystemObject { .. }
                ))
            ));
            assert!(!displaced_directory.join("probe").exists());
            assert!(!requested.join("probe").exists());
        }
    }
}

/// Scenario: a completed destination and its parent are passed through the
/// backup durability helper with an observable sync hook.
/// Guarantees: the destination directory is synced first and its canonical
/// parent is synced second.
#[test]
fn completed_backup_syncs_destination_then_parent() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let parent = root.path().join("backup-parent");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&parent).unwrap();
    let prepared = PreparedBackupDestination::create(&source, &parent.join("evidence")).unwrap();
    let mut synced = Vec::new();

    sync_backup_directories_with(&prepared, |directory, _operation| {
        synced.push(directory.path().to_path_buf());
        Ok(())
    })
    .unwrap();

    assert_eq!(
        synced,
        vec![
            prepared.directory.path().to_path_buf(),
            prepared.parent.path().to_path_buf(),
        ]
    );
}

/// Scenario: recognized backup candidates are respectively a symlink, a
/// FIFO with no writer, and an artifact one byte over its role-specific cap.
/// Guarantees: backup rejects every hostile object without blocking or
/// following it and leaves the complete source namespace byte-identical.
#[cfg(unix)]
#[test]
fn backup_rejects_symlink_fifo_and_oversized_artifacts_without_source_changes() {
    use std::os::unix::fs::symlink;

    for case in ["symlink", "fifo", "oversized"] {
        let root = tempfile::tempdir().unwrap();
        let namespace_id = format!("hostile-{case}");
        let store_options = options(&root.path().join("state"), &namespace_id);
        let namespace = store_options.namespace_dir.clone();
        drop(CheckpointStore::open(store_options.clone()).unwrap());

        let hostile_path = if case == "oversized" {
            namespace.join(CURRENT_COMPACT_TEMP_FILE_NAME)
        } else {
            namespace.join(temp_file_name(
                &snapshot_file_name(9),
                PublicationRole::Compact,
            ))
        };
        let victim = root.path().join("victim");
        match case {
            "symlink" => {
                fs::write(&victim, b"must-survive").unwrap();
                symlink(&victim, &hostile_path).unwrap();
            }
            "fifo" => make_fifo(&hostile_path),
            "oversized" => {
                fs::write(&hostile_path, vec![0u8; MARKER_READ_MAX_BYTES as usize + 1]).unwrap();
            }
            _ => unreachable!(),
        }

        let session = CheckpointAdminSession::open(store_options).unwrap();
        let source_before = filesystem_state(&namespace);
        assert!(
            session
                .backup(root.path().join(format!("evidence-{case}")))
                .is_err()
        );
        assert_eq!(
            filesystem_state(&namespace),
            source_before,
            "source changed for hostile {case}"
        );
        if case == "symlink" {
            assert_eq!(fs::read(&victim).unwrap(), b"must-survive");
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code, reason = "libc exposes no safe FIFO constructor")]
fn make_fifo(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let fifo_name = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo_name` is a live NUL-terminated path and `0o600` is a
    // valid FIFO permission bitmask.
    let result = unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) };
    assert_eq!(
        result,
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );
}

fn mutation_audit(reason: &str, action_time_unix_nano: u64) -> AuditMetadata {
    AuditMetadata {
        reason: reason.to_owned(),
        action_time_unix_nano,
    }
}

fn mutation_target(seed: u8, epoch: u32) -> QuarantinedFileTarget {
    QuarantinedFileTarget {
        file_id: hex::encode([seed; 16]),
        expected_lifecycle: ExpectedQuarantineState::Quarantined,
        expected_quarantine_epoch: epoch,
    }
}

fn source_locator(path: &Path) -> Locator {
    open_locator_for_stability_check_cancellable(path, false, || false)
        .unwrap()
        .unwrap()
}

fn seed_quarantined_source(
    store_options: &StoreOptions,
    source_path: &Path,
    seed: u8,
    committed_offset: u64,
    framing_resume: FramingResume,
    advisory_path: AdvisoryPath,
) -> SnapshotRecord {
    let locator = source_locator(source_path);
    let mut register = registration(seed, advisory_path);
    register.locator = locator;
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store.register_files(vec![register]).unwrap();
    if committed_offset != 0 {
        let _ = store
            .commit_progress(vec![UpdateProgress {
                file_id: FileId([seed; 16]),
                expected_committed_offset: 0,
                expected_file_epoch: 1,
                new_committed_offset: committed_offset,
                new_committed_frontier_guard: guard(committed_offset, seed),
                new_framing_resume: framing_resume,
                new_last_seen_time_unix_nano: 2_000,
                finalize: false,
            }])
            .unwrap();
    }
    let _ = store
        .quarantine_files(vec![QuarantineFile {
            file_id: FileId([seed; 16]),
            expected_file_epoch: 1,
            reason_code: 0x0003,
            locator,
            observed_size: fs::metadata(source_path).unwrap().len(),
            quarantine_epoch: 1,
            quarantine_time_unix_nano: 3_000,
        }])
        .unwrap();
    let record = store.table().get(&FileId([seed; 16])).unwrap().clone();
    drop(store);
    record
}

fn beginning_request(
    seed: u8,
    epoch: u32,
    source_path: &Path,
    follow_symlinks: bool,
    reason: &str,
) -> ResetToBeginningRequest {
    ResetToBeginningRequest {
        target: mutation_target(seed, epoch),
        source_path: source_path.to_path_buf(),
        follow_symlinks,
        audit: mutation_audit(reason, 4_000),
    }
}

fn keep_request(seed: u8, epoch: u32, reason: &str) -> KeepFailedRequest {
    KeepFailedRequest {
        target: mutation_target(seed, epoch),
        audit: mutation_audit(reason, 4_000),
    }
}

fn end_request(
    seed: u8,
    epoch: u32,
    source_path: &Path,
    follow_symlinks: bool,
    reason: &str,
) -> ResetToEndRequest {
    ResetToEndRequest {
        target: mutation_target(seed, epoch),
        source_path: source_path.to_path_buf(),
        follow_symlinks,
        audit: mutation_audit(reason, 4_000),
    }
}

#[derive(Clone, Copy, Debug)]
enum FaultedAdminAction {
    ResetToBeginning,
    ResetToEnd,
    KeepFailed,
    RemoveQuarantined,
}

impl FaultedAdminAction {
    const ALL: [Self; 4] = [
        Self::ResetToBeginning,
        Self::ResetToEnd,
        Self::KeepFailed,
        Self::RemoveQuarantined,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::ResetToBeginning => "reset-beginning",
            Self::ResetToEnd => "reset-end",
            Self::KeepFailed => "keep-failed",
            Self::RemoveQuarantined => "remove",
        }
    }

    const fn expected_result(self) -> QuarantineMutationAction {
        match self {
            Self::ResetToBeginning => QuarantineMutationAction::ResetToBeginning,
            Self::ResetToEnd => QuarantineMutationAction::ResetToEnd,
            Self::KeepFailed => QuarantineMutationAction::KeepFailed,
            Self::RemoveQuarantined => QuarantineMutationAction::RemoveQuarantined,
        }
    }
}

fn invoke_faulted_admin_action(
    session: &mut CheckpointAdminSession,
    action: FaultedAdminAction,
    seed: u8,
    source: &Path,
) -> Result<FileMutationResult, CheckpointAdminError> {
    let reason = format!("retry {} after injected fault", action.label());
    match action {
        FaultedAdminAction::ResetToBeginning => {
            session.reset_to_beginning(beginning_request(seed, 1, source, false, &reason))
        }
        FaultedAdminAction::ResetToEnd => {
            session.reset_to_end(end_request(seed, 1, source, false, &reason))
        }
        FaultedAdminAction::KeepFailed => session.keep_failed(keep_request(seed, 1, &reason)),
        FaultedAdminAction::RemoveQuarantined => {
            session.remove_quarantined(RemoveQuarantinedRequest {
                target: mutation_target(seed, 1),
                removal_reason: 0x0009,
                consequence: RemovalConsequence::AcknowledgeDuplicateOrLossPossible,
                audit: mutation_audit(&reason, 6_000),
            })
        }
    }
}

/// Scenario: a quarantined continuation is reset to the beginning through
/// the high-level administration API.
/// Guarantees: the API increments the epoch, installs offset zero, the empty
/// guard, Clean resume, and same-handle replacement fingerprint; reports
/// duplicate risk, syncs before success, retains the lock, and survives reopen.
#[test]
fn reset_to_beginning_is_audited_synced_and_durable() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    fs::write(&source, vec![b'a'; 128]).unwrap();
    let store_options = options(&root.path().join("state"), "reset-beginning");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        1,
        64,
        FramingResume::Continuation {
            record_start_offset: 8,
            record_end_offset: 96,
            next_fragment_index: 2,
        },
        AdvisoryPath::from_unix_bytes(b"/stale/source.log").unwrap(),
    );

    let mut session = CheckpointAdminSession::open(store_options.clone()).unwrap();
    let result = session
        .reset_to_beginning(beginning_request(
            1,
            1,
            &source,
            false,
            "replay this source",
        ))
        .unwrap();
    assert_eq!(result.namespace_id, "reset-beginning");
    assert_eq!(result.file_id, hex::encode([1; 16]));
    assert_eq!(result.action, QuarantineMutationAction::ResetToBeginning);
    assert_eq!(result.old_lifecycle, CheckpointLifecycleReport::Quarantined);
    assert_eq!(result.new_lifecycle, CheckpointLifecycleReport::Active);
    assert_eq!((result.old_epoch, result.new_epoch), (1, Some(2)));
    assert_eq!((result.old_offset, result.new_offset), (64, Some(0)));
    assert_eq!(result.data_effect, DataEffect::DuplicatePossible);
    assert!(result.reset_to_end_evidence.is_none());
    assert_eq!(
        serde_json::from_slice::<FileMutationResult>(&serde_json::to_vec(&result).unwrap())
            .unwrap(),
        result
    );
    assert!(matches!(
        CheckpointStore::open(store_options.clone()),
        Err(StoreError::NamespaceLocked { .. })
    ));
    session.release().unwrap();

    let reopened = CheckpointStore::open(store_options).unwrap();
    let record = reopened.table().get(&FileId([1; 16])).unwrap();
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
    assert_eq!(record.file_epoch, 2);
    assert_eq!(record.committed_offset, 0);
    assert_eq!(
        record.committed_frontier_guard,
        CommittedFrontierGuard::empty()
    );
    assert_eq!(record.framing_resume, FramingResume::Clean);
    assert_eq!(record.fingerprint, vec![b'a'; 128]);
    assert!(record.quarantine_evidence.is_none());
}

/// Scenario: reset-to-beginning is pointed at a different locator and a
/// missing path while the quarantined source still exists.
/// Guarantees: replacement fingerprint sampling never trusts advisory aliases,
/// and every source-validation failure leaves the WAL unchanged.
#[test]
fn reset_to_beginning_requires_the_exact_current_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    let wrong = root.path().join("wrong.log");
    fs::write(&source, b"source").unwrap();
    fs::write(&wrong, b"wrong").unwrap();
    let missing = root.path().join("missing.log");
    let store_options = options(&root.path().join("state"), "beginning-source");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        20,
        0,
        FramingResume::Clean,
        AdvisoryPath::unavailable(),
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();
    let mut session = CheckpointAdminSession::open(store_options).unwrap();

    assert!(matches!(
        session.reset_to_beginning(beginning_request(20, 1, &wrong, false, "wrong source",)),
        Err(CheckpointAdminError::ResetSourceLocatorMismatch { .. })
    ));
    assert!(matches!(
        session.reset_to_beginning(beginning_request(20, 1, &missing, false, "missing source",)),
        Err(CheckpointAdminError::ResetSourceIo { .. })
    ));
    assert_eq!(fs::read(wal_path).unwrap(), before);
}

/// Scenario: an operator records keep-failed for a quarantined record with
/// nonzero offset, continuation state, and immutable evidence.
/// Guarantees: the WAL gains a synced audit transaction while the complete
/// SnapshotRecord remains byte-identical both in the live session and after
/// reopening.
#[test]
fn keep_failed_preserves_all_record_state_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    fs::write(&source, vec![b'b'; 160]).unwrap();
    let store_options = options(&root.path().join("state"), "keep-failed");
    let old = seed_quarantined_source(
        &store_options,
        &source,
        2,
        80,
        FramingResume::Continuation {
            record_start_offset: 32,
            record_end_offset: 128,
            next_fragment_index: 4,
        },
        AdvisoryPath::from_unix_bytes(b"/var/log/keep.log").unwrap(),
    );

    let mut session = CheckpointAdminSession::open(store_options.clone()).unwrap();
    let transactions_before = session.validation().wal_transaction_count;
    let result = session
        .keep_failed(keep_request(2, 1, "retain quarantine"))
        .unwrap();
    assert_eq!(result.action, QuarantineMutationAction::KeepFailed);
    assert_eq!(result.old_lifecycle, CheckpointLifecycleReport::Quarantined);
    assert_eq!(result.new_lifecycle, CheckpointLifecycleReport::Quarantined);
    assert_eq!((result.old_epoch, result.new_epoch), (1, Some(1)));
    assert_eq!((result.old_offset, result.new_offset), (80, Some(80)));
    assert_eq!(result.data_effect, DataEffect::None);
    assert_eq!(
        session.validation().wal_transaction_count,
        transactions_before + 1
    );
    session.release().unwrap();

    let reopened = CheckpointStore::open(store_options).unwrap();
    assert_eq!(reopened.table().get(&FileId([2; 16])), Some(&old));
}

/// Scenario: a read-only administration session observes an allowed torn
/// WAL tail, repairs it while becoming writable, and then hits a no-write
/// injected failure on the requested audit transaction.
/// Guarantees: the live inspection is refreshed immediately after repair,
/// the definitive no-write leaves the session usable, and its next backup
/// describes and copies the repaired authority without a reopen.
#[test]
fn writable_transition_refreshes_repaired_torn_tail_before_append_failure() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    fs::write(&source, b"blocked\n").unwrap();
    let store_options = options(&root.path().join("state"), "repair-before-append");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        22,
        0,
        FramingResume::Clean,
        AdvisoryPath::unavailable(),
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let valid_len = fs::metadata(&wal_path).unwrap().len();
    let mut wal = OpenOptions::new().append(true).open(&wal_path).unwrap();
    wal.write_all(&[0x4f, 0x54, 0x41]).unwrap();
    wal.sync_all().unwrap();
    drop(wal);

    let mut session = CheckpointAdminSession::open_with_fault(
        store_options.clone(),
        FaultPoint::BeforeWalTransactionWrite,
    )
    .unwrap();
    assert_eq!(session.validation().torn_wal_tail_bytes, 3);
    assert!(matches!(
        session.keep_failed(keep_request(22, 1, "record failed retry")),
        Err(CheckpointAdminError::Store(StoreError::InjectedFault {
            point: FaultPoint::BeforeWalTransactionWrite,
        }))
    ));
    assert_eq!(session.validation().torn_wal_tail_bytes, 0);
    assert_eq!(fs::metadata(&wal_path).unwrap().len(), valid_len);

    let backup = root.path().join("repaired-evidence");
    let manifest = session.backup(&backup).unwrap();
    assert_eq!(valid_backup_validation(&manifest).torn_wal_tail_bytes, 0);
    assert_eq!(
        fs::read(backup.join(wal_file_name(0))).unwrap(),
        fs::read(&wal_path).unwrap()
    );
    session.release().unwrap();
}

/// Scenario: an audited keep-failed transaction is completely appended but
/// its first call observes an uncertain write result.
/// Guarantees: the same administration session can retry the exact request,
/// reconcile one transaction, and refresh authority without a duplicate
/// audit append.
#[test]
fn keep_failed_retries_an_exact_pending_append() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    fs::write(&source, b"blocked\n").unwrap();
    let store_options = options(&root.path().join("state"), "keep-failed-retry");
    let old = seed_quarantined_source(
        &store_options,
        &source,
        23,
        0,
        FramingResume::Clean,
        AdvisoryPath::unavailable(),
    );

    let mut session = CheckpointAdminSession::open_with_fault(
        store_options.clone(),
        FaultPoint::AfterWalTransactionWrite,
    )
    .unwrap();
    let transactions_before = session.validation().wal_transaction_count;
    assert!(matches!(
        session.keep_failed(keep_request(23, 1, "retain after retry")),
        Err(CheckpointAdminError::Store(StoreError::InjectedFault {
            point: FaultPoint::AfterWalTransactionWrite,
        }))
    ));
    let result = session
        .keep_failed(keep_request(23, 1, "retain after retry"))
        .unwrap();
    assert_eq!(result.action, QuarantineMutationAction::KeepFailed);
    assert_eq!(
        session.validation().wal_transaction_count,
        transactions_before + 1
    );
    session.release().unwrap();

    let reopened = CheckpointStore::open(store_options).unwrap();
    assert_eq!(reopened.table().get(&FileId([23; 16])), Some(&old));
    assert_eq!(
        reopened.recovery().transactions_replayed,
        usize::try_from(transactions_before + 1).unwrap()
    );
}

/// Scenario: an operator removes one exact quarantined record with matching
/// lifecycle, epoch, nonzero reason, namespace, and audit evidence.
/// Guarantees: explicit duplicate-or-loss acknowledgement is required, the
/// result reports both future-registration risks, and the record remains
/// absent after reopening.
#[test]
fn remove_quarantined_is_exact_audited_and_durable() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    fs::write(&source, b"remove me\n").unwrap();
    let store_options = options(&root.path().join("state"), "Remove.Quarantine");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        3,
        0,
        FramingResume::Clean,
        AdvisoryPath::from_unix_bytes(b"/var/log/remove.log").unwrap(),
    );

    let mut session = CheckpointAdminSession::open(store_options.clone()).unwrap();
    let request = RemoveQuarantinedRequest {
        target: mutation_target(3, 1),
        removal_reason: 0x0008,
        consequence: RemovalConsequence::AcknowledgeDuplicateOrLossPossible,
        audit: mutation_audit("delete blocked record", 5_000),
    };
    let mut missing_acknowledgement = serde_json::to_value(&request).unwrap();
    let _ = missing_acknowledgement
        .as_object_mut()
        .unwrap()
        .remove("consequence");
    assert!(serde_json::from_value::<RemoveQuarantinedRequest>(missing_acknowledgement).is_err());
    let result = session.remove_quarantined(request).unwrap();
    assert_eq!(result.action, QuarantineMutationAction::RemoveQuarantined);
    assert_eq!(result.old_lifecycle, CheckpointLifecycleReport::Quarantined);
    assert_eq!(result.new_lifecycle, CheckpointLifecycleReport::Absent);
    assert_eq!(result.new_epoch, None);
    assert_eq!(result.new_offset, None);
    assert_eq!(result.data_effect, DataEffect::DuplicateOrLossPossible);
    assert_eq!(session.validation().tracked_file_count, 0);
    let wal = decode_wal(
        &fs::read(store_options.namespace_dir.join(wal_file_name(0))).unwrap(),
        &namespace_digest("Remove.Quarantine"),
    )
    .unwrap();
    let removal = match &wal.transactions.last().unwrap().operations[0] {
        Operation::RemoveFile(removal) => removal,
        other => panic!("expected administrative removal, got {other:?}"),
    };
    assert_eq!(removal.namespace_id.as_deref(), Some("Remove.Quarantine"));
    assert_eq!(removal.expected_file_epoch, 1);
    assert_eq!(removal.expected_prior_state, LifecycleState::Quarantined);
    assert_eq!(
        removal.audit_reason.as_deref(),
        Some("delete blocked record")
    );
    session.release().unwrap();

    let reopened = CheckpointStore::open(store_options).unwrap();
    assert!(reopened.table().get(&FileId([3; 16])).is_none());
}

/// Scenario: reset-to-end samples a regular file longer than the 64-byte
/// committed-frontier window through its immutable quarantine locator.
/// Guarantees: fingerprint, EOF, and the digest of exactly the final 64 raw
/// bytes from the same handle are committed, reported without raw content,
/// synced, and recovered after restart.
#[test]
fn reset_to_end_commits_exact_locator_eof_and_real_guard() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    let bytes: Vec<u8> = (0..150).map(|value| value as u8).collect();
    fs::write(&source, &bytes).unwrap();
    let store_options = options(&root.path().join("state"), "reset-end");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        4,
        32,
        FramingResume::Clean,
        AdvisoryPath::from_unix_bytes(b"/obsolete/path.log").unwrap(),
    );
    let expected_guard = CommittedFrontierGuard::compute(150, &bytes[86..]).unwrap();

    let mut session = CheckpointAdminSession::open(store_options.clone()).unwrap();
    let result = session
        .reset_to_end(end_request(4, 1, &source, false, "skip blocked bytes"))
        .unwrap();
    assert_eq!(result.action, QuarantineMutationAction::ResetToEnd);
    assert_eq!(result.new_lifecycle, CheckpointLifecycleReport::Active);
    assert_eq!((result.old_epoch, result.new_epoch), (1, Some(2)));
    assert_eq!((result.old_offset, result.new_offset), (32, Some(150)));
    assert_eq!(result.data_effect, DataEffect::LossAccepted);
    let evidence = result.reset_to_end_evidence.as_ref().unwrap();
    assert_eq!(evidence.eof_offset, 150);
    assert_eq!(evidence.locator, source_locator(&source).into());
    assert_eq!(evidence.committed_frontier_guard.window_len, 64);
    assert_eq!(
        evidence.committed_frontier_guard.digest,
        hex::encode(expected_guard.digest)
    );
    let encoded = serde_json::to_vec(&result).unwrap();
    assert!(!encoded.windows(64).any(|window| window == &bytes[86..]));
    session.release().unwrap();

    let reopened = CheckpointStore::open(store_options).unwrap();
    let record = reopened.table().get(&FileId([4; 16])).unwrap();
    assert_eq!(record.file_epoch, 2);
    assert_eq!(record.committed_offset, 150);
    assert_eq!(record.committed_frontier_guard, expected_guard);
    assert_eq!(record.framing_resume, FramingResume::Clean);
    assert_eq!(record.fingerprint, bytes);
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
}

/// Scenario: each audited per-file action encounters every WAL write/sync
/// fault once, then the same high-level request is retried in the locked
/// administration session.
/// Guarantees: reset-to-beginning, reset-to-end, keep-failed, and removal each
/// publish exactly one durable transaction and recover their exact final state
/// without duplicating an uncertain append.
#[test]
fn every_per_file_admin_action_retries_every_wal_fault_exactly_once() {
    for (action_index, action) in FaultedAdminAction::ALL.into_iter().enumerate() {
        for point in FaultPoint::WAL_DURABILITY {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source.log");
            let source_bytes = vec![b'a' + u8::try_from(action_index).unwrap(); 128];
            fs::write(&source, &source_bytes).unwrap();
            let namespace_id = format!("{}-{}", action.label(), point.as_str());
            let store_options = options(&root.path().join("state"), &namespace_id);
            let seed = 40u8
                .checked_add(u8::try_from(action_index).unwrap())
                .unwrap();
            let old = seed_quarantined_source(
                &store_options,
                &source,
                seed,
                32,
                FramingResume::Clean,
                AdvisoryPath::unavailable(),
            );

            let mut session =
                CheckpointAdminSession::open_with_fault(store_options.clone(), point).unwrap();
            let transactions_before = session.validation().wal_transaction_count;
            let error = invoke_faulted_admin_action(&mut session, action, seed, &source)
                .expect_err("the armed administrative WAL boundary must fail once");
            assert!(
                matches!(
                    error,
                    CheckpointAdminError::Store(StoreError::InjectedFault { point: fired })
                        if fired == point
                ),
                "{action:?} at {point}: {error}"
            );

            let result = invoke_faulted_admin_action(&mut session, action, seed, &source)
                .unwrap_or_else(|error| {
                    panic!("exact retry for {action:?} at {point} failed: {error}")
                });
            assert_eq!(result.action, action.expected_result(), "{point}");
            assert_eq!(result.wal_sequence, transactions_before + 1, "{point}");
            assert_eq!(
                session.validation().wal_transaction_count,
                transactions_before + 1,
                "{point}"
            );
            session.release().unwrap();

            let reopened = CheckpointStore::open(store_options.clone()).unwrap();
            let record = reopened.table().get(&FileId([seed; 16]));
            match action {
                FaultedAdminAction::ResetToBeginning => {
                    let record = record.expect("reset-to-beginning keeps the record");
                    assert_eq!(record.lifecycle_state, LifecycleState::Active, "{point}");
                    assert_eq!(record.file_epoch, 2, "{point}");
                    assert_eq!(record.committed_offset, 0, "{point}");
                    assert_eq!(
                        record.committed_frontier_guard,
                        CommittedFrontierGuard::empty(),
                        "{point}"
                    );
                }
                FaultedAdminAction::ResetToEnd => {
                    let record = record.expect("reset-to-end keeps the record");
                    assert_eq!(record.lifecycle_state, LifecycleState::Active, "{point}");
                    assert_eq!(record.file_epoch, 2, "{point}");
                    assert_eq!(
                        record.committed_offset,
                        u64::try_from(source_bytes.len()).unwrap(),
                        "{point}"
                    );
                }
                FaultedAdminAction::KeepFailed => {
                    assert_eq!(record, Some(&old), "{point}");
                }
                FaultedAdminAction::RemoveQuarantined => {
                    assert!(record.is_none(), "{point}");
                }
            }
            let wal = decode_wal(
                &fs::read(store_options.namespace_dir.join(wal_file_name(0))).unwrap(),
                &namespace_digest(&namespace_id),
            )
            .unwrap();
            assert_eq!(
                u64::try_from(wal.transactions.len()).unwrap(),
                transactions_before + 1,
                "{action:?} at {point}"
            );
            let last = wal.transactions.last().unwrap();
            assert_eq!(last.operations.len(), 1, "{action:?} at {point}");
            match (action, &last.operations[0]) {
                (
                    FaultedAdminAction::ResetToBeginning,
                    Operation::ResetQuarantinedFile(operation),
                ) => assert_eq!(
                    operation.action,
                    ResetQuarantineAction::ResetToBeginning,
                    "{point}"
                ),
                (FaultedAdminAction::ResetToEnd, Operation::ResetQuarantinedFile(operation)) => {
                    assert_eq!(
                        operation.action,
                        ResetQuarantineAction::ResetToEnd,
                        "{point}"
                    )
                }
                (FaultedAdminAction::KeepFailed, Operation::ResetQuarantinedFile(operation)) => {
                    assert_eq!(
                        operation.action,
                        ResetQuarantineAction::KeepFailed,
                        "{point}"
                    )
                }
                (FaultedAdminAction::RemoveQuarantined, Operation::RemoveFile(operation)) => {
                    assert!(operation.administrative, "{point}");
                }
                (_, operation) => {
                    panic!("unexpected final operation for {action:?} at {point}: {operation:?}")
                }
            }
        }
    }
}

/// Scenario: malformed IDs, empty or oversized audit reasons, stale epochs,
/// a zero removal reason, and an already-absent file are submitted.
/// Guarantees: every invalid request fails before a WAL append; absent
/// administrative removal is fail-closed rather than reported idempotent.
#[test]
fn invalid_mutation_requests_leave_the_wal_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    fs::write(&source, b"blocked\n").unwrap();
    let store_options = options(&root.path().join("state"), "invalid-mutations");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        5,
        0,
        FramingResume::Clean,
        AdvisoryPath::unavailable(),
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();
    let mut session = CheckpointAdminSession::open(store_options).unwrap();

    for invalid in [
        hex::encode([5; 15]),
        "AA000000000000000000000000000000".to_owned(),
        "gg000000000000000000000000000000".to_owned(),
    ] {
        let mut request = beginning_request(5, 1, &source, false, "valid audit");
        request.target.file_id = invalid;
        assert!(matches!(
            session.reset_to_beginning(request),
            Err(CheckpointAdminError::InvalidFileId)
        ));
    }

    let mut empty_audit = beginning_request(5, 1, &source, false, "");
    empty_audit.audit.reason.clear();
    assert!(matches!(
        session.reset_to_beginning(empty_audit),
        Err(CheckpointAdminError::AuditReasonRequired { .. })
    ));
    let mut oversized = beginning_request(5, 1, &source, false, "oversized");
    oversized.audit.reason = "x".repeat(AUDIT_REASON_MAX_BYTES + 1);
    assert!(matches!(
        session.reset_to_beginning(oversized),
        Err(CheckpointAdminError::AuditReasonTooLong { .. })
    ));
    assert!(matches!(
        session.reset_to_beginning(beginning_request(5, 2, &source, false, "stale epoch",)),
        Err(CheckpointAdminError::QuarantineEpochMismatch { .. })
    ));
    assert!(matches!(
        session.remove_quarantined(RemoveQuarantinedRequest {
            target: mutation_target(5, 1),
            removal_reason: 0,
            consequence: RemovalConsequence::AcknowledgeDuplicateOrLossPossible,
            audit: mutation_audit("invalid reason", 4_000),
        }),
        Err(CheckpointAdminError::Store(
            StoreError::ReservedReasonCode { .. }
        ))
    ));
    assert!(matches!(
        session.remove_quarantined(RemoveQuarantinedRequest {
            target: mutation_target(99, 1),
            removal_reason: 1,
            consequence: RemovalConsequence::AcknowledgeDuplicateOrLossPossible,
            audit: mutation_audit("absent", 4_000),
        }),
        Err(CheckpointAdminError::FileNotFound { .. })
    ));
    assert_eq!(fs::read(&wal_path).unwrap(), before);
}

/// Scenario: a valid active record is targeted by a quarantine-only
/// high-level mutation.
/// Guarantees: current locked lifecycle is checked before constructing or
/// appending a reset operation.
#[test]
fn mutation_rejects_a_record_that_is_not_quarantined() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "active-target");
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store
        .register_files(vec![registration(6, AdvisoryPath::unavailable())])
        .unwrap();
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    drop(store);
    let before = fs::read(&wal_path).unwrap();

    let mut session = CheckpointAdminSession::open(store_options).unwrap();
    assert!(matches!(
        session.reset_to_beginning(beginning_request(6, 1, root.path(), false, "wrong state",)),
        Err(CheckpointAdminError::ExpectedQuarantine {
            state: CheckpointLifecycleReport::Active,
            ..
        })
    ));
    assert_eq!(fs::read(wal_path).unwrap(), before);
}

/// Scenario: a quarantined record already has the maximum u32 epoch.
/// Guarantees: reset-to-beginning detects checked-add overflow before
/// transitioning the session to append mode or changing the WAL.
#[test]
fn reset_to_beginning_rejects_epoch_overflow_without_append() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "epoch-overflow");
    drop(CheckpointStore::open(store_options.clone()).unwrap());
    let record = SnapshotRecord {
        file_id: FileId([7; 16]),
        file_epoch: u32::MAX,
        committed_offset: 0,
        committed_frontier_guard: CommittedFrontierGuard::empty(),
        fingerprint: vec![7; 8],
        ignored_header_bytes: 0,
        locator: Locator::PosixDevIno { dev: 7, ino: 7 },
        framing_profile_version: FRAMING_PROFILE_VERSION,
        framing_profile_digest: [7; 32],
        framing_resume: FramingResume::Clean,
        lifecycle_state: LifecycleState::Quarantined,
        quarantine_evidence: Some(QuarantineEvidence {
            reason_code: 3,
            observed_size: 0,
            quarantine_epoch: u32::MAX,
            quarantine_time_unix_nano: 3_000,
        }),
        last_seen_time_unix_nano: 2_000,
        advisory_path: AdvisoryPath::unavailable(),
    };
    fs::write(
        store_options.namespace_dir.join(snapshot_file_name(0)),
        encode_snapshot(0, &store_options.namespace_id, &[record]).unwrap(),
    )
    .unwrap();
    fs::write(
        store_options.namespace_dir.join(wal_file_name(0)),
        encode_wal(0, &store_options.namespace_id, &[]).unwrap(),
    )
    .unwrap();
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();

    let mut session = CheckpointAdminSession::open(store_options).unwrap();
    assert!(matches!(
        session.reset_to_beginning(beginning_request(
            7,
            u32::MAX,
            root.path(),
            false,
            "overflow",
        )),
        Err(CheckpointAdminError::FileEpochOverflow { .. })
    ));
    assert_eq!(fs::read(wal_path).unwrap(), before);
}

/// Scenario: reset-to-end is pointed at a different locator, a missing
/// path, and a directory while the quarantined source still exists elsewhere.
/// Guarantees: the API never searches aliases or falls back to advisory
/// metadata, and every failed source validation leaves the WAL unchanged.
#[test]
fn reset_to_end_rejects_wrong_unreadable_and_nonregular_paths() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    let wrong = root.path().join("wrong.log");
    fs::write(&source, vec![b's'; 96]).unwrap();
    fs::write(&wrong, vec![b'w'; 96]).unwrap();
    let directory = root.path().join("directory");
    fs::create_dir(&directory).unwrap();
    let store_options = options(&root.path().join("state"), "source-failures");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        8,
        0,
        FramingResume::Clean,
        AdvisoryPath::from_unix_bytes(wrong.as_os_str().as_encoded_bytes()).unwrap(),
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();
    let mut session = CheckpointAdminSession::open(store_options).unwrap();

    assert!(matches!(
        session.reset_to_end(end_request(8, 1, &wrong, false, "wrong locator")),
        Err(CheckpointAdminError::ResetSourceLocatorMismatch { .. })
    ));
    assert!(matches!(
        session.reset_to_end(end_request(
            8,
            1,
            &root.path().join("missing.log"),
            false,
            "missing",
        )),
        Err(CheckpointAdminError::ResetSourceIo { .. })
    ));
    assert!(matches!(
        session.reset_to_end(end_request(8, 1, &directory, false, "directory")),
        Err(CheckpointAdminError::ResetSourceNotRegular { .. })
    ));
    assert_eq!(fs::read(wal_path).unwrap(), before);
}

/// Scenario: a quarantine carries a deliberately truncated advisory path,
/// but the real exact-locator source exists at another path.
/// Guarantees: reset-to-end refuses the operator-supplied stale suffix and
/// never treats bounded advisory evidence as a path authority or alias search
/// hint.
#[test]
fn reset_to_end_never_trusts_a_truncated_advisory_path() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("real.log");
    fs::write(&source, vec![b't'; 96]).unwrap();
    let advisory = AdvisoryPath::from_unix_bytes(&vec![b'x'; 5_000]).unwrap();
    assert!(advisory.is_truncated());
    let store_options = options(&root.path().join("state"), "truncated-advisory");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        9,
        0,
        FramingResume::Clean,
        advisory,
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();

    let mut session = CheckpointAdminSession::open(store_options).unwrap();
    let displayed_suffix = root.path().join("displayed-suffix.log");
    assert!(matches!(
        session.reset_to_end(end_request(
            9,
            1,
            &displayed_suffix,
            false,
            "must supply exact path",
        )),
        Err(CheckpointAdminError::ResetSourceIo { .. })
    ));
    assert_eq!(fs::read(wal_path).unwrap(), before);
}

/// Scenario: the reset-to-end source grows, shrinks, or changes its trailing
/// bytes at constant size between the first bounded sample and recheck.
/// Guarantees: stable EOF evidence rejects every mutation and no WAL
/// transaction is appended.
#[test]
fn reset_to_end_rejects_mutable_source_evidence_without_append() {
    for case in ["grow", "shrink", "overwrite"] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.log");
        fs::write(&source, vec![b'm'; 128]).unwrap();
        let store_options = options(
            &root.path().join("state"),
            &format!("mutable-source-{case}"),
        );
        let _ = seed_quarantined_source(
            &store_options,
            &source,
            10,
            0,
            FramingResume::Clean,
            AdvisoryPath::unavailable(),
        );
        let wal_path = store_options.namespace_dir.join(wal_file_name(0));
        let before = fs::read(&wal_path).unwrap();
        let mut session = CheckpointAdminSession::open(store_options).unwrap();
        let mutation_path = source.clone();

        let result = session.reset_to_end_with_hook(
            end_request(10, 1, &source, false, "sample stable EOF"),
            move || match case {
                "grow" => {
                    OpenOptions::new()
                        .append(true)
                        .open(&mutation_path)
                        .unwrap()
                        .write_all(b"growth")
                        .unwrap();
                }
                "shrink" => {
                    OpenOptions::new()
                        .write(true)
                        .open(&mutation_path)
                        .unwrap()
                        .set_len(32)
                        .unwrap();
                }
                "overwrite" => {
                    fs::write(&mutation_path, vec![b'n'; 128]).unwrap();
                }
                _ => unreachable!(),
            },
        );
        assert!(
            matches!(result, Err(CheckpointAdminError::ResetSourceChanged { .. })),
            "unexpected mutable-source result for {case}: {result:?}"
        );
        assert_eq!(fs::read(&wal_path).unwrap(), before);
    }
}

/// Scenario: a symlink points to the exact quarantined locator and the
/// operator first selects no-follow, then explicitly selects follow.
/// Guarantees: no-follow rejects without append, while explicit follow
/// samples the target safely and commits the exact EOF.
#[cfg(unix)]
#[test]
fn reset_to_end_enforces_the_explicit_symlink_policy() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    let alias = root.path().join("source-link.log");
    fs::write(&source, vec![b'l'; 72]).unwrap();
    symlink(&source, &alias).unwrap();
    let store_options = options(&root.path().join("state"), "symlink-policy");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        11,
        0,
        FramingResume::Clean,
        AdvisoryPath::unavailable(),
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();
    let mut session = CheckpointAdminSession::open(store_options.clone()).unwrap();

    assert!(matches!(
        session.reset_to_end(end_request(11, 1, &alias, false, "no links")),
        Err(CheckpointAdminError::ResetSourceSymlinkOrReparse { .. })
    ));
    assert_eq!(fs::read(&wal_path).unwrap(), before);
    let result = session
        .reset_to_end(end_request(11, 1, &alias, true, "follow exact link"))
        .unwrap();
    assert_eq!(result.new_offset, Some(72));
    session.release().unwrap();
    assert_eq!(
        CheckpointStore::open(store_options)
            .unwrap()
            .table()
            .get(&FileId([11; 16]))
            .unwrap()
            .committed_offset,
        72
    );
}

/// Scenario: the exact source path is replaced by a symlink to another
/// locator after the first EOF sample while explicit follow is enabled.
/// Guarantees: the final path-bound locator/size recheck rejects the
/// substitution and no reset transaction reaches the WAL.
#[cfg(unix)]
#[test]
fn reset_to_end_rejects_path_substitution_during_sampling() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    let displaced = root.path().join("displaced.log");
    let replacement = root.path().join("replacement.log");
    fs::write(&source, vec![b'o'; 72]).unwrap();
    fs::write(&replacement, vec![b'n'; 72]).unwrap();
    let store_options = options(&root.path().join("state"), "path-substitution");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        15,
        0,
        FramingResume::Clean,
        AdvisoryPath::unavailable(),
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();
    let mut session = CheckpointAdminSession::open(store_options).unwrap();
    let source_for_hook = source.clone();
    let displaced_for_hook = displaced.clone();
    let replacement_for_hook = replacement.clone();

    let result = session.reset_to_end_with_hook(
        end_request(15, 1, &source, true, "reject path replacement"),
        move || {
            fs::rename(&source_for_hook, &displaced_for_hook).unwrap();
            symlink(&replacement_for_hook, &source_for_hook).unwrap();
        },
    );
    assert!(matches!(
        result,
        Err(CheckpointAdminError::ResetSourceChanged { .. })
    ));
    assert_eq!(fs::read(wal_path).unwrap(), before);
    assert!(displaced.is_file());
}

/// Scenario: reset-to-end is directed at a FIFO and another nonregular
/// object under no-follow policy.
/// Guarantees: nonblocking handle validation rejects both special objects
/// without waiting for a writer and without appending to the WAL.
#[cfg(unix)]
#[test]
fn reset_to_end_rejects_fifo_and_nonregular_sources_without_append() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.log");
    let fifo = root.path().join("source.fifo");
    fs::write(&source, vec![b'f'; 72]).unwrap();
    make_fifo(&fifo);
    let store_options = options(&root.path().join("state"), "fifo-source");
    let _ = seed_quarantined_source(
        &store_options,
        &source,
        12,
        0,
        FramingResume::Clean,
        AdvisoryPath::unavailable(),
    );
    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let before = fs::read(&wal_path).unwrap();
    let mut session = CheckpointAdminSession::open(store_options).unwrap();

    assert!(matches!(
        session.reset_to_end(end_request(12, 1, &fifo, false, "fifo")),
        Err(CheckpointAdminError::ResetSourceNotRegular { .. })
    ));
    assert_eq!(fs::read(wal_path).unwrap(), before);
}

/// Scenario: the selected generation has a structurally complete WAL with a
/// corrupt transaction checksum, so ordinary administration fails closed.
/// Guarantees: exclusive evidence administration reports the bounded failure,
/// preserves an exact backup, and never replaces or mutates corrupt authority.
#[test]
fn corrupt_authority_can_be_validated_and_backed_up_without_replacement() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "corrupt-evidence");
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store
        .register_files(vec![registration(31, AdvisoryPath::unavailable())])
        .unwrap();
    drop(store);

    let wal_path = store_options.namespace_dir.join(wal_file_name(0));
    let mut corrupt_wal = fs::read(&wal_path).unwrap();
    *corrupt_wal.last_mut().unwrap() ^= 0x80;
    fs::write(&wal_path, &corrupt_wal).unwrap();
    assert!(CheckpointAdminSession::open(store_options.clone()).is_err());
    let source_before = filesystem_state(&store_options.namespace_dir);

    let session = CheckpointEvidenceSession::open(store_options.clone()).unwrap();
    assert!(matches!(
        session.authority(),
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::SelectedGenerationInvalid,
                selected_generation: Some(0),
                detail_truncated: false,
                ..
            }
        }
    ));
    assert_eq!(
        filesystem_state(&store_options.namespace_dir),
        source_before
    );
    assert!(matches!(
        CheckpointStore::open(store_options.clone()),
        Err(StoreError::NamespaceLocked { .. })
    ));

    let backup = root.path().join("corrupt-backup");
    let manifest = session.backup(&backup).unwrap();
    assert!(matches!(
        manifest.authority,
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::SelectedGenerationInvalid,
                selected_generation: Some(0),
                ..
            }
        }
    ));
    assert_eq!(
        fs::read(backup.join(wal_file_name(0))).unwrap(),
        corrupt_wal
    );
    assert_eq!(
        filesystem_state(&store_options.namespace_dir),
        source_before
    );
    session.release().unwrap();

    let reopened = CheckpointEvidenceSession::open(store_options.clone()).unwrap();
    assert!(matches!(
        reopened.authority(),
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::SelectedGenerationInvalid,
                selected_generation: Some(0),
                ..
            }
        }
    ));
    reopened.release().unwrap();
    assert_eq!(
        filesystem_state(&store_options.namespace_dir),
        source_before
    );
}

/// Scenario: durable generation zero contains state but its authoritative
/// `CURRENT` marker is missing.
/// Guarantees: exclusive evidence administration records the authority gap,
/// backs up every surviving artifact, and neither invents nor publishes authority.
#[test]
fn missing_current_can_be_validated_and_backed_up_without_replacement() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "missing-current-evidence");
    let mut store = CheckpointStore::open(store_options.clone()).unwrap();
    let _ = store
        .register_files(vec![registration(32, AdvisoryPath::unavailable())])
        .unwrap();
    drop(store);
    fs::remove_file(store_options.namespace_dir.join(CURRENT_FILE_NAME)).unwrap();
    assert!(CheckpointAdminSession::open(store_options.clone()).is_err());
    let source_before = filesystem_state(&store_options.namespace_dir);

    let session = CheckpointEvidenceSession::open(store_options.clone()).unwrap();
    assert!(matches!(
        session.authority(),
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::MissingCurrent,
                selected_generation: None,
                ..
            }
        }
    ));
    let backup = root.path().join("missing-current-backup");
    let manifest = session.backup(&backup).unwrap();
    assert!(matches!(
        manifest.authority,
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::MissingCurrent,
                selected_generation: None,
                ..
            }
        }
    ));
    assert!(
        !manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.role == EvidenceArtifactRole::Current)
    );
    assert!(backup.join(snapshot_file_name(0)).is_file());
    assert!(backup.join(wal_file_name(0)).is_file());
    assert!(!backup.join(CURRENT_FILE_NAME).exists());
    assert_eq!(
        filesystem_state(&store_options.namespace_dir),
        source_before
    );
    session.release().unwrap();

    let reopened = CheckpointEvidenceSession::open(store_options.clone()).unwrap();
    assert!(matches!(
        reopened.authority(),
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::MissingCurrent,
                selected_generation: None,
                ..
            }
        }
    ));
    reopened.release().unwrap();
    assert_eq!(
        filesystem_state(&store_options.namespace_dir),
        source_before
    );
}

/// Scenario: valid `CURRENT` bytes name a missing generation above every
/// surviving recognized artifact.
/// Guarantees: validation retains the decoded generation in bounded evidence,
/// backup preserves only real artifacts, and `CURRENT` remains unchanged.
#[test]
fn missing_selected_generation_can_be_backed_up_without_replacement() {
    let root = tempfile::tempdir().unwrap();
    let store_options = options(&root.path().join("state"), "missing-selected-evidence");
    drop(CheckpointStore::open(store_options.clone()).unwrap());
    let marker = encode_current_marker(99);
    fs::write(store_options.namespace_dir.join(CURRENT_FILE_NAME), &marker).unwrap();
    let source_before = filesystem_state(&store_options.namespace_dir);

    let session = CheckpointEvidenceSession::open(store_options.clone()).unwrap();
    assert!(matches!(
        session.authority(),
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::SelectedGenerationInvalid,
                selected_generation: Some(99),
                ..
            }
        }
    ));
    let backup = root.path().join("missing-selected-backup");
    let manifest = session.backup(&backup).unwrap();
    assert!(matches!(
        manifest.authority,
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::SelectedGenerationInvalid,
                selected_generation: Some(99),
                ..
            }
        }
    ));
    assert!(backup.join(snapshot_file_name(0)).is_file());
    assert!(backup.join(wal_file_name(0)).is_file());
    assert_eq!(fs::read(backup.join(CURRENT_FILE_NAME)).unwrap(), marker);
    assert_eq!(
        filesystem_state(&store_options.namespace_dir),
        source_before
    );
    session.release().unwrap();

    let reopened = CheckpointEvidenceSession::open(store_options.clone()).unwrap();
    assert!(matches!(
        reopened.authority(),
        NamespaceAuthorityReport::Invalid {
            failure: NamespaceAuthorityFailureReport {
                kind: NamespaceAuthorityFailureKind::SelectedGenerationInvalid,
                selected_generation: Some(99),
                ..
            }
        }
    ));
    reopened.release().unwrap();
    assert_eq!(
        filesystem_state(&store_options.namespace_dir),
        source_before
    );
}
