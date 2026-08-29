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
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    CommittedFrontierGuard, FRAMING_PROFILE_VERSION, FileId, Locator,
};
use crate::receivers::filelog_receiver::checkpoint::store::layout::{
    OWNERSHIP_LOCK_FILE_NAME, backup_file_name, snapshot_file_name, temp_file_name, wal_file_name,
};
use crate::receivers::filelog_receiver::checkpoint::wal::{
    QuarantineFile, RegisterFile, UpdateProgress,
};

fn options(state_dir: &Path, namespace_id: &str) -> StoreOptions {
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
        assert_eq!(manifest.validation.tracked_file_count, 1);
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
        .join(temp_file_name(CURRENT_FILE_NAME));
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
    assert_eq!(manifest.validation.torn_wal_tail_bytes, 3);
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
/// marker recovery forms, generation temporary/backup forms, and unrelated
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
    let _ = fs::copy(&current, namespace.join(temp_file_name(CURRENT_FILE_NAME))).unwrap();
    let _ = fs::copy(
        &current,
        namespace.join(backup_file_name(CURRENT_FILE_NAME)),
    )
    .unwrap();
    fs::write(
        namespace.join(temp_file_name(&snapshot_file_name(2))),
        b"partial-snapshot",
    )
    .unwrap();
    fs::write(
        namespace.join(backup_file_name(&snapshot_file_name(2))),
        b"replacement-snapshot-backup",
    )
    .unwrap();
    fs::write(
        namespace.join(temp_file_name(&wal_file_name(2))),
        b"partial-wal",
    )
    .unwrap();
    fs::write(
        namespace.join(backup_file_name(&wal_file_name(2))),
        b"replacement-wal-backup",
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
        temp_file_name(CURRENT_FILE_NAME),
        backup_file_name(CURRENT_FILE_NAME),
        snapshot_file_name(0),
        wal_file_name(0),
        snapshot_file_name(1),
        wal_file_name(1),
        temp_file_name(&snapshot_file_name(2)),
        backup_file_name(&snapshot_file_name(2)),
        temp_file_name(&wal_file_name(2)),
        backup_file_name(&wal_file_name(2)),
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
        role_for(&temp_file_name(CURRENT_FILE_NAME)),
        (EvidenceArtifactRole::CurrentTemporary, None)
    );
    assert_eq!(
        role_for(&backup_file_name(CURRENT_FILE_NAME)),
        (EvidenceArtifactRole::CurrentBackup, None)
    );
    assert_eq!(
        role_for(&snapshot_file_name(1)),
        (EvidenceArtifactRole::Snapshot, Some(1))
    );
    assert_eq!(
        role_for(&temp_file_name(&snapshot_file_name(2))),
        (EvidenceArtifactRole::SnapshotTemporary, Some(2))
    );
    assert_eq!(
        role_for(&backup_file_name(&snapshot_file_name(2))),
        (EvidenceArtifactRole::SnapshotBackup, Some(2))
    );
    assert_eq!(
        role_for(&wal_file_name(1)),
        (EvidenceArtifactRole::Wal, Some(1))
    );
    assert_eq!(
        role_for(&temp_file_name(&wal_file_name(2))),
        (EvidenceArtifactRole::WalTemporary, Some(2))
    );
    assert_eq!(
        role_for(&backup_file_name(&wal_file_name(2))),
        (EvidenceArtifactRole::WalBackup, Some(2))
    );
    assert_eq!(manifest.namespace_id, "backup-report");
    assert_eq!(
        manifest.source_namespace,
        manifest.validation.derived_namespace_path
    );
    assert_eq!(manifest.selected_generation, 1);
    assert_eq!(manifest.validation.torn_wal_tail_bytes, 0);
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
            namespace.join(temp_file_name(CURRENT_FILE_NAME))
        } else {
            namespace.join(temp_file_name(&snapshot_file_name(9)))
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
