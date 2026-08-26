// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! On-disk layout of one checkpoint namespace.
//!
//! The layout is exactly the one specified in `docs/filelog-receiver.md`
//! Appendix B and `docs/filelog-checkpoint-format.md`:
//!
//! ```text
//! ${engine.state_dir}/filelog/<checkpoint.id>/
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
use std::path::{Path, PathBuf};

use super::error::StoreError;

/// Name of the marker file that selects the active generation.
pub const CURRENT_FILE_NAME: &str = "CURRENT";
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
pub(crate) const MAX_TEMP_FILES: usize = 1 + (MAX_GENERATIONS_ON_DISK * 2);

const GENERATION_PREFIX: &str = "offsets-";
const SNAPSHOT_EXTENSION: &str = ".snapshot";
const WAL_EXTENSION: &str = ".wal";
const TEMP_EXTENSION: &str = ".tmp";

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

/// The same-directory temporary name used while `final_name` is being
/// written. Keeping the temporary beside its final name keeps the
/// installing rename atomic (a rename across directories or filesystems is
/// not).
#[must_use]
pub(crate) fn temp_file_name(final_name: &str) -> String {
    format!("{final_name}{TEMP_EXTENSION}")
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
fn classify_generation_file(name: &str) -> Option<(u64, bool)> {
    let rest = name.strip_prefix(GENERATION_PREFIX)?;
    if let Some(digits) = rest.strip_suffix(SNAPSHOT_EXTENSION) {
        return parse_generation(digits).map(|generation| (generation, true));
    }
    let digits = rest.strip_suffix(WAL_EXTENSION)?;
    parse_generation(digits).map(|generation| (generation, false))
}

/// Whether `name` is a temporary file this store itself writes, and is
/// therefore safe to delete during recovery.
///
/// Only the exact names the store produces qualify: `CURRENT.tmp`, and
/// `offsets-<generation>.snapshot.tmp` / `offsets-<generation>.wal.tmp` for
/// a strictly formatted generation number. Any other file -- including an
/// unrelated `*.tmp` file, the marker, a live generation pair, and the
/// ownership lock -- is left untouched.
#[must_use]
pub(crate) fn is_namespace_temp_file(name: &str) -> bool {
    let Some(final_name) = name.strip_suffix(TEMP_EXTENSION) else {
        return false;
    };
    final_name == CURRENT_FILE_NAME || classify_generation_file(final_name).is_some()
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
        let Some((generation, is_snapshot)) = classify_generation_file(name) else {
            continue;
        };
        if !found.contains_key(&generation) && found.len() >= MAX_GENERATIONS_ON_DISK {
            return Err(StoreError::TooManyGenerations {
                dir: dir.to_path_buf(),
                max: MAX_GENERATIONS_ON_DISK,
            });
        }
        let files = found.entry(generation).or_default();
        if is_snapshot {
            files.snapshot = true;
        } else {
            files.wal = true;
        }
    }
    Ok(Some(found))
}

/// Removes every temporary file this store owns in `dir`, returning the
/// number removed.
///
/// This runs only while the namespace ownership lock is held, so a
/// temporary file can never belong to a live writer. A temporary file is an
/// abandoned artifact of an interrupted write: it was never renamed into
/// place, so it is by construction not part of any recoverable generation.
pub(crate) fn remove_stale_temp_files(
    dir: &Path,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<usize>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let mut stale: Vec<PathBuf> = Vec::new();
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
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !is_namespace_temp_file(name) {
            continue;
        }
        if stale.len() >= MAX_TEMP_FILES {
            return Err(StoreError::TooManyTemporaryFiles {
                dir: dir.to_path_buf(),
                max: MAX_TEMP_FILES,
            });
        }
        stale.push(dir.join(name));
    }
    for path in &stale {
        if cancelled() {
            return Ok(None);
        }
        let removed = std::fs::remove_file(path);
        if cancelled() {
            return Ok(None);
        }
        removed.map_err(|source| StoreError::Io {
            operation: "remove an abandoned checkpoint temporary file",
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(Some(stale.len()))
}
