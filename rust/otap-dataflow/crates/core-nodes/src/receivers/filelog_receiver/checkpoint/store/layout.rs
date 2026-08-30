// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! On-disk layout of one checkpoint namespace.
//!
//! The layout is exactly the one specified in `docs/filelog-receiver.md`
//! Appendix B and `docs/filelog-checkpoint-format.md`:
//!
//! ```text
//! ${engine.state_dir}/filelog/@v1/<encoded checkpoint.id>/
//!   CURRENT
//!   offsets-<generation>.snapshot
//!   offsets-<generation>.wal
//!   ownership.lock
//! ```
//!
//! `<generation>` is the ASCII decimal rendering of a `u64` with no leading
//! zeros. This module owns the naming rules, the strict parser for them, and
//! the classification of the same-directory temporary files the store
//! writes, so cleanup can never remove a file this namespace does not own.

use std::collections::BTreeMap;
use std::path::Path;

use super::error::StoreError;

/// Name of the marker file that selects the active generation.
pub const CURRENT_FILE_NAME: &str = "CURRENT";
/// Initial-publication marker temporary.
pub(crate) const CURRENT_CREATE_TEMP_FILE_NAME: &str = "CURRENT.create.tmp";
/// Compaction marker temporary.
pub(crate) const CURRENT_COMPACT_TEMP_FILE_NAME: &str = "CURRENT.compact.tmp";
/// Name of the advisory namespace ownership lock file.
pub const OWNERSHIP_LOCK_FILE_NAME: &str = "ownership.lock";
/// The generation number a namespace is created with.
pub const INITIAL_GENERATION: u64 = 0;
/// Maximum recognized generations retained during recovery.
///
/// A healthy store has at most the active generation and one retired or
/// incompletely staged generation. One additional generation is accepted
/// so namespaces created by the pre-hardening implementation can be opened
/// and cleaned, while hostile directory populations remain bounded.
pub const MAX_GENERATIONS_ON_DISK: usize = 3;
/// Maximum abandoned temporary artifacts recovery will process.
pub(crate) const MAX_TEMP_FILES: usize = 2 + (MAX_GENERATIONS_ON_DISK * 2);

const GENERATION_PREFIX: &str = "offsets-";
const SNAPSHOT_EXTENSION: &str = ".snapshot";
const WAL_EXTENSION: &str = ".wal";
const CREATE_TEMP_EXTENSION: &str = ".create.tmp";
const COMPACT_TEMP_EXTENSION: &str = ".compact.tmp";

/// Durable publication sequence that owns one exact temporary-name family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationRole {
    /// First-generation namespace creation.
    Create,
    /// Later-generation compaction.
    Compact,
}

/// Recognized checkpoint artifact kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamespaceArtifactKind {
    /// Generation-selection marker.
    Current,
    /// Namespace ownership lock.
    OwnershipLock,
    /// Snapshot recovery base.
    Snapshot,
    /// Append-only write-ahead log.
    Wal,
}

/// On-disk form of one recognized artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactForm {
    /// Published name.
    Final,
    /// Initial-publication temporary name.
    CreateTemporary,
    /// Compaction temporary name.
    CompactTemporary,
}

/// One recognized checkpoint namespace artifact name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NamespaceArtifact {
    /// Marker, lock, snapshot, or WAL role.
    pub(crate) kind: NamespaceArtifactKind,
    /// Published, temporary, or backup form.
    pub(crate) form: ArtifactForm,
    /// Generation encoded in snapshot/WAL names.
    pub(crate) generation: Option<u64>,
}

/// File name of `generation`'s snapshot.
#[must_use]
pub fn snapshot_file_name(generation: u64) -> String {
    format!("{GENERATION_PREFIX}{generation}{SNAPSHOT_EXTENSION}")
}

/// File name of `generation`'s WAL.
#[must_use]
pub fn wal_file_name(generation: u64) -> String {
    format!("{GENERATION_PREFIX}{generation}{WAL_EXTENSION}")
}

/// The exact same-directory temporary name for `role`.
#[must_use]
pub(crate) fn temp_file_name(final_name: &str, role: PublicationRole) -> String {
    let extension = match role {
        PublicationRole::Create => CREATE_TEMP_EXTENSION,
        PublicationRole::Compact => COMPACT_TEMP_EXTENSION,
    };
    format!("{final_name}{extension}")
}

/// Which files of a generation's snapshot/WAL pair are present on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerationFiles {
    /// Whether `offsets-<generation>.snapshot` exists.
    pub snapshot: bool,
    /// Whether `offsets-<generation>.wal` exists.
    pub wal: bool,
}

impl GenerationFiles {
    /// Whether both files of the pair are present. A generation is only ever
    /// recoverable as a complete pair; the snapshot is the recovery base and
    /// the WAL carries every change made since it was written.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.snapshot && self.wal
    }

    /// The missing member of an incomplete pair, for diagnostics.
    #[must_use]
    pub(crate) const fn missing(self) -> &'static str {
        if !self.snapshot {
            "the snapshot file"
        } else if !self.wal {
            "the WAL file"
        } else {
            "nothing"
        }
    }
}

/// Parses a generation number exactly as the format specifies it: ASCII
/// decimal, no sign, no leading zeros, within `u64` range. Anything else is
/// not a name this namespace produced and is reported as such (`None`)
/// rather than being coerced into a number.
fn parse_generation(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse::<u64>().ok()
}

/// Classifies a directory entry name as one of this generation-pair's files.
fn classify_generation_file(name: &str) -> Option<(u64, NamespaceArtifactKind)> {
    let rest = name.strip_prefix(GENERATION_PREFIX)?;
    if let Some(digits) = rest.strip_suffix(SNAPSHOT_EXTENSION) {
        return parse_generation(digits)
            .map(|generation| (generation, NamespaceArtifactKind::Snapshot));
    }
    let digits = rest.strip_suffix(WAL_EXTENSION)?;
    parse_generation(digits).map(|generation| (generation, NamespaceArtifactKind::Wal))
}

/// Classifies exactly the names the checkpoint store owns.
#[must_use]
pub(crate) fn classify_namespace_artifact(name: &str) -> Option<NamespaceArtifact> {
    if name == OWNERSHIP_LOCK_FILE_NAME {
        return Some(NamespaceArtifact {
            kind: NamespaceArtifactKind::OwnershipLock,
            form: ArtifactForm::Final,
            generation: None,
        });
    }
    let (final_name, form) = if let Some(final_name) = name.strip_suffix(CREATE_TEMP_EXTENSION) {
        (final_name, ArtifactForm::CreateTemporary)
    } else if let Some(final_name) = name.strip_suffix(COMPACT_TEMP_EXTENSION) {
        (final_name, ArtifactForm::CompactTemporary)
    } else {
        (name, ArtifactForm::Final)
    };
    if final_name == CURRENT_FILE_NAME {
        return Some(NamespaceArtifact {
            kind: NamespaceArtifactKind::Current,
            form,
            generation: None,
        });
    }
    classify_generation_file(final_name).map(|(generation, kind)| NamespaceArtifact {
        kind,
        form,
        generation: Some(generation),
    })
}

/// Returns the exact canonical spelling of a recognized artifact when
/// `name` differs only by ASCII case.
///
/// Administration uses this second-pass classifier to reject names that a
/// case-insensitive filesystem may resolve through the canonical pathname
/// even though an exact inventory would otherwise omit them.
#[must_use]
pub(crate) fn canonical_artifact_name_ignoring_ascii_case(name: &str) -> Option<String> {
    if !name.is_ascii() {
        return None;
    }
    if name.eq_ignore_ascii_case(OWNERSHIP_LOCK_FILE_NAME) {
        return Some(OWNERSHIP_LOCK_FILE_NAME.to_owned());
    }

    let lowercase = name.to_ascii_lowercase();
    let (final_name, suffix) =
        if let Some(final_name) = lowercase.strip_suffix(CREATE_TEMP_EXTENSION) {
            (final_name, CREATE_TEMP_EXTENSION)
        } else if let Some(final_name) = lowercase.strip_suffix(COMPACT_TEMP_EXTENSION) {
            (final_name, COMPACT_TEMP_EXTENSION)
        } else {
            (lowercase.as_str(), "")
        };

    let canonical_final = if final_name == "current" {
        CURRENT_FILE_NAME.to_owned()
    } else {
        let rest = final_name.strip_prefix(GENERATION_PREFIX)?;
        let (digits, kind) = if let Some(digits) = rest.strip_suffix(SNAPSHOT_EXTENSION) {
            (digits, NamespaceArtifactKind::Snapshot)
        } else {
            (
                rest.strip_suffix(WAL_EXTENSION)?,
                NamespaceArtifactKind::Wal,
            )
        };
        let generation = parse_generation(digits)?;
        match kind {
            NamespaceArtifactKind::Snapshot => snapshot_file_name(generation),
            NamespaceArtifactKind::Wal => wal_file_name(generation),
            NamespaceArtifactKind::Current | NamespaceArtifactKind::OwnershipLock => {
                unreachable!("the case-insensitive generation parser returns snapshot or WAL")
            }
        }
    };
    Some(format!("{canonical_final}{suffix}"))
}

/// Scans `dir` for generation files, returning which members of each
/// generation's pair exist, ordered by generation.
///
/// Entries that are not this namespace's own generation files are ignored:
/// the store never deletes or interprets a file it did not create.
pub(crate) fn scan_generations(
    dir: &Path,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<BTreeMap<u64, GenerationFiles>>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let mut found: BTreeMap<u64, GenerationFiles> = BTreeMap::new();
    let entries = std::fs::read_dir(dir);
    if cancelled() {
        return Ok(None);
    }
    let entries = entries.map_err(|source| StoreError::Io {
        operation: "list the checkpoint namespace directory",
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        if cancelled() {
            return Ok(None);
        }
        let entry = entry.map_err(|source| StoreError::Io {
            operation: "read a checkpoint namespace directory entry",
            path: dir.to_path_buf(),
            source,
        })?;
        let file_name = entry.file_name();
        // A name that is not valid UTF-8 cannot be one this store wrote:
        // every name it writes is built from ASCII literals and decimal
        // digits.
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(NamespaceArtifact {
            kind,
            form: ArtifactForm::Final,
            generation: Some(generation),
        }) = classify_namespace_artifact(name)
        else {
            continue;
        };
        if !found.contains_key(&generation) && found.len() >= MAX_GENERATIONS_ON_DISK {
            return Err(StoreError::TooManyGenerations {
                dir: dir.to_path_buf(),
                max: MAX_GENERATIONS_ON_DISK,
            });
        }
        let files = found.entry(generation).or_default();
        match kind {
            NamespaceArtifactKind::Snapshot => files.snapshot = true,
            NamespaceArtifactKind::Wal => files.wal = true,
            NamespaceArtifactKind::Current | NamespaceArtifactKind::OwnershipLock => {
                unreachable!("only generation artifacts carry a generation")
            }
        }
    }
    Ok(Some(found))
}

/// Non-cancellable read-only generation inventory.
pub(crate) fn scan_generations_read_only(
    dir: &Path,
) -> Result<BTreeMap<u64, GenerationFiles>, StoreError> {
    scan_generations(dir, || false)
        .map(|scan| scan.expect("a non-cancellable generation scan cannot be cancelled"))
}
