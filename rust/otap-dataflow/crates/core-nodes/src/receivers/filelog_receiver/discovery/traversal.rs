// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! One-directory-handle, width-independent filesystem traversal.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{DiscoveryError, DiscoveryIssue};
use crate::receivers::filelog_receiver::checkpoint::Locator;

const DIRECTORY_ENTRY_DIGEST_DOMAIN: &[u8] = b"otel-arrow-filelog-directory-entry-v1\0";
const DIRECTORY_ENTRY_SET_DIGEST_DOMAIN: &[u8] = b"otel-arrow-filelog-directory-entry-set-v1\0";
const DIRECTORY_CHANGE_DIGEST_DOMAIN: &[u8] = b"otel-arrow-filelog-directory-change-v1\0";
#[cfg(target_os = "linux")]
const DIRECTORY_ENTRY_BATCH_SIZE: usize = 256;
#[cfg(not(target_os = "linux"))]
const DIRECTORY_ENTRY_BATCH_SIZE: usize = 1;

/// Type information used only to route one traversal entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TraversalEntryKind {
    Directory,
    RegularFile,
    Other,
}

/// One bounded traversal unit. No directory handle remains open while the
/// scanner owns this value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TraversalEntry {
    path: PathBuf,
    name: OsString,
    depth: usize,
    kind: TraversalEntryKind,
    path_is_symlink: bool,
}

impl TraversalEntry {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) const fn depth(&self) -> usize {
        self.depth
    }

    pub(super) const fn kind(&self) -> TraversalEntryKind {
        self.kind
    }

    pub(super) const fn path_is_symlink(&self) -> bool {
        self.path_is_symlink
    }
}

#[derive(Debug)]
pub(super) enum TraversalFailure {
    Cancelled,
    Recoverable(DiscoveryIssue),
    Stop(DiscoveryIssue),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryToken {
    locator: Locator,
    change_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryEvidence {
    token: DirectoryToken,
    entry_count: u64,
    entry_set_digest: [u8; 32],
}

#[derive(Debug)]
struct Frame {
    id: u64,
    component: Option<OsString>,
    depth: usize,
    baseline: Option<DirectoryEvidence>,
    after: Option<OsString>,
    recovery_attempts: u8,
    entry_issue_reported: bool,
}

#[derive(Debug)]
struct CachedEntry {
    name: OsString,
    kind: TraversalEntryKind,
    path_is_symlink: bool,
}

#[derive(Debug)]
struct PendingDirectory {
    component: Option<OsString>,
    path: PathBuf,
    depth: usize,
}

/// Explicit traversal state. Frames retain no native directory handle.
#[derive(Debug)]
pub(super) struct BoundedTraversal {
    root: PathBuf,
    max_depth: usize,
    follow_symlinks: bool,
    root_pending: bool,
    finished: bool,
    last_unit_opened_directory: bool,
    frames: Vec<Frame>,
    pending_directory: Option<PendingDirectory>,
    entry_batch: Vec<CachedEntry>,
    entry_batch_owner: Option<u64>,
    next_frame_id: u64,
}

impl BoundedTraversal {
    pub(super) fn new(
        root: PathBuf,
        max_depth: usize,
        follow_symlinks: bool,
    ) -> Result<Self, DiscoveryError> {
        let frame_capacity = max_depth
            .checked_add(1)
            .ok_or(DiscoveryError::BoundTooLarge {
                field: "max_recursion_depth + root frame",
                value: u64::try_from(max_depth).unwrap_or(u64::MAX),
            })?;
        let mut frames = Vec::new();
        frames.try_reserve_exact(frame_capacity).map_err(|source| {
            DiscoveryError::AllocationFailed {
                resource: "directory traversal frame stack",
                source,
            }
        })?;
        let mut entry_batch = Vec::new();
        entry_batch
            .try_reserve_exact(DIRECTORY_ENTRY_BATCH_SIZE)
            .map_err(|source| DiscoveryError::AllocationFailed {
                resource: "directory traversal native-entry batch",
                source,
            })?;
        Ok(Self {
            root,
            max_depth,
            follow_symlinks,
            root_pending: true,
            finished: false,
            last_unit_opened_directory: false,
            frames,
            pending_directory: None,
            entry_batch,
            entry_batch_owner: None,
            next_frame_id: 1,
        })
    }

    /// Returns one native-name-ordered entry after closing and path-validating
    /// the only directory handle used for this unit.
    pub(super) fn next(
        &mut self,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<TraversalEntry>, TraversalFailure> {
        if cancelled() {
            return Err(TraversalFailure::Cancelled);
        }
        self.last_unit_opened_directory = false;
        if let Some(pending) = self.pending_directory.as_ref() {
            return Err(TraversalFailure::Stop(DiscoveryIssue::TraversalResume {
                path: pending.path.clone(),
                reason: "directory entry was yielded without an explicit descend or skip decision",
            }));
        }
        if self.finished {
            return Ok(None);
        }
        if self.root_pending {
            self.root_pending = false;
            let metadata = std::fs::metadata(&self.root).map_err(|source| {
                TraversalFailure::Stop(DiscoveryIssue::TraversalIo {
                    operation: "inspect traversal root",
                    path: self.root.clone(),
                    source,
                })
            })?;
            if cancelled() {
                return Err(TraversalFailure::Cancelled);
            }
            let kind = if metadata.is_dir() {
                TraversalEntryKind::Directory
            } else if metadata.is_file() {
                TraversalEntryKind::RegularFile
            } else {
                TraversalEntryKind::Other
            };
            let entry = TraversalEntry {
                path: self.root.clone(),
                name: OsString::new(),
                depth: 0,
                kind,
                path_is_symlink: false,
            };
            if kind == TraversalEntryKind::Directory {
                self.pending_directory = Some(PendingDirectory {
                    component: None,
                    path: self.root.clone(),
                    depth: 0,
                });
            } else {
                self.finished = true;
            }
            return Ok(Some(entry));
        }

        loop {
            let Some(_) = self.frames.last() else {
                self.finished = true;
                return Ok(None);
            };
            let directory_path = self.current_directory_path();
            if !self.entry_batch.is_empty() {
                let frame = self
                    .frames
                    .last()
                    .expect("a cached entry batch always has an owning frame");
                if self.entry_batch_owner != Some(frame.id) {
                    self.clear_entry_batch();
                    return Err(TraversalFailure::Stop(DiscoveryIssue::TraversalResume {
                        path: directory_path,
                        reason: "cached directory entries lost their owning traversal frame",
                    }));
                }
                let expected = frame
                    .baseline
                    .as_ref()
                    .expect("a cached entry batch always has baseline evidence")
                    .token
                    .clone();
                self.last_unit_opened_directory = true;
                match verify_cached_directory(
                    &directory_path,
                    &expected,
                    self.follow_symlinks,
                    &mut cancelled,
                ) {
                    Ok(CachedDirectoryVerification::Stable) => {
                        return self.take_cached_entry(directory_path);
                    }
                    Ok(CachedDirectoryVerification::Changed) => {
                        self.mark_current_frame_ambiguous(None);
                        return Err(TraversalFailure::Recoverable(
                            DiscoveryIssue::TraversalResume {
                                path: directory_path,
                                reason: "directory change evidence changed before cached resume",
                            },
                        ));
                    }
                    Err(DirectoryScanFailure::Cancelled) => {
                        self.clear_entry_batch();
                        return Err(TraversalFailure::Cancelled);
                    }
                    Err(DirectoryScanFailure::Stop(issue)) => {
                        self.clear_entry_batch();
                        return Err(TraversalFailure::Stop(issue));
                    }
                    Err(DirectoryScanFailure::Unstable(reason)) => {
                        self.mark_current_frame_ambiguous(None);
                        return Err(TraversalFailure::Recoverable(
                            DiscoveryIssue::TraversalResume {
                                path: directory_path,
                                reason,
                            },
                        ));
                    }
                    Err(DirectoryScanFailure::Replaced(reason)) => {
                        self.clear_entry_batch();
                        let _ = self.frames.pop();
                        return Err(TraversalFailure::Recoverable(
                            DiscoveryIssue::TraversalResume {
                                path: directory_path,
                                reason,
                            },
                        ));
                    }
                }
            }
            let (expected, after) = {
                let frame = self
                    .frames
                    .last()
                    .expect("the traversal frame was checked above");
                (frame.baseline.clone(), frame.after.clone())
            };
            self.clear_entry_batch();
            let scan = match scan_directory(
                &directory_path,
                expected.as_ref(),
                after.as_deref(),
                self.follow_symlinks,
                &mut self.entry_batch,
                &mut cancelled,
            ) {
                Ok(DirectoryScanOutcome::Stable(scan)) => scan,
                Ok(DirectoryScanOutcome::Changed(scan)) => {
                    self.last_unit_opened_directory = true;
                    let issue = DiscoveryIssue::TraversalResume {
                        path: directory_path,
                        reason: "directory entry-set resume evidence changed",
                    };
                    self.mark_current_frame_ambiguous(Some(scan.evidence));
                    return Err(TraversalFailure::Recoverable(issue));
                }
                Err(DirectoryScanFailure::Cancelled) => {
                    self.clear_entry_batch();
                    return Err(TraversalFailure::Cancelled);
                }
                Err(DirectoryScanFailure::Stop(issue)) => {
                    self.clear_entry_batch();
                    return Err(TraversalFailure::Stop(issue));
                }
                Err(DirectoryScanFailure::Unstable(reason)) => {
                    let issue = DiscoveryIssue::TraversalResume {
                        path: directory_path,
                        reason,
                    };
                    self.mark_current_frame_ambiguous(None);
                    return Err(TraversalFailure::Recoverable(issue));
                }
                Err(DirectoryScanFailure::Replaced(reason)) => {
                    self.clear_entry_batch();
                    let _ = self.frames.pop();
                    return Err(TraversalFailure::Recoverable(
                        DiscoveryIssue::TraversalResume {
                            path: directory_path,
                            reason,
                        },
                    ));
                }
            };
            self.last_unit_opened_directory = true;
            if cancelled() {
                return Err(TraversalFailure::Cancelled);
            }
            let frame_index = self.frames.len() - 1;
            if self.frames[frame_index].baseline.is_none() {
                let locator = scan.evidence.token.locator;
                if self.frames[..frame_index].iter().any(|ancestor| {
                    ancestor
                        .baseline
                        .as_ref()
                        .is_some_and(|evidence| evidence.token.locator == locator)
                }) {
                    self.clear_entry_batch();
                    let _ = self.frames.pop();
                    return Err(TraversalFailure::Recoverable(
                        DiscoveryIssue::TraversalCycle {
                            path: directory_path,
                            locator,
                        },
                    ));
                }
            }
            if self.frames[frame_index].baseline.is_none() || DIRECTORY_ENTRY_BATCH_SIZE > 1 {
                self.frames[frame_index].baseline = Some(scan.evidence.clone());
            }
            if !self.entry_batch.is_empty() {
                self.entry_batch_owner = Some(self.frames[frame_index].id);
            }
            if let Some(issue) = scan.entry_issue
                && !self.frames[frame_index].entry_issue_reported
            {
                self.frames[frame_index].entry_issue_reported = true;
                return Err(TraversalFailure::Recoverable(issue));
            }
            if self.entry_batch.is_empty() {
                self.clear_entry_batch();
                let _ = self.frames.pop();
                continue;
            }
            return self.take_cached_entry(directory_path);
        }
    }

    /// Descends into the last yielded directory. Scanner policy must call
    /// this or [`Self::skip_current_dir`] before requesting another entry.
    pub(super) fn descend_current_dir(&mut self) -> Result<(), TraversalFailure> {
        let pending = self.pending_directory.take().ok_or_else(|| {
            TraversalFailure::Stop(DiscoveryIssue::TraversalResume {
                path: self.root.clone(),
                reason: "traversal descent was requested without a pending directory",
            })
        })?;
        self.clear_entry_batch();
        if pending.depth >= self.max_depth {
            return Err(TraversalFailure::Recoverable(
                DiscoveryIssue::TraversalDepth {
                    path: pending.path,
                    max_depth: self.max_depth,
                },
            ));
        }
        let frame_id = self.next_frame_id;
        self.next_frame_id = frame_id.checked_add(1).ok_or_else(|| {
            TraversalFailure::Stop(DiscoveryIssue::TraversalResume {
                path: pending.path.clone(),
                reason: "directory traversal frame identity overflowed",
            })
        })?;
        self.frames.push(Frame {
            id: frame_id,
            component: pending.component,
            depth: pending.depth,
            baseline: None,
            after: None,
            recovery_attempts: 0,
            entry_issue_reported: false,
        });
        Ok(())
    }

    /// Intentionally does not descend into the last yielded directory.
    pub(super) fn skip_current_dir(&mut self) -> Result<(), TraversalFailure> {
        let pending = self.pending_directory.take().ok_or_else(|| {
            TraversalFailure::Stop(DiscoveryIssue::TraversalResume {
                path: self.root.clone(),
                reason: "traversal skip was requested without a pending directory",
            })
        })?;
        if pending.depth == 0 {
            self.clear_entry_batch();
            self.finished = true;
        }
        Ok(())
    }

    pub(super) const fn last_unit_opened_directory(&self) -> bool {
        self.last_unit_opened_directory
    }

    fn current_directory_path(&self) -> PathBuf {
        let mut path = self.root.clone();
        for frame in &self.frames {
            if let Some(component) = &frame.component {
                path.push(component);
            }
        }
        path
    }

    #[cfg(all(test, unix))]
    fn retained_frame_capacity(&self) -> usize {
        self.frames.capacity()
    }

    fn clear_entry_batch(&mut self) {
        self.entry_batch.clear();
        self.entry_batch_owner = None;
    }

    fn mark_current_frame_ambiguous(&mut self, baseline: Option<DirectoryEvidence>) {
        self.clear_entry_batch();
        let should_pop = {
            let frame = self
                .frames
                .last_mut()
                .expect("an ambiguous traversal unit has a current frame");
            if frame.recovery_attempts == 0 {
                frame.baseline = baseline;
                frame.recovery_attempts = 1;
                false
            } else {
                true
            }
        };
        if should_pop {
            let _ = self.frames.pop();
        }
    }

    fn take_cached_entry(
        &mut self,
        directory_path: PathBuf,
    ) -> Result<Option<TraversalEntry>, TraversalFailure> {
        let frame_index = self.frames.len() - 1;
        if self.entry_batch_owner != Some(self.frames[frame_index].id) {
            self.clear_entry_batch();
            return Err(TraversalFailure::Stop(DiscoveryIssue::TraversalResume {
                path: directory_path,
                reason: "cached directory entries lost their owning traversal frame",
            }));
        }
        let selected = self.entry_batch.pop().ok_or_else(|| {
            TraversalFailure::Stop(DiscoveryIssue::TraversalResume {
                path: directory_path.clone(),
                reason: "cached directory entry batch was unexpectedly empty",
            })
        })?;
        if self.entry_batch.is_empty() {
            self.entry_batch_owner = None;
        }
        self.frames[frame_index].after = Some(selected.name.clone());
        let entry_path = directory_path.join(&selected.name);
        let entry = TraversalEntry {
            path: entry_path.clone(),
            name: selected.name.clone(),
            depth: self.frames[frame_index]
                .depth
                .checked_add(1)
                .ok_or_else(|| {
                    TraversalFailure::Stop(DiscoveryIssue::TraversalDepth {
                        path: entry_path.clone(),
                        max_depth: self.max_depth,
                    })
                })?,
            kind: selected.kind,
            path_is_symlink: selected.path_is_symlink,
        };
        if entry.kind == TraversalEntryKind::Directory {
            self.pending_directory = Some(PendingDirectory {
                component: Some(entry.name.clone()),
                path: entry.path.clone(),
                depth: entry.depth,
            });
        }
        Ok(Some(entry))
    }
}

struct DirectoryScan {
    evidence: DirectoryEvidence,
    entry_issue: Option<DiscoveryIssue>,
}

enum DirectoryScanOutcome {
    Stable(DirectoryScan),
    Changed(DirectoryScan),
}

#[derive(Debug)]
enum DirectoryScanFailure {
    Cancelled,
    Stop(DiscoveryIssue),
    Unstable(&'static str),
    Replaced(&'static str),
}

enum CachedDirectoryVerification {
    Stable,
    Changed,
}

fn verify_cached_directory(
    path: &Path,
    expected: &DirectoryToken,
    follow_symlinks: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<CachedDirectoryVerification, DirectoryScanFailure> {
    if cancelled() {
        return Err(DirectoryScanFailure::Cancelled);
    }
    let verifier = platform::DirectoryHandle::open(path, follow_symlinks)
        .map_err(|error| platform_failure(path, error))?;
    let verified = verifier
        .token(path)
        .map_err(|error| platform_failure(path, error))?;
    drop(verifier);
    if cancelled() {
        return Err(DirectoryScanFailure::Cancelled);
    }
    if verified.locator != expected.locator {
        return Err(DirectoryScanFailure::Replaced(
            "directory pathname no longer resolves to the cached directory",
        ));
    }
    if verified.change_digest != expected.change_digest {
        return Ok(CachedDirectoryVerification::Changed);
    }
    Ok(CachedDirectoryVerification::Stable)
}

fn scan_directory(
    path: &Path,
    expected: Option<&DirectoryEvidence>,
    after: Option<&OsStr>,
    follow_symlinks: bool,
    selected: &mut Vec<CachedEntry>,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<DirectoryScanOutcome, DirectoryScanFailure> {
    scan_directory_with_hook(
        path,
        expected,
        after,
        follow_symlinks,
        selected,
        cancelled,
        &mut |_| {},
    )
}

fn scan_directory_with_hook(
    path: &Path,
    expected: Option<&DirectoryEvidence>,
    after: Option<&OsStr>,
    follow_symlinks: bool,
    selected: &mut Vec<CachedEntry>,
    cancelled: &mut impl FnMut() -> bool,
    before_path_reopen: &mut impl FnMut(&Path),
) -> Result<DirectoryScanOutcome, DirectoryScanFailure> {
    if cancelled() {
        return Err(DirectoryScanFailure::Cancelled);
    }
    selected.clear();
    debug_assert!(selected.capacity() >= DIRECTORY_ENTRY_BATCH_SIZE);
    note_directory_scan_started();
    let mut directory = platform::DirectoryHandle::open(path, follow_symlinks)
        .map_err(|error| platform_failure(path, error))?;
    let before = directory
        .token(path)
        .map_err(|error| platform_failure(path, error))?;
    let mut accumulator = EntrySetAccumulator::default();
    let mut entry_issue = None;
    loop {
        if cancelled() {
            return Err(DirectoryScanFailure::Cancelled);
        }
        let entry = directory
            .next(follow_symlinks)
            .map_err(|error| platform_failure(path, error))?;
        let Some(mut entry) = entry else {
            break;
        };
        let selectable = entry.issue.is_none();
        if entry_issue.is_none()
            && let Some(issue) = entry.issue.take()
        {
            entry_issue = Some(platform_entry_issue(&path.join(&entry.name), issue));
        }
        accumulator.add(entry.record_digest).map_err(|reason| {
            DirectoryScanFailure::Stop(DiscoveryIssue::TraversalResume {
                path: path.to_path_buf(),
                reason,
            })
        })?;
        let after_match = after.is_none_or(|after| native_name_cmp(&entry.name, after).is_gt());
        if selectable && after_match {
            retain_cached_entry(
                selected,
                CachedEntry {
                    name: entry.name,
                    kind: entry.kind,
                    path_is_symlink: entry.path_is_symlink,
                },
            );
        }
    }
    let after_enumeration = directory
        .token(path)
        .map_err(|error| platform_failure(path, error))?;
    if before != after_enumeration {
        return Err(DirectoryScanFailure::Unstable(
            "directory change evidence changed during enumeration",
        ));
    }
    let evidence = DirectoryEvidence {
        token: after_enumeration.clone(),
        entry_count: accumulator.count,
        entry_set_digest: accumulator.finish(),
    };
    drop(directory);
    if cancelled() {
        return Err(DirectoryScanFailure::Cancelled);
    }
    before_path_reopen(path);
    if cancelled() {
        return Err(DirectoryScanFailure::Cancelled);
    }
    let verifier = platform::DirectoryHandle::open(path, follow_symlinks)
        .map_err(|error| platform_failure(path, error))?;
    let verified = verifier
        .token(path)
        .map_err(|error| platform_failure(path, error))?;
    drop(verifier);
    if verified.locator != after_enumeration.locator {
        return Err(DirectoryScanFailure::Replaced(
            "directory pathname no longer resolves to the enumerated directory",
        ));
    }
    if (DIRECTORY_ENTRY_BATCH_SIZE > 1 || selected.is_empty())
        && verified.change_digest != after_enumeration.change_digest
    {
        return Err(DirectoryScanFailure::Unstable(if selected.is_empty() {
            "directory changed before terminal traversal completion"
        } else {
            "directory changed before cached traversal batch publication"
        }));
    }
    if let Some(expected) = expected {
        if expected.token.locator != evidence.token.locator {
            return Err(DirectoryScanFailure::Replaced(
                "directory locator changed before resume",
            ));
        }
        if (DIRECTORY_ENTRY_BATCH_SIZE > 1
            && expected.token.change_digest != evidence.token.change_digest)
            || expected.entry_count != evidence.entry_count
            || expected.entry_set_digest != evidence.entry_set_digest
        {
            selected.clear();
            return Ok(DirectoryScanOutcome::Changed(DirectoryScan {
                evidence,
                entry_issue,
            }));
        }
    }
    selected.reverse();
    note_entry_batch_len(selected.len());
    Ok(DirectoryScanOutcome::Stable(DirectoryScan {
        evidence,
        entry_issue,
    }))
}

fn retain_cached_entry(entries: &mut Vec<CachedEntry>, entry: CachedEntry) {
    let position = entries
        .binary_search_by(|current| native_name_cmp(&current.name, &entry.name))
        .unwrap_or_else(|position| position);
    if entries.len() < DIRECTORY_ENTRY_BATCH_SIZE {
        entries.insert(position, entry);
    } else if position < DIRECTORY_ENTRY_BATCH_SIZE {
        let _ = entries.pop();
        entries.insert(position, entry);
    }
}

fn platform_failure(path: &Path, error: PlatformError) -> DirectoryScanFailure {
    match error {
        PlatformError::Io { operation, source } => {
            DirectoryScanFailure::Stop(DiscoveryIssue::TraversalIo {
                operation,
                path: path.to_path_buf(),
                source,
            })
        }

        PlatformError::Ambiguous { reason } => DirectoryScanFailure::Unstable(reason),
    }
}

fn platform_entry_issue(path: &Path, issue: PlatformEntryIssue) -> DiscoveryIssue {
    match issue {
        PlatformEntryIssue::Io { operation, source } => DiscoveryIssue::TraversalIo {
            operation,
            path: path.to_path_buf(),
            source,
        },
        PlatformEntryIssue::Ambiguous { reason } => DiscoveryIssue::TraversalResume {
            path: path.to_path_buf(),
            reason,
        },
    }
}

#[derive(Debug)]
enum PlatformError {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    Ambiguous {
        reason: &'static str,
    },
}

#[derive(Debug)]
struct NativeEntry {
    name: OsString,
    kind: TraversalEntryKind,
    path_is_symlink: bool,
    record_digest: [u8; 32],
    issue: Option<PlatformEntryIssue>,
}

#[derive(Debug)]
#[cfg_attr(
    windows,
    allow(
        dead_code,
        reason = "entry-local issues are constructed only by the POSIX traversal backend"
    )
)]
enum PlatformEntryIssue {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    Ambiguous {
        reason: &'static str,
    },
}

impl NativeEntry {
    fn new(
        name: OsString,
        locator: Locator,
        kind: TraversalEntryKind,
        path_is_symlink: bool,
        reparse_tag: u32,
    ) -> Self {
        let record_digest =
            directory_entry_digest(&name, locator, kind, path_is_symlink, reparse_tag);
        Self {
            name,
            kind,
            path_is_symlink,
            record_digest,
            issue: None,
        }
    }

    #[cfg_attr(
        windows,
        allow(
            dead_code,
            reason = "entry-local issues are constructed only by the POSIX traversal backend"
        )
    )]
    fn with_issue(
        name: OsString,
        locator: Locator,
        path_is_symlink: bool,
        issue: PlatformEntryIssue,
    ) -> Self {
        let kind = TraversalEntryKind::Other;
        let record_digest = directory_entry_digest(&name, locator, kind, path_is_symlink, 0);
        Self {
            name,
            kind,
            path_is_symlink,
            record_digest,
            issue: Some(issue),
        }
    }
}

#[derive(Default)]
struct EntrySetAccumulator {
    count: u64,
    xor: [u8; 32],
    sum: [u8; 32],
}

impl EntrySetAccumulator {
    fn add(&mut self, digest: [u8; 32]) -> Result<(), &'static str> {
        self.count = self
            .count
            .checked_add(1)
            .ok_or("directory entry count overflowed u64")?;
        for (target, value) in self.xor.iter_mut().zip(digest) {
            *target ^= value;
        }
        let mut carry = 0u16;
        for index in (0..self.sum.len()).rev() {
            let total = u16::from(self.sum[index]) + u16::from(digest[index]) + carry;
            self.sum[index] = total as u8;
            carry = total >> 8;
        }
        Ok(())
    }

    fn finish(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(DIRECTORY_ENTRY_SET_DIGEST_DOMAIN);
        hasher.update(self.count.to_be_bytes());
        hasher.update(self.xor);
        hasher.update(self.sum);
        hasher.finalize().into()
    }
}

fn directory_entry_digest(
    name: &OsStr,
    locator: Locator,
    kind: TraversalEntryKind,
    path_is_symlink: bool,
    reparse_tag: u32,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DIRECTORY_ENTRY_DIGEST_DOMAIN);
    hash_native_name(&mut hasher, name);
    match locator {
        Locator::PosixDevIno { dev, ino } => {
            hasher.update([1]);
            hasher.update(dev.to_be_bytes());
            hasher.update(ino.to_be_bytes());
        }
        Locator::WindowsVolumeFileId {
            volume_serial,
            file_id,
        } => {
            hasher.update([2]);
            hasher.update(volume_serial.to_be_bytes());
            hasher.update(file_id);
        }
        Locator::Unspecified => hasher.update([0]),
    }
    hasher.update([match kind {
        TraversalEntryKind::Directory => 1,
        TraversalEntryKind::RegularFile => 2,
        TraversalEntryKind::Other => 3,
    }]);
    hasher.update([u8::from(path_is_symlink)]);
    hasher.update(reparse_tag.to_be_bytes());
    hasher.finalize().into()
}

#[cfg(unix)]
fn hash_native_name(hasher: &mut Sha256, name: &OsStr) {
    use std::os::unix::ffi::OsStrExt;

    let bytes = name.as_bytes();
    hasher.update([1]);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(windows)]
fn hash_native_name(hasher: &mut Sha256, name: &OsStr) {
    use std::os::windows::ffi::OsStrExt;

    let units = name.encode_wide();
    let count = units.clone().count();
    hasher.update([2]);
    hasher.update((count as u64).to_be_bytes());
    for unit in units {
        hasher.update(unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn hash_native_name(hasher: &mut Sha256, name: &OsStr) {
    let text = name.to_string_lossy();
    hasher.update([0]);
    hasher.update((text.len() as u64).to_be_bytes());
    hasher.update(text.as_bytes());
}

#[cfg(unix)]
fn native_name_cmp(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::unix::ffi::OsStrExt;

    left.as_bytes().cmp(right.as_bytes())
}

#[cfg(windows)]
fn native_name_cmp(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt;

    left.encode_wide().cmp(right.encode_wide())
}

#[cfg(not(any(unix, windows)))]
fn native_name_cmp(left: &OsStr, right: &OsStr) -> Ordering {
    left.cmp(right)
}

fn directory_change_digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DIRECTORY_CHANGE_DIGEST_DOMAIN);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

#[cfg(test)]
thread_local! {
    static OPEN_DIRECTORY_HANDLES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEAK_DIRECTORY_HANDLES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FULL_DIRECTORY_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static MAX_ENTRY_BATCH_LEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static NEXT_DIRECTORY_OPEN_ERROR: std::cell::RefCell<Option<io::Error>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn note_directory_handle_opened() {
    OPEN_DIRECTORY_HANDLES.with(|current| {
        let next = current
            .get()
            .checked_add(1)
            .expect("test handle count fits");
        current.set(next);
        PEAK_DIRECTORY_HANDLES.with(|peak| peak.set(peak.get().max(next)));
    });
}

#[cfg(not(test))]
fn note_directory_handle_opened() {}

#[cfg(test)]
fn note_directory_handle_closed() {
    OPEN_DIRECTORY_HANDLES.with(|current| {
        current.set(
            current
                .get()
                .checked_sub(1)
                .expect("directory handle close has a matching open"),
        );
    });
}

#[cfg(not(test))]
fn note_directory_handle_closed() {}

#[cfg(test)]
fn note_directory_scan_started() {
    FULL_DIRECTORY_SCANS.with(|scans| {
        scans.set(
            scans
                .get()
                .checked_add(1)
                .expect("test directory scan count fits"),
        );
    });
}

#[cfg(not(test))]
fn note_directory_scan_started() {}

#[cfg(test)]
fn note_entry_batch_len(len: usize) {
    MAX_ENTRY_BATCH_LEN.with(|maximum| maximum.set(maximum.get().max(len)));
}

#[cfg(not(test))]
fn note_entry_batch_len(_len: usize) {}

#[cfg(test)]
pub(super) fn reset_directory_handle_observations() {
    OPEN_DIRECTORY_HANDLES.with(|current| current.set(0));
    PEAK_DIRECTORY_HANDLES.with(|peak| peak.set(0));
    FULL_DIRECTORY_SCANS.with(|scans| scans.set(0));
    MAX_ENTRY_BATCH_LEN.with(|maximum| maximum.set(0));
}

#[cfg(test)]
pub(super) fn peak_directory_handles() -> usize {
    PEAK_DIRECTORY_HANDLES.with(std::cell::Cell::get)
}

#[cfg(all(test, target_os = "linux"))]
fn full_directory_scans() -> usize {
    FULL_DIRECTORY_SCANS.with(std::cell::Cell::get)
}

#[cfg(all(test, target_os = "linux"))]
fn max_entry_batch_len() -> usize {
    MAX_ENTRY_BATCH_LEN.with(std::cell::Cell::get)
}

#[cfg(all(test, unix))]
pub(super) fn current_directory_handles() -> usize {
    OPEN_DIRECTORY_HANDLES.with(std::cell::Cell::get)
}

#[cfg(all(test, unix))]
pub(super) fn fail_next_directory_open(error: io::Error) {
    NEXT_DIRECTORY_OPEN_ERROR.with(|next| {
        *next.borrow_mut() = Some(error);
    });
}

#[cfg(test)]
fn take_directory_open_error() -> Option<io::Error> {
    NEXT_DIRECTORY_OPEN_ERROR.with(|next| next.borrow_mut().take())
}

#[cfg(not(test))]
fn take_directory_open_error() -> Option<io::Error> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(
    unsafe_code,
    reason = "POSIX directory enumeration requires owning DIR*, readdir, fstatat, and errno"
)]
mod platform {
    use std::ffi::{CStr, OsString};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::mem::{ManuallyDrop, MaybeUninit};
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;
    use std::ptr::NonNull;

    use super::{
        DirectoryToken, NativeEntry, PlatformEntryIssue, PlatformError, TraversalEntryKind,
        directory_change_digest, note_directory_handle_closed, note_directory_handle_opened,
    };
    use crate::receivers::filelog_receiver::checkpoint::Locator;

    pub(super) struct DirectoryHandle {
        directory: NonNull<libc::DIR>,
    }

    impl DirectoryHandle {
        pub(super) fn open(path: &Path, follow_symlinks: bool) -> Result<Self, PlatformError> {
            if let Some(source) = super::take_directory_open_error() {
                return Err(PlatformError::Io {
                    operation: "open traversal directory",
                    source,
                });
            }
            let mut options = OpenOptions::new();
            let _ = options.read(true);
            let mut flags = libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NONBLOCK;
            if !follow_symlinks {
                flags |= libc::O_NOFOLLOW;
            }
            let file =
                options
                    .custom_flags(flags)
                    .open(path)
                    .map_err(|source| PlatformError::Io {
                        operation: "open traversal directory",
                        source,
                    })?;
            let descriptor = file.into_raw_fd();
            // SAFETY: `descriptor` is a newly owned directory descriptor.
            // On success `fdopendir` assumes sole ownership; on failure this
            // function closes the descriptor exactly once below.
            let directory = unsafe { libc::fdopendir(descriptor) };
            let Some(directory) = NonNull::new(directory) else {
                let source = io::Error::last_os_error();
                // SAFETY: `fdopendir` failed and therefore did not consume
                // the still-owned descriptor.
                let _ = unsafe { libc::close(descriptor) };
                return Err(PlatformError::Io {
                    operation: "create traversal directory stream",
                    source,
                });
            };
            note_directory_handle_opened();
            Ok(Self { directory })
        }

        pub(super) fn token(&self, path: &Path) -> Result<DirectoryToken, PlatformError> {
            // SAFETY: `self.directory` is a live `DIR*` owned by this value.
            let descriptor = unsafe { libc::dirfd(self.directory.as_ptr()) };
            if descriptor < 0 {
                return Err(PlatformError::Io {
                    operation: "inspect traversal directory descriptor",
                    source: io::Error::last_os_error(),
                });
            }
            // SAFETY: the descriptor remains owned by `DIR*`; ManuallyDrop
            // prevents the temporary `File` view from closing it.
            let file = ManuallyDrop::new(unsafe { File::from_raw_fd(descriptor) });
            let metadata = file.metadata().map_err(|source| PlatformError::Io {
                operation: "read traversal directory metadata",
                source,
            })?;
            if !metadata.is_dir() {
                return Err(PlatformError::Ambiguous {
                    reason: "opened traversal handle is no longer a directory",
                });
            }
            let locator = Locator::PosixDevIno {
                dev: metadata.dev(),
                ino: metadata.ino(),
            };
            let mode = metadata.mode().to_be_bytes();
            let links = metadata.nlink().to_be_bytes();
            let size = metadata.size().to_be_bytes();
            let blocks = metadata.blocks().to_be_bytes();
            let modified_seconds = metadata.mtime().to_be_bytes();
            let modified_nanoseconds = metadata.mtime_nsec().to_be_bytes();
            let changed_seconds = metadata.ctime().to_be_bytes();
            let changed_nanoseconds = metadata.ctime_nsec().to_be_bytes();
            let path_bytes = {
                use std::os::unix::ffi::OsStrExt;
                path.as_os_str().as_bytes()
            };
            Ok(DirectoryToken {
                locator,
                change_digest: directory_change_digest(&[
                    path_bytes,
                    &mode,
                    &links,
                    &size,
                    &blocks,
                    &modified_seconds,
                    &modified_nanoseconds,
                    &changed_seconds,
                    &changed_nanoseconds,
                ]),
            })
        }

        #[allow(
            trivial_numeric_casts,
            reason = "libc dev_t and ino_t widths differ between Linux and macOS"
        )]
        pub(super) fn next(
            &mut self,
            follow_symlinks: bool,
        ) -> Result<Option<NativeEntry>, PlatformError> {
            loop {
                clear_errno();
                // SAFETY: `self.directory` is a live directory stream and
                // this method has exclusive access while `readdir` advances
                // its internal cursor.
                let entry = unsafe { libc::readdir(self.directory.as_ptr()) };
                if entry.is_null() {
                    let source = io::Error::last_os_error();
                    if source.raw_os_error().unwrap_or(0) == 0 {
                        return Ok(None);
                    }
                    return Err(PlatformError::Io {
                        operation: "enumerate traversal directory",
                        source,
                    });
                }
                // SAFETY: `readdir` returned a valid pointer whose `d_name`
                // remains valid until the next directory-stream operation.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                let bytes = name.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                let native_name = OsString::from_vec(bytes.to_vec());
                // SAFETY: `self.directory` remains live and owns this fd.
                let descriptor = unsafe { libc::dirfd(self.directory.as_ptr()) };
                let nofollow =
                    match stat_entry_optional(descriptor, name, libc::AT_SYMLINK_NOFOLLOW) {
                        Ok(Some(metadata)) => metadata,
                        Ok(None) => {
                            return Ok(Some(NativeEntry::with_issue(
                                native_name,
                                Locator::Unspecified,
                                false,
                                PlatformEntryIssue::Ambiguous {
                                    reason: "directory entry disappeared during enumeration",
                                },
                            )));
                        }
                        Err(error) => {
                            return Ok(Some(NativeEntry::with_issue(
                                native_name,
                                Locator::Unspecified,
                                false,
                                entry_issue(error),
                            )));
                        }
                    };
                let nofollow_kind = mode_kind(nofollow.st_mode);
                let path_is_symlink = nofollow_kind == ModeKind::Symlink;
                let locator = Locator::PosixDevIno {
                    dev: nofollow.st_dev as u64,
                    ino: nofollow.st_ino as u64,
                };
                let effective = if path_is_symlink && follow_symlinks {
                    match stat_entry_optional(descriptor, name, 0) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            return Ok(Some(NativeEntry::with_issue(
                                native_name,
                                locator,
                                true,
                                entry_issue(error),
                            )));
                        }
                    }
                } else {
                    Some(nofollow)
                };
                let kind = match effective.map(|metadata| mode_kind(metadata.st_mode)) {
                    Some(ModeKind::Directory) => TraversalEntryKind::Directory,
                    Some(ModeKind::Regular) => TraversalEntryKind::RegularFile,
                    Some(ModeKind::Symlink | ModeKind::Other) | None => {
                        // A dangling followed symlink is positive evidence for
                        // a non-candidate entry, not a permanent traversal
                        // failure for every later native name.
                        TraversalEntryKind::Other
                    }
                };
                return Ok(Some(NativeEntry::new(
                    native_name,
                    locator,
                    kind,
                    path_is_symlink,
                    0,
                )));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::ffi::{OsStr, OsString};
        use std::fs::{File, FileTimes};
        use std::path::PathBuf;
        use std::time::{Duration, SystemTime};

        use super::super::*;

        fn descend_root(traversal: &mut BoundedTraversal) {
            let root = traversal.next(|| false).unwrap().unwrap();
            assert_eq!(root.depth(), 0);
            assert_eq!(root.kind(), TraversalEntryKind::Directory);
            traversal.descend_current_dir().unwrap();
        }

        fn collect_regular_files(traversal: &mut BoundedTraversal) -> Vec<PathBuf> {
            let mut files = Vec::new();
            while let Some(entry) = traversal.next(|| false).unwrap() {
                match entry.kind() {
                    TraversalEntryKind::Directory => traversal.descend_current_dir().unwrap(),
                    TraversalEntryKind::RegularFile => files.push(entry.path().to_path_buf()),
                    TraversalEntryKind::Other => {}
                }
            }
            files
        }

        fn entry_batch() -> Vec<CachedEntry> {
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(DIRECTORY_ENTRY_BATCH_SIZE)
                .unwrap();
            entries
        }

        fn set_directory_mtime(path: &Path, seconds: u64) {
            let directory = File::open(path).unwrap();
            directory
                .set_times(
                    FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .unwrap();
        }

        /// Scenario: identical entry records are accumulated in opposite backend
        /// enumeration orders.
        /// Guarantees: count, XOR, modular sum, and final domain-separated digest
        /// form one fixed-memory order-independent entry-set evidence value.
        #[test]
        fn entry_set_evidence_is_order_independent() {
            let first = directory_entry_digest(
                OsStr::new("a"),
                Locator::PosixDevIno { dev: 1, ino: 2 },
                TraversalEntryKind::RegularFile,
                false,
                0,
            );
            let second = directory_entry_digest(
                OsStr::new("b"),
                Locator::PosixDevIno { dev: 1, ino: 3 },
                TraversalEntryKind::Directory,
                false,
                0,
            );
            let mut forward = EntrySetAccumulator::default();
            forward.add(first).unwrap();
            forward.add(second).unwrap();
            let mut reverse = EntrySetAccumulator::default();
            reverse.add(second).unwrap();
            reverse.add(first).unwrap();

            assert_eq!(forward.count, 2);
            assert_eq!(forward.finish(), reverse.finish());
        }

        /// Scenario: files are created in reverse lexical order under one
        /// traversal root.
        /// Guarantees: yielded entry order follows complete native names rather
        /// than filesystem enumeration or creation order.
        #[test]
        fn traversal_selects_deterministic_native_name_order() {
            let directory = tempfile::tempdir().unwrap();
            for name in ["z.log", "m.log", "a.log"] {
                std::fs::write(directory.path().join(name), name).unwrap();
            }
            reset_directory_handle_observations();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);

            let names: Vec<_> = collect_regular_files(&mut traversal)
                .into_iter()
                .map(|path| path.file_name().unwrap().to_owned())
                .collect();

            assert_eq!(
                names,
                [
                    OsString::from("a.log"),
                    OsString::from("m.log"),
                    OsString::from("z.log")
                ]
            );
            assert_eq!(peak_directory_handles(), 1);
            assert_eq!(current_directory_handles(), 0);
        }

        /// Scenario: a directory contains 256 regular files while the
        /// configured traversal depth remains one.
        /// Guarantees: every entry is visited, frame and entry-batch capacity
        /// are independent of directory width, Linux uses one complete refill
        /// plus one terminal scan, and only one native directory handle is live.
        #[test]
        fn wide_directory_keeps_bounded_traversal_state() {
            let directory = tempfile::tempdir().unwrap();
            for index in 0..256 {
                std::fs::write(directory.path().join(format!("{index:04}.log")), b"line").unwrap();
            }
            reset_directory_handle_observations();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            let initial_capacity = traversal.retained_frame_capacity();
            let initial_entry_capacity = traversal.entry_batch.capacity();
            descend_root(&mut traversal);

            let files = collect_regular_files(&mut traversal);

            assert_eq!(files.len(), 256);
            assert_eq!(traversal.retained_frame_capacity(), initial_capacity);
            assert_eq!(traversal.entry_batch.capacity(), initial_entry_capacity);
            assert!(initial_entry_capacity >= DIRECTORY_ENTRY_BATCH_SIZE);
            #[cfg(target_os = "linux")]
            {
                assert_eq!(full_directory_scans(), 2);
                assert_eq!(max_entry_batch_len(), DIRECTORY_ENTRY_BATCH_SIZE);
            }
            assert_eq!(peak_directory_handles(), 1);
            assert_eq!(current_directory_handles(), 0);
        }

        /// Scenario: stable Linux directories contain populations immediately
        /// below, at, and above one or two fixed native-entry batches.
        /// Guarantees: complete scans equal `ceil(entries / batch) + 1`, the
        /// logical batch never exceeds its fixed bound, and capacity never grows.
        #[cfg(target_os = "linux")]
        #[test]
        fn linux_entry_batch_boundaries_have_fixed_scan_and_memory_counts() {
            for count in [255usize, 256, 257, 512] {
                let directory = tempfile::tempdir().unwrap();
                for index in 0..count {
                    std::fs::write(directory.path().join(format!("{index:04}.log")), b"line")
                        .unwrap();
                }
                reset_directory_handle_observations();
                let mut traversal =
                    BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
                let initial_entry_capacity = traversal.entry_batch.capacity();
                descend_root(&mut traversal);

                let files = collect_regular_files(&mut traversal);

                assert_eq!(files.len(), count);
                assert_eq!(traversal.entry_batch.capacity(), initial_entry_capacity);
                assert_eq!(
                    full_directory_scans(),
                    count.div_ceil(DIRECTORY_ENTRY_BATCH_SIZE) + 1
                );
                assert_eq!(max_entry_batch_len(), count.min(DIRECTORY_ENTRY_BATCH_SIZE));
                assert_eq!(peak_directory_handles(), 1);
                assert_eq!(current_directory_handles(), 0);
            }
        }

        /// Scenario: the first and then a middle entry in a full parent batch
        /// are directories with regular-file siblings after them.
        /// Guarantees: descent discards cached parent names, resumes from only
        /// the last yielded name, and later siblings are rediscovered in order.
        #[cfg(target_os = "linux")]
        #[test]
        fn batched_parent_siblings_are_rediscovered_after_descent() {
            let first_parent = tempfile::tempdir().unwrap();
            std::fs::create_dir(first_parent.path().join("a-dir")).unwrap();
            std::fs::write(first_parent.path().join("a-dir/inside.log"), b"inside").unwrap();
            std::fs::write(first_parent.path().join("b.log"), b"b").unwrap();
            std::fs::write(first_parent.path().join("c.log"), b"c").unwrap();
            let mut first =
                BoundedTraversal::new(first_parent.path().to_path_buf(), 2, false).unwrap();
            descend_root(&mut first);
            assert_eq!(
                collect_regular_files(&mut first),
                [
                    first_parent.path().join("a-dir/inside.log"),
                    first_parent.path().join("b.log"),
                    first_parent.path().join("c.log"),
                ]
            );

            let middle_parent = tempfile::tempdir().unwrap();
            std::fs::write(middle_parent.path().join("a.log"), b"a").unwrap();
            std::fs::create_dir(middle_parent.path().join("b-dir")).unwrap();
            std::fs::write(middle_parent.path().join("b-dir/inside.log"), b"inside").unwrap();
            std::fs::write(middle_parent.path().join("c.log"), b"c").unwrap();
            let mut middle =
                BoundedTraversal::new(middle_parent.path().to_path_buf(), 2, false).unwrap();
            descend_root(&mut middle);
            assert_eq!(
                collect_regular_files(&mut middle),
                [
                    middle_parent.path().join("a.log"),
                    middle_parent.path().join("b-dir/inside.log"),
                    middle_parent.path().join("c.log"),
                ]
            );
        }

        /// Scenario: traversal descends through 256 one-child directories
        /// before reaching one regular file.
        /// Guarantees: recursion retains only one bounded frame per configured
        /// depth and never retains an ancestor directory handle.
        #[test]
        fn deep_tree_reopens_ancestors_with_one_handle() {
            let directory = tempfile::tempdir().unwrap();
            let mut leaf = directory.path().to_path_buf();
            for index in 0..256 {
                leaf.push(format!("d{index:03}"));
                std::fs::create_dir(&leaf).unwrap();
            }
            std::fs::write(leaf.join("deep.log"), b"deep").unwrap();
            reset_directory_handle_observations();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 257, false).unwrap();
            descend_root(&mut traversal);

            let files = collect_regular_files(&mut traversal);

            assert_eq!(files, [leaf.join("deep.log")]);
            assert_eq!(peak_directory_handles(), 1);
            assert_eq!(current_directory_handles(), 0);
        }

        /// Scenario: cancellation becomes true while a wide directory is being
        /// streamed for its next deterministic entry.
        /// Guarantees: traversal checks cancellation between entries, returns a
        /// cancellation outcome, and drops the active directory handle.
        #[test]
        fn cancellation_interrupts_directory_stream_and_closes_handle() {
            let directory = tempfile::tempdir().unwrap();
            for index in 0..128 {
                std::fs::write(directory.path().join(format!("{index:03}.log")), b"x").unwrap();
            }
            reset_directory_handle_observations();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);
            let mut checks = 0usize;

            let result = traversal.next(|| {
                checks += 1;
                checks > 32
            });

            assert!(matches!(result, Err(TraversalFailure::Cancelled)));
            assert_eq!(peak_directory_handles(), 1);
            assert_eq!(current_directory_handles(), 0);
        }

        /// Scenario: the traversal root is replaced after one entry is yielded
        /// and before the next bounded unit reopens it.
        /// Guarantees: locator or entry-set evidence mismatch rejects resume
        /// instead of transferring the old directory's cursor to the replacement.
        #[test]
        fn replacement_between_units_makes_resume_ambiguous() {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("root");
            let old = parent.path().join("old");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("a.log"), b"a").unwrap();
            std::fs::write(root.join("b.log"), b"b").unwrap();
            let mut traversal = BoundedTraversal::new(root.clone(), 1, false).unwrap();
            descend_root(&mut traversal);
            assert!(traversal.next(|| false).unwrap().is_some());
            std::fs::rename(&root, &old).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("c.log"), b"c").unwrap();

            let result = traversal.next(|| false);

            assert!(matches!(
                result,
                Err(TraversalFailure::Recoverable(
                    DiscoveryIssue::TraversalResume { .. }
                ))
            ));
            assert!(traversal.next(|| false).unwrap().is_none());
        }

        /// Scenario: a new native name is inserted before the last yielded
        /// resume name between two bounded units.
        /// Guarantees: the order-independent complete entry-set evidence
        /// detects the mutation and refuses to continue from the stale cursor.
        #[test]
        fn entry_set_mutation_before_cursor_makes_resume_ambiguous() {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("b.log"), b"b").unwrap();
            std::fs::write(directory.path().join("c.log"), b"c").unwrap();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);
            let first = traversal.next(|| false).unwrap().unwrap();
            assert!(first.path().ends_with("b.log"));
            std::fs::write(directory.path().join("a.log"), b"a").unwrap();

            let result = traversal.next(|| false);

            assert!(matches!(
                result,
                Err(TraversalFailure::Recoverable(
                    DiscoveryIssue::TraversalResume { .. }
                ))
            ));
            let resumed = traversal.next(|| false).unwrap().unwrap();
            assert!(resumed.path().ends_with("c.log"));
        }

        /// Scenario: Linux caches `a.log` and `c.log`, then `b.log` is inserted
        /// inside the unconsumed native-name range after `a.log` is yielded.
        /// Guarantees: token change invalidates the cache, marks the pass
        /// incomplete, and the replacement scan yields `b.log` before `c.log`.
        #[cfg(target_os = "linux")]
        #[test]
        fn insertion_inside_cached_range_is_not_skipped() {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("a.log"), b"a").unwrap();
            std::fs::write(directory.path().join("c.log"), b"c").unwrap();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);
            let first = traversal.next(|| false).unwrap().unwrap();
            assert!(first.path().ends_with("a.log"));
            std::fs::write(directory.path().join("b.log"), b"b").unwrap();

            assert!(matches!(
                traversal.next(|| false),
                Err(TraversalFailure::Recoverable(
                    DiscoveryIssue::TraversalResume { .. }
                ))
            ));
            assert!(
                traversal
                    .next(|| false)
                    .unwrap()
                    .unwrap()
                    .path()
                    .ends_with("b.log")
            );
            assert!(
                traversal
                    .next(|| false)
                    .unwrap()
                    .unwrap()
                    .path()
                    .ends_with("c.log")
            );
        }

        /// Scenario: Linux directory metadata changes without changing its
        /// entry set while cached names remain.
        /// Guarantees: the observed token change still marks the pass
        /// incomplete before traversal establishes new evidence and continues.
        #[cfg(target_os = "linux")]
        #[test]
        fn token_change_with_unchanged_entry_set_marks_pass_incomplete() {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("a.log"), b"a").unwrap();
            std::fs::write(directory.path().join("b.log"), b"b").unwrap();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);
            assert!(
                traversal
                    .next(|| false)
                    .unwrap()
                    .unwrap()
                    .path()
                    .ends_with("a.log")
            );
            set_directory_mtime(directory.path(), 1_000_000);

            assert!(matches!(
                traversal.next(|| false),
                Err(TraversalFailure::Recoverable(
                    DiscoveryIssue::TraversalResume { .. }
                ))
            ));
            assert!(
                traversal
                    .next(|| false)
                    .unwrap()
                    .unwrap()
                    .path()
                    .ends_with("b.log")
            );
        }

        /// Scenario: Linux directory change evidence is disturbed twice while
        /// cached names remain in the same frame.
        /// Guarantees: one rebase is allowed, the second ambiguity prunes the
        /// frame, and the traversal cannot spin indefinitely.
        #[cfg(target_os = "linux")]
        #[test]
        fn repeated_token_churn_prunes_the_frame_after_one_rebase() {
            let directory = tempfile::tempdir().unwrap();
            for name in ["a.log", "b.log", "c.log"] {
                std::fs::write(directory.path().join(name), name).unwrap();
            }
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);
            assert!(traversal.next(|| false).unwrap().is_some());

            set_directory_mtime(directory.path(), 1_000_000);
            assert!(matches!(
                traversal.next(|| false),
                Err(TraversalFailure::Recoverable(
                    DiscoveryIssue::TraversalResume { .. }
                ))
            ));
            assert!(traversal.next(|| false).unwrap().is_some());

            set_directory_mtime(directory.path(), 1_000_001);
            assert!(matches!(
                traversal.next(|| false),
                Err(TraversalFailure::Recoverable(
                    DiscoveryIssue::TraversalResume { .. }
                ))
            ));
            assert!(traversal.next(|| false).unwrap().is_none());
        }

        /// Scenario: Linux adds an entry after enumeration closes but before
        /// the pathname verifier opens for a nonterminal refill.
        /// Guarantees: no cached name is published under a token that was not
        /// produced by the same complete enumeration.
        #[cfg(target_os = "linux")]
        #[test]
        fn refill_mutation_before_path_verifier_discards_the_batch() {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("a.log"), b"a").unwrap();
            std::fs::write(directory.path().join("b.log"), b"b").unwrap();
            let mut entries = entry_batch();

            let result = scan_directory_with_hook(
                directory.path(),
                None,
                None,
                false,
                &mut entries,
                &mut || false,
                &mut |path| {
                    std::fs::write(path.join("c.log"), b"c").unwrap();
                },
            );

            assert!(matches!(
                result,
                Err(DirectoryScanFailure::Unstable(
                    "directory changed before cached traversal batch publication"
                ))
            ));
        }

        /// Scenario: the root pathname is replaced after the terminal rescan has
        /// found no successor but before the sequential pathname verifier opens.
        /// Guarantees: the final no-successor unit cannot declare a detached old
        /// directory complete under a replacement pathname.
        #[test]
        fn terminal_path_replacement_is_detected_before_completion() {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("root");
            let old = parent.path().join("old");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("only.log"), b"x").unwrap();
            let mut entries = entry_batch();
            let DirectoryScanOutcome::Stable(first) =
                scan_directory(&root, None, None, false, &mut entries, &mut || false).unwrap()
            else {
                panic!("the first stable scan must establish a baseline");
            };
            let after = entries.pop().unwrap().name;
            let baseline = first.evidence;

            let result = scan_directory_with_hook(
                &root,
                Some(&baseline),
                Some(&after),
                false,
                &mut entries,
                &mut || false,
                &mut |_| {
                    std::fs::rename(&root, &old).unwrap();
                    std::fs::create_dir(&root).unwrap();
                },
            );

            assert!(matches!(result, Err(DirectoryScanFailure::Replaced(_))));
        }

        /// Scenario: traversal is asked to descend into a directory already at
        /// the configured maximum entry depth.
        /// Guarantees: exhaustion is explicit and incomplete rather than a silent
        /// clean truncation of the subtree.
        #[test]
        fn requested_descent_at_depth_bound_is_rejected() {
            let directory = tempfile::tempdir().unwrap();
            std::fs::create_dir(directory.path().join("child")).unwrap();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);
            let child = traversal.next(|| false).unwrap().unwrap();
            assert_eq!(child.kind(), TraversalEntryKind::Directory);
            assert_eq!(child.depth(), 1);

            assert!(matches!(
                traversal.descend_current_dir(),
                Err(TraversalFailure::Recoverable(
                    DiscoveryIssue::TraversalDepth { max_depth: 1, .. }
                ))
            ));
        }

        /// Scenario: a new entry is created after a terminal scan finds no
        /// successor but before the sequential pathname verifier opens.
        /// Guarantees: terminal completion requires the final directory
        /// change token, so the pass cannot fabricate absence across the gap.
        #[test]
        fn terminal_entry_insertion_is_detected_before_completion() {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("only.log"), b"x").unwrap();
            let mut entries = entry_batch();
            let DirectoryScanOutcome::Stable(first) = scan_directory(
                directory.path(),
                None,
                None,
                false,
                &mut entries,
                &mut || false,
            )
            .unwrap() else {
                panic!("the first stable scan must establish a baseline");
            };
            let after = entries.pop().unwrap().name;
            let baseline = first.evidence;

            let result = scan_directory_with_hook(
                directory.path(),
                Some(&baseline),
                Some(&after),
                false,
                &mut entries,
                &mut || false,
                &mut |path| {
                    std::fs::create_dir(path.join("inserted")).unwrap();
                },
            );

            assert!(matches!(
                result,
                Err(DirectoryScanFailure::Unstable(
                    "directory changed before terminal traversal completion"
                ))
            ));
        }

        /// Scenario: the traversal root is one exact regular file rather than a
        /// directory.
        /// Guarantees: literal file roots remain one valid depth-zero unit and do
        /// not require a directory handle.
        #[test]
        fn exact_regular_file_root_is_preserved() {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("exact.log");
            std::fs::write(&file, b"line").unwrap();
            reset_directory_handle_observations();
            let mut traversal = BoundedTraversal::new(file.clone(), 1, false).unwrap();

            let entry = traversal.next(|| false).unwrap().unwrap();

            assert_eq!(entry.path(), file);
            assert_eq!(entry.depth(), 0);
            assert_eq!(entry.kind(), TraversalEntryKind::RegularFile);
            assert!(traversal.next(|| false).unwrap().is_none());
            assert_eq!(peak_directory_handles(), 0);
        }

        #[cfg(unix)]
        /// Scenario: a directory contains a filename whose native bytes are not
        /// valid UTF-8.
        /// Guarantees: ordering, hashing, and yielded path preservation use exact
        /// native bytes without lossy conversion.
        #[test]
        fn unix_invalid_utf8_name_round_trips() {
            use std::os::unix::ffi::{OsStrExt, OsStringExt};

            let directory = tempfile::tempdir().unwrap();
            let name = OsString::from_vec(vec![b'a', 0xff, b'.', b'l', b'o', b'g']);
            std::fs::write(directory.path().join(&name), b"line").unwrap();
            let mut traversal =
                BoundedTraversal::new(directory.path().to_path_buf(), 1, false).unwrap();
            descend_root(&mut traversal);

            let files = collect_regular_files(&mut traversal);

            assert_eq!(files.len(), 1);
            assert_eq!(
                files[0].file_name().unwrap().as_bytes(),
                name.as_os_str().as_bytes()
            );
        }
    }

    impl Drop for DirectoryHandle {
        fn drop(&mut self) {
            // SAFETY: this value owns the live `DIR*` exactly once.
            let _ = unsafe { libc::closedir(self.directory.as_ptr()) };
            note_directory_handle_closed();
        }
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum ModeKind {
        Directory,
        Regular,
        Symlink,
        Other,
    }

    fn mode_kind(mode: libc::mode_t) -> ModeKind {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => ModeKind::Directory,
            libc::S_IFREG => ModeKind::Regular,
            libc::S_IFLNK => ModeKind::Symlink,
            _ => ModeKind::Other,
        }
    }

    fn entry_issue(error: PlatformError) -> PlatformEntryIssue {
        match error {
            PlatformError::Io { operation, source } => PlatformEntryIssue::Io { operation, source },
            PlatformError::Ambiguous { reason } => PlatformEntryIssue::Ambiguous { reason },
        }
    }

    fn stat_entry_optional(
        descriptor: i32,
        name: &CStr,
        flags: i32,
    ) -> Result<Option<libc::stat>, PlatformError> {
        let mut metadata = MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: all pointers are valid for the call; `name` is the current
        // NUL-terminated directory entry and `metadata` is writable.
        let result =
            unsafe { libc::fstatat(descriptor, name.as_ptr(), metadata.as_mut_ptr(), flags) };
        if result != 0 {
            let source = io::Error::last_os_error();
            if source.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(PlatformError::Io {
                operation: "inspect traversal directory entry",
                source,
            });
        }
        // SAFETY: successful `fstatat` initialized the complete structure.
        Ok(Some(unsafe { metadata.assume_init() }))
    }

    #[cfg(target_os = "linux")]
    fn clear_errno() {
        // SAFETY: the returned pointer addresses this thread's errno slot.
        unsafe { *libc::__errno_location() = 0 };
    }

    #[cfg(target_os = "macos")]
    fn clear_errno() {
        // SAFETY: the returned pointer addresses this thread's errno slot.
        unsafe { *libc::__error() = 0 };
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "Windows directory enumeration requires typed GetFileInformationByHandleEx buffers"
)]
mod platform {
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::mem::{align_of, offset_of, size_of};
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES,
        ERROR_NOT_SUPPORTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileBasicInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo,
        GetFileInformationByHandleEx,
    };

    use super::{
        DirectoryToken, NativeEntry, PlatformError, TraversalEntryKind, directory_change_digest,
        note_directory_handle_closed, note_directory_handle_opened,
    };
    use crate::receivers::filelog_receiver::checkpoint::Locator;

    const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;
    const DIRECTORY_BUFFER_WORDS: usize = DIRECTORY_BUFFER_BYTES / size_of::<u64>();

    pub(super) struct DirectoryHandle {
        file: File,
        volume_serial: u64,
        buffer: Box<[u64; DIRECTORY_BUFFER_WORDS]>,
        next_offset: Option<usize>,
        restart: bool,
    }

    impl DirectoryHandle {
        pub(super) fn open(path: &Path, follow_symlinks: bool) -> Result<Self, PlatformError> {
            if let Some(source) = super::take_directory_open_error() {
                return Err(PlatformError::Io {
                    operation: "open traversal directory",
                    source,
                });
            }
            let mut options = OpenOptions::new();
            options
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
            let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
            if !follow_symlinks {
                flags |= FILE_FLAG_OPEN_REPARSE_POINT;
            }
            options.custom_flags(flags);
            let file = options.open(path).map_err(|source| PlatformError::Io {
                operation: "open traversal directory",
                source,
            })?;
            let identity: FILE_ID_INFO =
                query(&file, FileIdInfo).map_err(|source| PlatformError::Io {
                    operation: "read traversal directory identity",
                    source,
                })?;
            let basic: FILE_BASIC_INFO =
                query(&file, FileBasicInfo).map_err(|source| PlatformError::Io {
                    operation: "read traversal directory metadata",
                    source,
                })?;
            if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(PlatformError::Ambiguous {
                    reason: "opened traversal handle is no longer a directory",
                });
            }
            if !follow_symlinks && basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(PlatformError::Ambiguous {
                    reason: "no-follow traversal opened a reparse point",
                });
            }
            note_directory_handle_opened();
            Ok(Self {
                file,
                volume_serial: identity.VolumeSerialNumber,
                buffer: Box::new([0; DIRECTORY_BUFFER_WORDS]),
                next_offset: None,
                restart: true,
            })
        }

        pub(super) fn token(&self, _path: &Path) -> Result<DirectoryToken, PlatformError> {
            let identity: FILE_ID_INFO =
                query(&self.file, FileIdInfo).map_err(|source| PlatformError::Io {
                    operation: "read traversal directory identity",
                    source,
                })?;
            let basic: FILE_BASIC_INFO =
                query(&self.file, FileBasicInfo).map_err(|source| PlatformError::Io {
                    operation: "read traversal directory metadata",
                    source,
                })?;
            if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(PlatformError::Ambiguous {
                    reason: "opened traversal handle is no longer a directory",
                });
            }
            let creation = basic.CreationTime.to_be_bytes();
            let modified = basic.LastWriteTime.to_be_bytes();
            let changed = basic.ChangeTime.to_be_bytes();
            let attributes = basic.FileAttributes.to_be_bytes();
            Ok(DirectoryToken {
                locator: Locator::WindowsVolumeFileId {
                    volume_serial: identity.VolumeSerialNumber,
                    file_id: identity.FileId.Identifier,
                },
                change_digest: directory_change_digest(&[
                    &creation,
                    &modified,
                    &changed,
                    &attributes,
                ]),
            })
        }

        pub(super) fn next(
            &mut self,
            follow_symlinks: bool,
        ) -> Result<Option<NativeEntry>, PlatformError> {
            loop {
                if self.next_offset.is_none() {
                    if !self.fill_buffer()? {
                        return Ok(None);
                    }
                }
                let offset = self.next_offset.take().ok_or(PlatformError::Ambiguous {
                    reason: "Windows traversal buffer lost its next entry offset",
                })?;
                let bytes = self.buffer.as_ref().as_ptr().cast::<u8>();
                let header_end = offset
                    .checked_add(size_of::<FILE_ID_EXTD_DIR_INFO>())
                    .ok_or(PlatformError::Ambiguous {
                        reason: "Windows traversal entry header overflowed",
                    })?;
                if header_end > DIRECTORY_BUFFER_BYTES {
                    return Err(PlatformError::Ambiguous {
                        reason: "Windows traversal entry header exceeds its fixed buffer",
                    });
                }
                // SAFETY: bounds above cover the fixed header; read_unaligned
                // avoids assuming the OS-provided offset has Rust alignment.
                let entry = unsafe {
                    std::ptr::read_unaligned(bytes.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>())
                };
                let name_bytes = usize::try_from(entry.FileNameLength).map_err(|_| {
                    PlatformError::Ambiguous {
                        reason: "Windows traversal filename length does not fit usize",
                    }
                })?;
                if !name_bytes.is_multiple_of(size_of::<u16>()) {
                    return Err(PlatformError::Ambiguous {
                        reason: "Windows traversal filename length is not UTF-16 aligned",
                    });
                }
                let name_offset = offset
                    .checked_add(offset_of!(FILE_ID_EXTD_DIR_INFO, FileName))
                    .ok_or(PlatformError::Ambiguous {
                        reason: "Windows traversal filename offset overflowed",
                    })?;
                let name_end =
                    name_offset
                        .checked_add(name_bytes)
                        .ok_or(PlatformError::Ambiguous {
                            reason: "Windows traversal filename extent overflowed",
                        })?;
                if name_end > DIRECTORY_BUFFER_BYTES {
                    return Err(PlatformError::Ambiguous {
                        reason: "Windows traversal filename exceeds its fixed buffer",
                    });
                }
                if entry.NextEntryOffset == 0 {
                    self.next_offset = None;
                } else {
                    let next = offset
                        .checked_add(usize::try_from(entry.NextEntryOffset).map_err(|_| {
                            PlatformError::Ambiguous {
                                reason: "Windows traversal next offset does not fit usize",
                            }
                        })?)
                        .ok_or(PlatformError::Ambiguous {
                            reason: "Windows traversal next offset overflowed",
                        })?;
                    if next <= offset
                        || next >= DIRECTORY_BUFFER_BYTES
                        || !next.is_multiple_of(align_of::<u64>())
                    {
                        return Err(PlatformError::Ambiguous {
                            reason: "Windows traversal next offset is malformed",
                        });
                    }
                    self.next_offset = Some(next);
                }
                // SAFETY: the validated byte range is UTF-16 aligned because
                // the base buffer and structure field are both aligned.
                let name = unsafe {
                    std::slice::from_raw_parts(
                        bytes.add(name_offset).cast::<u16>(),
                        name_bytes / size_of::<u16>(),
                    )
                };
                if name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16] {
                    continue;
                }
                let path_is_symlink = entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                let kind = if path_is_symlink && !follow_symlinks {
                    TraversalEntryKind::Other
                } else if entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    TraversalEntryKind::Directory
                } else if entry.FileAttributes & FILE_ATTRIBUTE_DEVICE == 0 {
                    TraversalEntryKind::RegularFile
                } else {
                    TraversalEntryKind::Other
                };
                return Ok(Some(NativeEntry::new(
                    OsString::from_wide(name),
                    Locator::WindowsVolumeFileId {
                        volume_serial: self.volume_serial,
                        file_id: entry.FileId.Identifier,
                    },
                    kind,
                    path_is_symlink,
                    entry.ReparsePointTag,
                )));
            }
        }

        fn fill_buffer(&mut self) -> Result<bool, PlatformError> {
            self.buffer.fill(0);
            let class = if self.restart {
                FileIdExtdDirectoryRestartInfo
            } else {
                FileIdExtdDirectoryInfo
            };
            // SAFETY: `self.file` is a live directory handle and `buffer` is
            // a writable fixed-size aligned region for the selected class.
            let succeeded = unsafe {
                GetFileInformationByHandleEx(
                    self.file.as_raw_handle(),
                    class,
                    self.buffer.as_mut_ptr().cast(),
                    DIRECTORY_BUFFER_BYTES as u32,
                )
            };
            self.restart = false;
            if succeeded == 0 {
                let source = io::Error::last_os_error();
                return match source.raw_os_error().map(|code| code as u32) {
                    Some(ERROR_NO_MORE_FILES) => Ok(false),
                    Some(
                        ERROR_INVALID_PARAMETER
                        | ERROR_NOT_SUPPORTED
                        | ERROR_MORE_DATA
                        | ERROR_INSUFFICIENT_BUFFER,
                    ) => Err(PlatformError::Ambiguous {
                        reason: "Windows extended directory identity enumeration is unavailable",
                    }),
                    _ => Err(PlatformError::Io {
                        operation: "enumerate traversal directory",
                        source,
                    }),
                };
            }
            self.next_offset = Some(0);
            Ok(true)
        }
    }

    impl Drop for DirectoryHandle {
        fn drop(&mut self) {
            note_directory_handle_closed();
        }
    }

    #[cfg(test)]
    mod tests {
        use std::ffi::{OsStr, OsString};
        use std::os::windows::ffi::OsStringExt;

        use sha2::{Digest, Sha256};

        use super::super::{
            TraversalEntryKind, directory_entry_digest, hash_native_name, native_name_cmp,
        };
        use crate::receivers::filelog_receiver::checkpoint::Locator;

        /// Scenario: two native Windows names include an unpaired UTF-16
        /// surrogate and differ only in the following code unit.
        /// Guarantees: traversal ordering and hashing consume complete UTF-16
        /// units without lossy Unicode conversion.
        #[test]
        fn native_name_order_preserves_unpaired_utf16() {
            let first = OsString::from_wide(&[b'a' as u16, 0xd800, b'a' as u16]);
            let second = OsString::from_wide(&[b'a' as u16, 0xd800, b'b' as u16]);

            assert!(native_name_cmp(&first, &second).is_lt());
            let mut first_hash = Sha256::new();
            hash_native_name(&mut first_hash, &first);
            let mut second_hash = Sha256::new();
            hash_native_name(&mut second_hash, &second);
            assert_ne!(first_hash.finalize(), second_hash.finalize());
        }

        /// Scenario: two Windows directory entries have equal low 64 file-ID
        /// bits and differ only in the upper half of `FILE_ID_128`.
        /// Guarantees: resume evidence binds the complete 128-bit file ID
        /// instead of truncating to the legacy directory-information value.
        #[test]
        fn entry_digest_uses_full_128_bit_file_id() {
            let mut first_id = [0u8; 16];
            first_id[..8].copy_from_slice(&7u64.to_be_bytes());
            let mut second_id = first_id;
            second_id[15] = 1;
            let first = directory_entry_digest(
                OsStr::new("same.log"),
                Locator::WindowsVolumeFileId {
                    volume_serial: 9,
                    file_id: first_id,
                },
                TraversalEntryKind::RegularFile,
                false,
                0,
            );
            let second = directory_entry_digest(
                OsStr::new("same.log"),
                Locator::WindowsVolumeFileId {
                    volume_serial: 9,
                    file_id: second_id,
                },
                TraversalEntryKind::RegularFile,
                false,
                0,
            );

            assert_ne!(first, second);
        }
    }

    fn query<T: Default>(file: &File, class: i32) -> io::Result<T> {
        let mut value = T::default();
        // SAFETY: `value` is writable for its exact type size, and the
        // information class selected by each caller matches `T`.
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                (&raw mut value).cast(),
                size_of::<T>() as u32,
            )
        };
        if succeeded == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use std::path::Path;

    use super::{DirectoryToken, NativeEntry, PlatformError};

    pub(super) struct DirectoryHandle;

    impl DirectoryHandle {
        pub(super) fn open(_path: &Path, _follow_symlinks: bool) -> Result<Self, PlatformError> {
            if let Some(source) = super::take_directory_open_error() {
                return Err(PlatformError::Io {
                    operation: "open traversal directory",
                    source,
                });
            }
            Err(PlatformError::Ambiguous {
                reason: "bounded directory traversal is unsupported on this platform",
            })
        }

        pub(super) fn token(&self, _path: &Path) -> Result<DirectoryToken, PlatformError> {
            unreachable!("unsupported traversal cannot create a directory handle")
        }

        pub(super) fn next(
            &mut self,
            _follow_symlinks: bool,
        ) -> Result<Option<NativeEntry>, PlatformError> {
            unreachable!("unsupported traversal cannot create a directory handle")
        }
    }
}
