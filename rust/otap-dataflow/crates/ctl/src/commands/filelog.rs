// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Offline filelog checkpoint administration.
//!
//! These commands never resolve an admin endpoint or create an admin client.
//! Filesystem work runs on Tokio's blocking pool and delegates all durable
//! authority, locking, mutation, and backup behavior to the reviewed
//! core-nodes checkpoint administration API.

use crate::args::{
    CheckpointAuditArgs, CheckpointBackupArgs, CheckpointInspectArgs, CheckpointNamespaceArgs,
    CheckpointNamespaceResetAcknowledgement, CheckpointRemoveArgs, CheckpointResetBeginningArgs,
    CheckpointResetCommand, CheckpointResetEndArgs, CheckpointResetNamespaceArgs,
    CheckpointTargetArgs, CheckpointValidateArgs, FilelogArgs, FilelogCheckpointCommand,
    FilelogCommand,
};
use crate::commands::output::{
    validate_mutation_output_mode, write_mutation_command_output, write_read_command_output,
};
use crate::error::CliError;
use crate::style::{HumanStyle, terminal_safe};
use otap_df_core_nodes::receivers::filelog_receiver::checkpoint::{
    AuditMetadata, CheckpointAdminError, CheckpointAdminSession, CheckpointInspectionReport,
    CheckpointNamespaceResetSession, EvidenceBackupManifest, ExpectedQuarantineState,
    FileMutationResult, KeepFailedRequest, NamespaceAuthorityFailureKind, NamespaceAuthorityReport,
    NamespaceResetConsequence, NamespaceResetRequest, NamespaceResetResult, QuarantinedFileTarget,
    RemovalConsequence, RemoveQuarantinedRequest, ResetToBeginningRequest, ResetToEndRequest,
    StoreError, StoreOptions,
};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// Executes one offline filelog command.
pub(crate) async fn run(
    stdout: &mut dyn std::io::Write,
    human_style: HumanStyle,
    args: FilelogArgs,
) -> Result<(), CliError> {
    match args.command {
        FilelogCommand::Checkpoint(args) => match args.command {
            FilelogCheckpointCommand::Inspect(args) => {
                let output = args.output.output;
                let report =
                    run_blocking("inspect filelog checkpoint", move || inspect(args)).await?;
                write_read_command_output(stdout, output, &report, || {
                    Ok(render_inspection(&human_style, &report))
                })
            }
            FilelogCheckpointCommand::Validate(args) => {
                let output = args.output.output;
                let authority =
                    run_blocking("validate filelog checkpoint", move || validate(args)).await?;
                write_read_command_output(stdout, output, &authority, || {
                    Ok(render_authority(&human_style, &authority))
                })
            }
            FilelogCheckpointCommand::Backup(args) => {
                let output = args.output.output;
                let manifest =
                    run_blocking("back up filelog checkpoint", move || backup(args)).await?;
                write_read_command_output(stdout, output, &manifest, || {
                    Ok(render_backup(&human_style, &manifest))
                })
            }
            FilelogCheckpointCommand::Reset(args) => match args.command {
                CheckpointResetCommand::Beginning(args) => {
                    let output = args.output.output.into();
                    validate_mutation_output_mode(output, false)?;
                    require_confirmation(args.acknowledge_duplicates, "--acknowledge-duplicates")?;
                    let result = run_blocking("reset filelog checkpoint to beginning", move || {
                        reset_to_beginning(args)
                    })
                    .await?;
                    write_mutation_command_output(stdout, output, "completed", &result, || {
                        Ok(render_file_mutation(&human_style, &result))
                    })
                }
                CheckpointResetCommand::End(args) => {
                    let output = args.output.output.into();
                    validate_mutation_output_mode(output, false)?;
                    require_confirmation(args.acknowledge_loss, "--acknowledge-loss")?;
                    let result = run_blocking("reset filelog checkpoint to end", move || {
                        reset_to_end(args)
                    })
                    .await?;
                    write_mutation_command_output(stdout, output, "completed", &result, || {
                        Ok(render_file_mutation(&human_style, &result))
                    })
                }
                CheckpointResetCommand::KeepFailed(args) => {
                    let output = args.output.output.into();
                    validate_mutation_output_mode(output, false)?;
                    let result = run_blocking("record filelog checkpoint keep-failed", move || {
                        keep_failed(args)
                    })
                    .await?;
                    write_mutation_command_output(stdout, output, "recorded", &result, || {
                        Ok(render_file_mutation(&human_style, &result))
                    })
                }
                CheckpointResetCommand::Namespace(args) => {
                    let output = args.output.output.into();
                    validate_mutation_output_mode(output, false)?;
                    let result = run_blocking("reset filelog checkpoint namespace", move || {
                        reset_namespace(args)
                    })
                    .await?;
                    write_mutation_command_output(stdout, output, "completed", &result, || {
                        Ok(render_namespace_reset(&human_style, &result))
                    })
                }
            },
            FilelogCheckpointCommand::Remove(args) => {
                let output = args.output.output.into();
                validate_mutation_output_mode(output, false)?;
                require_confirmation(
                    args.acknowledge_duplicate_or_loss,
                    "--acknowledge-duplicate-or-loss",
                )?;
                let result =
                    run_blocking("remove filelog checkpoint record", move || remove(args)).await?;
                write_mutation_command_output(stdout, output, "completed", &result, || {
                    Ok(render_file_mutation(&human_style, &result))
                })
            }
        },
    }
}

async fn run_blocking<T>(
    operation: &'static str,
    task: impl FnOnce() -> Result<T, CliError> + Send + 'static,
) -> Result<T, CliError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|error| {
        CliError::config(format!(
            "blocking task failed while attempting to {operation}: {error}"
        ))
    })?
}

fn inspect(args: CheckpointInspectArgs) -> Result<CheckpointInspectionReport, CliError> {
    let session =
        CheckpointAdminSession::open(store_options(args.namespace)?).map_err(checkpoint_error)?;
    let report = session.inspection().clone();
    session.release().map_err(checkpoint_error)?;
    Ok(report)
}

fn validate(args: CheckpointValidateArgs) -> Result<NamespaceAuthorityReport, CliError> {
    let session = CheckpointNamespaceResetSession::open(store_options(args.namespace)?)
        .map_err(checkpoint_error)?;
    let authority = session.authority().clone();
    session.release().map_err(checkpoint_error)?;
    Ok(authority)
}

fn backup(args: CheckpointBackupArgs) -> Result<EvidenceBackupManifest, CliError> {
    let session = CheckpointNamespaceResetSession::open(store_options(args.namespace)?)
        .map_err(checkpoint_error)?;
    let manifest = session.backup(args.destination).map_err(checkpoint_error)?;
    session.release().map_err(checkpoint_error)?;
    Ok(manifest)
}

fn reset_to_beginning(args: CheckpointResetBeginningArgs) -> Result<FileMutationResult, CliError> {
    let request = ResetToBeginningRequest {
        target: target(args.target),
        audit: audit(args.audit)?,
    };
    let mut session =
        CheckpointAdminSession::open(store_options(args.namespace)?).map_err(checkpoint_error)?;
    let result = session
        .reset_to_beginning(request)
        .map_err(checkpoint_error)?;
    session.release().map_err(checkpoint_error)?;
    Ok(result)
}

fn reset_to_end(args: CheckpointResetEndArgs) -> Result<FileMutationResult, CliError> {
    let request = ResetToEndRequest {
        target: target(args.target),
        source_path: args.source_path,
        follow_symlinks: args.follow_symlinks,
        audit: audit(args.audit)?,
    };
    let mut session =
        CheckpointAdminSession::open(store_options(args.namespace)?).map_err(checkpoint_error)?;
    let result = session.reset_to_end(request).map_err(checkpoint_error)?;
    session.release().map_err(checkpoint_error)?;
    Ok(result)
}

fn keep_failed(
    args: crate::args::CheckpointKeepFailedArgs,
) -> Result<FileMutationResult, CliError> {
    let request = KeepFailedRequest {
        target: target(args.target),
        audit: audit(args.audit)?,
    };
    let mut session =
        CheckpointAdminSession::open(store_options(args.namespace)?).map_err(checkpoint_error)?;
    let result = session.keep_failed(request).map_err(checkpoint_error)?;
    session.release().map_err(checkpoint_error)?;
    Ok(result)
}

fn remove(args: CheckpointRemoveArgs) -> Result<FileMutationResult, CliError> {
    let request = RemoveQuarantinedRequest {
        target: target(args.target),
        removal_reason: args.removal_reason_code,
        consequence: RemovalConsequence::AcknowledgeDuplicateOrLossPossible,
        audit: audit(args.audit)?,
    };
    let mut session =
        CheckpointAdminSession::open(store_options(args.namespace)?).map_err(checkpoint_error)?;
    let result = session
        .remove_quarantined(request)
        .map_err(checkpoint_error)?;
    session.release().map_err(checkpoint_error)?;
    Ok(result)
}

fn reset_namespace(args: CheckpointResetNamespaceArgs) -> Result<NamespaceResetResult, CliError> {
    let consequence = match args.acknowledge {
        CheckpointNamespaceResetAcknowledgement::DuplicatePossible => {
            NamespaceResetConsequence::AcknowledgeDuplicatePossible
        }
        CheckpointNamespaceResetAcknowledgement::LossAccepted => {
            NamespaceResetConsequence::AcknowledgeLossAccepted
        }
    };
    let request = NamespaceResetRequest {
        backup_destination: args.backup_destination,
        consequence,
        audit: audit(args.audit)?,
    };
    let mut session = CheckpointNamespaceResetSession::open(store_options(args.namespace)?)
        .map_err(checkpoint_error)?;
    let result = session.reset_namespace(request).map_err(checkpoint_error)?;
    session.release().map_err(checkpoint_error)?;
    Ok(result)
}

fn store_options(args: CheckpointNamespaceArgs) -> Result<StoreOptions, CliError> {
    let mut options =
        StoreOptions::from_state_dir(args.state_dir, &args.checkpoint_id).map_err(|error| {
            CliError::invalid_usage(terminal_safe(format!(
                "invalid checkpoint namespace: {error}"
            )))
        })?;
    if let Some(timeout) = args.ownership_timeout {
        options.ownership_timeout = timeout;
    }
    if let Some(compact_after_bytes) = args.compact_after_bytes {
        options.compact_after_bytes = compact_after_bytes;
    }
    if let Some(max_tracked_files) = args.max_tracked_files {
        options.max_tracked_files = max_tracked_files;
    }
    if let Some(fingerprint_bytes) = args.fingerprint_bytes {
        options.fingerprint_bytes = fingerprint_bytes;
    }
    Ok(options)
}

fn target(args: CheckpointTargetArgs) -> QuarantinedFileTarget {
    QuarantinedFileTarget {
        file_id: args.file_id,
        expected_lifecycle: ExpectedQuarantineState::Quarantined,
        expected_quarantine_epoch: args.expected_epoch,
    }
}

fn audit(args: CheckpointAuditArgs) -> Result<AuditMetadata, CliError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            CliError::config(format!("system clock is before the Unix epoch: {error}"))
        })?;
    let action_time_unix_nano = u64::try_from(elapsed.as_nanos()).map_err(|_| {
        CliError::config("current Unix timestamp does not fit checkpoint audit metadata")
    })?;
    Ok(AuditMetadata {
        reason: args.reason,
        action_time_unix_nano,
    })
}

fn require_confirmation(confirmed: bool, flag: &'static str) -> Result<(), CliError> {
    if confirmed {
        Ok(())
    } else {
        Err(CliError::invalid_usage(format!(
            "destructive filelog checkpoint operation requires {flag}"
        )))
    }
}

fn checkpoint_error(error: CheckpointAdminError) -> CliError {
    let message = terminal_safe(format!("filelog checkpoint administration failed: {error}"));
    match &error {
        CheckpointAdminError::Namespace(_)
        | CheckpointAdminError::InvalidFileId
        | CheckpointAdminError::AuditReasonRequired { .. }
        | CheckpointAdminError::AuditReasonTooLong { .. } => CliError::invalid_usage(message),
        CheckpointAdminError::FileNotFound { .. } => CliError::not_found(message),
        CheckpointAdminError::ExpectedQuarantine { .. }
        | CheckpointAdminError::QuarantineEpochMismatch { .. }
        | CheckpointAdminError::FileEpochOverflow { .. }
        | CheckpointAdminError::AuthorityChanged { .. }
        | CheckpointAdminError::AuthorityStateChanged
        | CheckpointAdminError::BackupDestinationExists { .. }
        | CheckpointAdminError::BackupDestinationInsideNamespace { .. }
        | CheckpointAdminError::ResetSourceNotRegular { .. }
        | CheckpointAdminError::ResetSourceSymlinkOrReparse { .. }
        | CheckpointAdminError::ResetSourceLocatorMismatch { .. }
        | CheckpointAdminError::ResetSourceChanged { .. }
        | CheckpointAdminError::ResetSourceUnsupported { .. }
        | CheckpointAdminError::ResetSourceValidation { .. }
        | CheckpointAdminError::NamespaceResetGenerationCapacity { .. } => {
            CliError::invalid_request(message)
        }
        CheckpointAdminError::Store(StoreError::NamespaceLocked { .. }) => {
            CliError::invalid_request(message)
        }
        CheckpointAdminError::NamespacePathMismatch { .. }
        | CheckpointAdminError::Store(_)
        | CheckpointAdminError::RequiredArtifactMissing { .. }
        | CheckpointAdminError::MissingQuarantineEvidence { .. }
        | CheckpointAdminError::CountOverflow { .. }
        | CheckpointAdminError::BackupArtifactDisappeared { .. }
        | CheckpointAdminError::BackupIo { .. }
        | CheckpointAdminError::ManifestEncode { .. }
        | CheckpointAdminError::NativePathEncode { .. }
        | CheckpointAdminError::NativePathUnsupported { .. }
        | CheckpointAdminError::NonCanonicalArtifactName { .. }
        | CheckpointAdminError::ResetSourceIo { .. }
        | CheckpointAdminError::BackupVerification { .. } => CliError::config(message),
    }
}

fn render_inspection(style: &HumanStyle, report: &CheckpointInspectionReport) -> String {
    let mut rendered = format!(
        "{}\n{}",
        style.header("filelog checkpoint inspection"),
        render_validation(style, &report.validation)
    );
    let _ = write!(
        rendered,
        "\n{}: {}",
        style.label("quarantine_entries"),
        report.quarantines.len()
    );
    for quarantine in &report.quarantines {
        let _ = write!(
            rendered,
            "\n\n{}\n{}: {}\n{}: {}\n{}: {}\n{}: {:?}\n{}: {:?}\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {:?}",
            style.header("quarantine"),
            style.label("file_id"),
            quarantine.file_id,
            style.label("epoch"),
            quarantine.epoch,
            style.label("committed_offset"),
            quarantine.committed_offset,
            style.label("framing_resume"),
            quarantine.framing_resume,
            style.label("locator"),
            quarantine.locator,
            style.label("reason_code"),
            quarantine.reason_code,
            style.label("observed_size"),
            quarantine.observed_size,
            style.label("quarantine_epoch"),
            quarantine.quarantine_epoch,
            style.label("quarantine_time_unix_nano"),
            quarantine.quarantine_time_unix_nano,
            style.label("advisory_path"),
            quarantine.advisory_path,
        );
    }
    rendered
}

fn render_authority(style: &HumanStyle, authority: &NamespaceAuthorityReport) -> String {
    match authority {
        NamespaceAuthorityReport::Valid { validation } => {
            let mut rendered = format!(
                "{}\n{}: {}",
                style.header("filelog checkpoint validation"),
                style.label("status"),
                style.state("valid")
            );
            let _ = write!(rendered, "\n{}", render_validation(style, validation));
            rendered
        }
        NamespaceAuthorityReport::Invalid { failure } => {
            let selected = failure
                .selected_generation
                .map_or_else(|| "unknown".to_string(), |value| value.to_string());
            format!(
                "{}\n{}: {}\n{}: {}\n{}: {}\n{}: {}",
                style.header("filelog checkpoint validation"),
                style.label("status"),
                style.state("invalid"),
                style.label("failure_kind"),
                authority_failure_kind(failure.kind),
                style.label("selected_generation"),
                selected,
                style.label("detail"),
                terminal_safe(&failure.detail),
            )
        }
    }
}

fn render_validation(
    style: &HumanStyle,
    validation: &otap_df_core_nodes::receivers::filelog_receiver::checkpoint::NamespaceValidationReport,
) -> String {
    format!(
        "{}: {}\n{}: {:?}\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {:?}",
        style.label("checkpoint_id"),
        terminal_safe(&validation.namespace_id),
        style.label("namespace_path"),
        validation.derived_namespace_path,
        style.label("selected_generation"),
        validation.selected_generation,
        style.label("snapshot_records"),
        validation.snapshot_record_count,
        style.label("wal_transactions"),
        validation.wal_transaction_count,
        style.label("tracked_files"),
        validation.tracked_file_count,
        style.label("quarantines"),
        validation.quarantine_count,
        style.label("torn_wal_tail_bytes"),
        validation.torn_wal_tail_bytes,
        style.label("retired_generations"),
        validation.retired_generations,
    )
}

fn render_backup(style: &HumanStyle, manifest: &EvidenceBackupManifest) -> String {
    let mut rendered = format!(
        "{}\n{}: {}\n{}: {}\n{}: {:?}\n{}: {}",
        style.header("filelog checkpoint evidence backup"),
        style.label("manifest_version"),
        manifest.manifest_version,
        style.label("checkpoint_id"),
        terminal_safe(&manifest.namespace_id),
        style.label("source_namespace"),
        manifest.source_namespace,
        style.label("artifacts"),
        manifest.artifacts.len(),
    );
    let _ = write!(
        rendered,
        "\n{}",
        render_authority(style, &manifest.authority)
    );
    for artifact in &manifest.artifacts {
        let _ = write!(
            rendered,
            "\n{}: {} bytes sha256={}",
            artifact.name, artifact.length, artifact.sha256
        );
    }
    rendered
}

fn render_file_mutation(style: &HumanStyle, result: &FileMutationResult) -> String {
    let mut rendered = format!(
        "{}\n{}: {:?}\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {:?} -> {:?}\n{}: {:?} -> {:?}\n{}: {:?} -> {:?}\n{}: {:?}\n{}: {}\n{}: {}\n{}: {}",
        style.header("filelog checkpoint mutation"),
        style.label("action"),
        result.action,
        style.label("checkpoint_id"),
        terminal_safe(&result.namespace_id),
        style.label("file_id"),
        result.file_id,
        style.label("generation"),
        result.generation,
        style.label("wal_sequence"),
        result.wal_sequence,
        style.label("lifecycle"),
        result.old_lifecycle,
        result.new_lifecycle,
        style.label("epoch"),
        result.old_epoch,
        result.new_epoch,
        style.label("offset"),
        result.old_offset,
        result.new_offset,
        style.label("data_effect"),
        result.data_effect,
        style.label("consequence"),
        terminal_safe(&result.consequence),
        style.label("audit_reason"),
        terminal_safe(&result.audit.reason),
        style.label("action_time_unix_nano"),
        result.audit.action_time_unix_nano,
    );
    if let Some(evidence) = &result.reset_to_end_evidence {
        let _ = write!(
            rendered,
            "\n{}: {:?}\n{}: {}\n{}: {:?}\n{}: {:?}",
            style.label("reset_source_path"),
            evidence.source_path,
            style.label("reset_eof_offset"),
            evidence.eof_offset,
            style.label("reset_locator"),
            evidence.locator,
            style.label("reset_committed_frontier_guard"),
            evidence.committed_frontier_guard,
        );
    }
    rendered
}

fn render_namespace_reset(style: &HumanStyle, result: &NamespaceResetResult) -> String {
    let report = &result.reset_report;
    format!(
        "{}\n{}: {}\n{}: {:?}\n{}: {:?}\n{}: {:?} -> {}\n{}: {:?} -> {}\n{}: {:?} -> {}\n{}: {:?}\n{}: {:?}\n{}: {}\n{}: {}\n{}: {}",
        style.header("filelog checkpoint namespace reset"),
        style.label("checkpoint_id"),
        terminal_safe(&report.namespace_id),
        style.label("namespace_path"),
        report.namespace_path,
        style.label("backup_destination"),
        report.backup_destination,
        style.label("generation"),
        report.old_generation,
        report.new_generation,
        style.label("tracked_files"),
        report.old_tracked_file_count,
        report.new_tracked_file_count,
        style.label("quarantines"),
        report.old_quarantine_count,
        report.new_quarantine_count,
        style.label("retained_evidence_generations"),
        report.retained_evidence_generations,
        style.label("data_effect"),
        report.data_effect,
        style.label("consequence"),
        terminal_safe(&report.consequence),
        style.label("audit_reason"),
        terminal_safe(&report.audit.reason),
        style.label("action_time_unix_nano"),
        report.audit.action_time_unix_nano,
    )
}

fn authority_failure_kind(kind: NamespaceAuthorityFailureKind) -> &'static str {
    match kind {
        NamespaceAuthorityFailureKind::MissingCurrent => "missing_current",
        NamespaceAuthorityFailureKind::CurrentUnreadable => "current_unreadable",
        NamespaceAuthorityFailureKind::CurrentInvalid => "current_invalid",
        NamespaceAuthorityFailureKind::SelectedGenerationInvalid => "selected_generation_invalid",
        NamespaceAuthorityFailureKind::GenerationInventoryInvalid => "generation_inventory_invalid",
        NamespaceAuthorityFailureKind::ReportInvalid => "report_invalid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;
    use otap_df_core_nodes::receivers::filelog_receiver::checkpoint::CheckpointStore;
    use otap_df_core_nodes::receivers::filelog_receiver::checkpoint::primitives::{
        AdvisoryPath, CommittedFrontierGuard, FRAMING_PROFILE_VERSION, FileId, FramingResume,
        LifecycleState, Locator,
    };
    use otap_df_core_nodes::receivers::filelog_receiver::checkpoint::store::layout::wal_file_name;
    use otap_df_core_nodes::receivers::filelog_receiver::checkpoint::wal::{
        QuarantineFile, RegisterFile,
    };
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn cli_path(path: &Path) -> &str {
        path.to_str().expect("temporary test paths are UTF-8")
    }

    fn file_id_hex(seed: u8) -> String {
        format!("{seed:02x}").repeat(16)
    }

    fn registration(seed: u8, locator: Locator) -> RegisterFile {
        RegisterFile {
            file_id: FileId([seed; 16]),
            file_epoch: 1,
            committed_offset: 0,
            committed_frontier_guard: CommittedFrontierGuard::empty(),
            fingerprint: vec![seed; 8],
            ignored_header_bytes: 0,
            locator,
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: [seed; 32],
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 1,
            advisory_path: AdvisoryPath::unavailable(),
        }
    }

    fn synthetic_locator(seed: u8) -> Locator {
        Locator::PosixDevIno {
            dev: 1,
            ino: u64::from(seed),
        }
    }

    fn seed_quarantine(
        state_dir: &Path,
        checkpoint_id: &str,
        seed: u8,
        locator: Locator,
        observed_size: u64,
    ) -> StoreOptions {
        let options = StoreOptions::from_state_dir(state_dir, checkpoint_id).unwrap();
        let mut store = CheckpointStore::open(options.clone()).unwrap();
        let _ = store
            .register_files(vec![registration(seed, locator)])
            .unwrap();
        let _ = store
            .quarantine_files(vec![QuarantineFile {
                file_id: FileId([seed; 16]),
                expected_file_epoch: 1,
                reason_code: 3,
                locator,
                observed_size,
                quarantine_epoch: 1,
                quarantine_time_unix_nano: 2,
            }])
            .unwrap();
        drop(store);
        options
    }

    /// Scenario: a corrupt checkpoint is validated and backed up while a
    /// nonexistent remote profile is supplied globally.
    /// Guarantees: offline dispatch occurs before connection resolution, the
    /// invalid authority is emitted as data, and backup preserves the exact
    /// corrupt WAL bytes.
    #[tokio::test]
    async fn corrupt_validate_and_backup_skip_connection_resolution() {
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let checkpoint_id = "offline-corrupt";
        let options = StoreOptions::from_state_dir(&state_dir, checkpoint_id).unwrap();
        drop(CheckpointStore::open(options.clone()).unwrap());
        let wal_path = options.namespace_dir.join(wal_file_name(0));
        let mut corrupt_wal = fs::read(&wal_path).unwrap();
        *corrupt_wal.last_mut().unwrap() ^= 0x80;
        fs::write(&wal_path, &corrupt_wal).unwrap();
        let missing_profile = root.path().join("missing-profile.yaml");

        let cli = Cli::try_parse_from([
            "dfctl",
            "--profile-file",
            cli_path(&missing_profile),
            "filelog",
            "checkpoint",
            "validate",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--output",
            "json",
        ])
        .unwrap();
        let mut stdout = Vec::new();
        crate::run(cli, &mut stdout).await.unwrap();
        let validation: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(validation["status"], "invalid");
        assert_eq!(validation["failure"]["kind"], "selected_generation_invalid");

        let destination = root.path().join("evidence");
        let cli = Cli::try_parse_from([
            "dfctl",
            "--url",
            "not a valid URL",
            "filelog",
            "checkpoint",
            "backup",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--destination",
            cli_path(&destination),
            "--output",
            "json",
        ])
        .unwrap();
        stdout.clear();
        crate::run(cli, &mut stdout).await.unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(manifest["authority"]["status"], "invalid");
        assert_eq!(
            fs::read(destination.join(wal_file_name(0))).unwrap(),
            corrupt_wal
        );
    }

    /// Scenario: a live checkpoint store still owns the namespace when an
    /// offline validation command starts.
    /// Guarantees: the CLI never bypasses the runtime lock and maps bounded
    /// lock contention to the stable invalid-request exit code.
    #[tokio::test]
    async fn live_namespace_lock_is_reported_as_invalid_request() {
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let checkpoint_id = "offline-locked";
        let options = StoreOptions::from_state_dir(&state_dir, checkpoint_id).unwrap();
        let store = CheckpointStore::open(options).unwrap();
        let cli = Cli::try_parse_from([
            "dfctl",
            "filelog",
            "checkpoint",
            "validate",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--ownership-timeout",
            "10ms",
            "--output",
            "json",
        ])
        .unwrap();
        let mut stdout = Vec::new();
        let error = crate::run(cli, &mut stdout).await.unwrap_err();
        assert_eq!(error.exit_code(), 4);
        assert!(stdout.is_empty());
        drop(store);
    }

    /// Scenario: a corrupt checkpoint namespace is reset through the offline
    /// CLI after an explicit duplicate acknowledgement and create-only backup.
    /// Guarantees: machine output uses the shared mutation envelope, the
    /// corrupt evidence is retained, and restart selects a higher empty
    /// authority without salvage.
    #[tokio::test]
    async fn corrupt_namespace_reset_is_backup_gated_and_restartable() {
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let checkpoint_id = "offline-reset";
        let options = StoreOptions::from_state_dir(&state_dir, checkpoint_id).unwrap();
        let mut store = CheckpointStore::open(options.clone()).unwrap();
        let _ = store
            .register_files(vec![registration(2, synthetic_locator(2))])
            .unwrap();
        drop(store);
        let wal_path = options.namespace_dir.join(wal_file_name(0));
        let mut corrupt_wal = fs::read(&wal_path).unwrap();
        *corrupt_wal.last_mut().unwrap() ^= 0x40;
        fs::write(&wal_path, &corrupt_wal).unwrap();
        let destination = root.path().join("reset-evidence");

        let cli = Cli::try_parse_from([
            "dfctl",
            "filelog",
            "checkpoint",
            "reset",
            "namespace",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--backup-destination",
            cli_path(&destination),
            "--acknowledge",
            "duplicate-possible",
            "--reason",
            "rebuild corrupt checkpoint",
            "--output",
            "json",
        ])
        .unwrap();
        let mut stdout = Vec::new();
        crate::run(cli, &mut stdout).await.unwrap();
        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["outcome"], "completed");
        assert_eq!(output["data"]["reset_report"]["old_generation"], 0);
        assert_eq!(output["data"]["reset_report"]["new_generation"], 1);
        assert_eq!(
            fs::read(destination.join(wal_file_name(0))).unwrap(),
            corrupt_wal
        );

        let reopened = CheckpointStore::open(options).unwrap();
        assert_eq!(reopened.generation(), 1);
        assert!(reopened.table().is_empty());
    }

    /// Scenario: an operator inspects a quarantine, records keep-failed, and
    /// then removes the exact epoch with explicit duplicate-or-loss
    /// acknowledgement.
    /// Guarantees: each command uses the offline output contract, keep-failed
    /// preserves quarantine, and removal remains durable after reopen.
    #[tokio::test]
    async fn inspect_keep_failed_and_remove_quarantine() {
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let checkpoint_id = "offline-quarantine";
        let options = seed_quarantine(&state_dir, checkpoint_id, 3, synthetic_locator(3), 10);
        let file_id = file_id_hex(3);

        let cli = Cli::try_parse_from([
            "dfctl",
            "filelog",
            "checkpoint",
            "inspect",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
        ])
        .unwrap();
        let mut stdout = Vec::new();
        crate::run(cli, &mut stdout).await.unwrap();
        let inspection = String::from_utf8(stdout.clone()).unwrap();
        assert!(inspection.contains(&file_id));
        assert!(inspection.contains("locator"));
        assert!(inspection.contains("quarantine_time_unix_nano"));

        let cli = Cli::try_parse_from([
            "dfctl",
            "filelog",
            "checkpoint",
            "reset",
            "keep-failed",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--file-id",
            &file_id,
            "--expected-epoch",
            "1",
            "--reason",
            "continue investigation",
            "--output",
            "json",
        ])
        .unwrap();
        stdout.clear();
        crate::run(cli, &mut stdout).await.unwrap();
        let keep: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(keep["outcome"], "recorded");
        assert_eq!(keep["data"]["new_lifecycle"], "quarantined");

        let cli = Cli::try_parse_from([
            "dfctl",
            "filelog",
            "checkpoint",
            "remove",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--file-id",
            &file_id,
            "--expected-epoch",
            "1",
            "--removal-reason-code",
            "8",
            "--reason",
            "remove blocked record",
            "--acknowledge-duplicate-or-loss",
            "--output",
            "json",
        ])
        .unwrap();
        stdout.clear();
        crate::run(cli, &mut stdout).await.unwrap();
        let removal: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(removal["outcome"], "completed");
        assert_eq!(removal["data"]["data_effect"], "duplicate_or_loss_possible");

        let reopened = CheckpointStore::open(options).unwrap();
        assert!(reopened.table().get(&FileId([3; 16])).is_none());
    }

    /// Scenario: reset-to-beginning is invoked with exact file and epoch
    /// evidence plus explicit duplicate acknowledgement.
    /// Guarantees: the CLI durably returns the record to Active at offset zero
    /// with an incremented epoch.
    #[tokio::test]
    async fn reset_to_beginning_requires_ack_and_commits_zero() {
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let checkpoint_id = "offline-beginning";
        let options = seed_quarantine(&state_dir, checkpoint_id, 4, synthetic_locator(4), 10);
        let file_id = file_id_hex(4);
        let cli = Cli::try_parse_from([
            "dfctl",
            "filelog",
            "checkpoint",
            "reset",
            "beginning",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--file-id",
            &file_id,
            "--expected-epoch",
            "1",
            "--reason",
            "replay source",
            "--acknowledge-duplicates",
            "--output",
            "json",
        ])
        .unwrap();
        let mut stdout = Vec::new();
        crate::run(cli, &mut stdout).await.unwrap();

        let reopened = CheckpointStore::open(options).unwrap();
        let record = reopened.table().get(&FileId([4; 16])).unwrap();
        assert_eq!(record.lifecycle_state, LifecycleState::Active);
        assert_eq!(record.file_epoch, 2);
        assert_eq!(record.committed_offset, 0);
    }

    /// Scenario: reset-to-end targets the exact quarantined Unix file and
    /// explicitly accepts loss before the sampled EOF.
    /// Guarantees: the CLI commits the stable current size and real trailing
    /// guard, increments the epoch, and reopens as Active.
    #[cfg(unix)]
    #[tokio::test]
    async fn reset_to_end_commits_exact_source_eof() {
        use std::os::unix::fs::MetadataExt as _;

        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let source = root.path().join("source.log");
        let bytes = b"one\ntwo\nthree\n";
        fs::write(&source, bytes).unwrap();
        let metadata = fs::metadata(&source).unwrap();
        let locator = Locator::PosixDevIno {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        let checkpoint_id = "offline-end";
        let options = seed_quarantine(&state_dir, checkpoint_id, 5, locator, metadata.len());
        let file_id = file_id_hex(5);
        let cli = Cli::try_parse_from([
            "dfctl",
            "filelog",
            "checkpoint",
            "reset",
            "end",
            "--state-dir",
            cli_path(&state_dir),
            "--checkpoint-id",
            checkpoint_id,
            "--file-id",
            &file_id,
            "--expected-epoch",
            "1",
            "--source-path",
            cli_path(&source),
            "--reason",
            "skip malformed prefix",
            "--acknowledge-loss",
            "--output",
            "json",
        ])
        .unwrap();
        let mut stdout = Vec::new();
        crate::run(cli, &mut stdout).await.unwrap();

        let reopened = CheckpointStore::open(options).unwrap();
        let record = reopened.table().get(&FileId([5; 16])).unwrap();
        assert_eq!(record.lifecycle_state, LifecycleState::Active);
        assert_eq!(record.file_epoch, 2);
        assert_eq!(record.committed_offset, bytes.len() as u64);
        assert_eq!(
            record.committed_frontier_guard,
            CommittedFrontierGuard::compute(bytes.len() as u64, bytes).unwrap()
        );
    }
}
