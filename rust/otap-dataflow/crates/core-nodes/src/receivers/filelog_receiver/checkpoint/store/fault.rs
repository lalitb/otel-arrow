// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic fault injection at every persistence boundary of
//! namespace-parent creation, generation publication, compaction, live WAL
//! appends, torn-tail repair, and retired-generation cleanup.
//!
//! A [`FaultPlan`] belongs to exactly one [`super::CheckpointStore`]
//! instance; it is not a global, a thread-local, or an environment switch,
//! so two stores (or two tests) never interfere with each other and a
//! production build cannot be induced to fail from outside. The only
//! constructor that arms a point, [`FaultPlan::armed`], is compiled under
//! `cfg(test)`; a production build can only ever construct
//! [`FaultPlan::disabled`], for which [`FaultPlan::check`] is a single
//! `Option` comparison that always succeeds.
//!
//! An armed point fires exactly once, at the selected occurrence of exactly
//! the boundary it names, so a test can assert the recovery outcome for each
//! individual step of the "write, sync, publish, sync directory" sequence,
//! the resumability of a cleanup that fails part way through, and a failure
//! between logical-batch WAL chunks.

use std::fmt;

use super::error::StoreError;

/// One persistence boundary of the durable sequences the store performs:
/// syncing namespace parents ([`FaultPoint::NAMESPACE_CREATION`]), publishing
/// a generation ([`FaultPoint::PUBLICATION`]), appending and syncing its live
/// WAL ([`FaultPoint::WAL_DURABILITY`]), repairing a torn tail
/// ([`FaultPoint::TORN_TAIL_REPAIR`]), and removing a retired one
/// ([`FaultPoint::CLEANUP`]).
///
/// The publication boundaries are ordered exactly as the durable sequence
/// executes them, and every artifact is written to a same-directory
/// role-specific temporary file, synced, and then atomically installed, so a
/// fault at any single boundary leaves either the complete old generation or
/// the complete new generation reachable, never a mixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    /// Before syncing `engine.state_dir` after opening or creating `filelog`.
    BeforeFilelogParentSync,
    /// After syncing `engine.state_dir` for the `filelog` entry.
    AfterFilelogParentSync,
    /// Before syncing `filelog` after opening or creating `@v1`.
    BeforeVersionParentSync,
    /// After syncing `filelog` for the `@v1` entry.
    AfterVersionParentSync,
    /// Before syncing `@v1` after opening or creating the namespace.
    BeforeNamespaceParentSync,
    /// After syncing `@v1` for the namespace entry.
    AfterNamespaceParentSync,
    /// Before the snapshot temporary file is created.
    BeforeSnapshotWrite,
    /// After the snapshot bytes are written, before they are synced.
    AfterSnapshotWrite,
    /// After the snapshot temporary file is synced, before it is renamed.
    AfterSnapshotSync,
    /// After the snapshot temporary file is renamed into place.
    AfterSnapshotPublish,
    /// Before the WAL temporary file is created.
    BeforeWalWrite,
    /// After the WAL header bytes are written, before they are synced.
    AfterWalWrite,
    /// After the WAL temporary file is synced, before it is renamed.
    AfterGenerationWalSync,
    /// After the WAL temporary file is renamed into place.
    AfterWalPublish,
    /// Before syncing the namespace directory that contains the staged
    /// snapshot/WAL pair.
    BeforeGenerationDirSync,
    /// After the namespace directory is synced, so both new generation
    /// files are durable, but before the marker is touched.
    AfterGenerationDirSync,
    /// Before the `CURRENT` temporary marker is created.
    BeforeMarkerWrite,
    /// After the marker bytes are written, before they are synced.
    AfterMarkerWrite,
    /// After the temporary marker is synced, before it replaces `CURRENT`.
    AfterMarkerSync,
    /// After the temporary marker atomically replaces `CURRENT`; the new
    /// generation is now authoritative.
    AfterMarkerPublish,
    /// Before syncing the directory entry for the newly published marker.
    BeforeMarkerDirSync,
    /// After the final namespace directory sync that makes the marker
    /// replacement itself durable.
    AfterMarkerDirSync,
    /// Before either file of a retired generation is unlinked.
    BeforeRetiredGenerationRemoval,
    /// After a retired generation's WAL is unlinked, before its snapshot is,
    /// which is the required partial-removal state cleanup must resume from.
    AfterRetiredWalRemoval,
    /// Before syncing the directory after retired files were removed.
    BeforeRetiredDirectorySync,
    /// Before writing a normal WAL transaction.
    BeforeWalTransactionWrite,
    /// After writing a strict prefix of a WAL transaction.
    DuringWalTransactionWrite,
    /// After writing a complete WAL transaction, before any required sync.
    AfterWalTransactionWrite,
    /// Before syncing appended WAL transactions.
    BeforeWalSync,
    /// After syncing appended WAL transactions.
    AfterWalSync,
    /// Before truncating a structurally incomplete final WAL transaction.
    BeforeTornTailTruncate,
    /// After truncating and syncing a structurally incomplete WAL tail.
    AfterTornTailTruncate,
}

impl FaultPoint {
    /// Every required parent-directory durability boundary.
    pub const NAMESPACE_CREATION: [FaultPoint; 6] = [
        FaultPoint::BeforeFilelogParentSync,
        FaultPoint::AfterFilelogParentSync,
        FaultPoint::BeforeVersionParentSync,
        FaultPoint::AfterVersionParentSync,
        FaultPoint::BeforeNamespaceParentSync,
        FaultPoint::AfterNamespaceParentSync,
    ];

    /// Every boundary of the generation-publication sequence, in the order
    /// it executes them.
    pub const PUBLICATION: [FaultPoint; 16] = [
        FaultPoint::BeforeSnapshotWrite,
        FaultPoint::AfterSnapshotWrite,
        FaultPoint::AfterSnapshotSync,
        FaultPoint::AfterSnapshotPublish,
        FaultPoint::BeforeWalWrite,
        FaultPoint::AfterWalWrite,
        FaultPoint::AfterGenerationWalSync,
        FaultPoint::AfterWalPublish,
        FaultPoint::BeforeGenerationDirSync,
        FaultPoint::AfterGenerationDirSync,
        FaultPoint::BeforeMarkerWrite,
        FaultPoint::AfterMarkerWrite,
        FaultPoint::AfterMarkerSync,
        FaultPoint::AfterMarkerPublish,
        FaultPoint::BeforeMarkerDirSync,
        FaultPoint::AfterMarkerDirSync,
    ];

    /// Every boundary of retired-generation cleanup, in the order it
    /// executes them.
    pub const CLEANUP: [FaultPoint; 3] = [
        FaultPoint::BeforeRetiredGenerationRemoval,
        FaultPoint::AfterRetiredWalRemoval,
        FaultPoint::BeforeRetiredDirectorySync,
    ];

    /// Every boundary of ordinary WAL append and sync.
    pub const WAL_DURABILITY: [FaultPoint; 5] = [
        FaultPoint::BeforeWalTransactionWrite,
        FaultPoint::DuringWalTransactionWrite,
        FaultPoint::AfterWalTransactionWrite,
        FaultPoint::BeforeWalSync,
        FaultPoint::AfterWalSync,
    ];

    /// Both boundaries of torn-tail repair during recovery.
    pub const TORN_TAIL_REPAIR: [FaultPoint; 2] = [
        FaultPoint::BeforeTornTailTruncate,
        FaultPoint::AfterTornTailTruncate,
    ];

    /// A stable, human-readable name for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FaultPoint::BeforeFilelogParentSync => "before_filelog_parent_sync",
            FaultPoint::AfterFilelogParentSync => "after_filelog_parent_sync",
            FaultPoint::BeforeVersionParentSync => "before_version_parent_sync",
            FaultPoint::AfterVersionParentSync => "after_version_parent_sync",
            FaultPoint::BeforeNamespaceParentSync => "before_namespace_parent_sync",
            FaultPoint::AfterNamespaceParentSync => "after_namespace_parent_sync",
            FaultPoint::BeforeSnapshotWrite => "before_snapshot_write",
            FaultPoint::AfterSnapshotWrite => "after_snapshot_write",
            FaultPoint::AfterSnapshotSync => "after_snapshot_sync",
            FaultPoint::AfterSnapshotPublish => "after_snapshot_publish",
            FaultPoint::BeforeWalWrite => "before_wal_write",
            FaultPoint::AfterWalWrite => "after_wal_write",
            FaultPoint::AfterGenerationWalSync => "after_generation_wal_sync",
            FaultPoint::AfterWalPublish => "after_wal_publish",
            FaultPoint::BeforeGenerationDirSync => "before_generation_dir_sync",
            FaultPoint::AfterGenerationDirSync => "after_generation_dir_sync",
            FaultPoint::BeforeMarkerWrite => "before_marker_write",
            FaultPoint::AfterMarkerWrite => "after_marker_write",
            FaultPoint::AfterMarkerSync => "after_marker_sync",
            FaultPoint::AfterMarkerPublish => "after_marker_publish",
            FaultPoint::BeforeMarkerDirSync => "before_marker_dir_sync",
            FaultPoint::AfterMarkerDirSync => "after_marker_dir_sync",
            FaultPoint::BeforeRetiredGenerationRemoval => "before_retired_generation_removal",
            FaultPoint::AfterRetiredWalRemoval => "after_retired_wal_removal",
            FaultPoint::BeforeRetiredDirectorySync => "before_retired_directory_sync",
            FaultPoint::BeforeWalTransactionWrite => "before_wal_transaction_write",
            FaultPoint::DuringWalTransactionWrite => "during_wal_transaction_write",
            FaultPoint::AfterWalTransactionWrite => "after_wal_transaction_write",
            FaultPoint::BeforeWalSync => "before_wal_sync",
            FaultPoint::AfterWalSync => "after_wal_sync",
            FaultPoint::BeforeTornTailTruncate => "before_torn_tail_truncate",
            FaultPoint::AfterTornTailTruncate => "after_torn_tail_truncate",
        }
    }
}

impl fmt::Display for FaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The at-most-one armed fault point of a single store instance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FaultPlan {
    armed: Option<FaultPoint>,
    #[cfg(test)]
    matching_occurrences_to_skip: usize,
}

impl FaultPlan {
    /// A plan that never fires. This is the only plan a production build can
    /// construct.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            armed: None,
            #[cfg(test)]
            matching_occurrences_to_skip: 0,
        }
    }

    /// A plan that fires once, when `point` is reached.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn armed(point: FaultPoint) -> Self {
        Self {
            armed: Some(point),
            matching_occurrences_to_skip: 0,
        }
    }

    /// A plan that skips `matching_occurrences_to_skip` matching boundaries
    /// and then fires once. This permits deterministic failures between
    /// chunks of one logical batch.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn armed_after(
        point: FaultPoint,
        matching_occurrences_to_skip: usize,
    ) -> Self {
        Self {
            armed: Some(point),
            matching_occurrences_to_skip,
        }
    }

    /// Fails with [`StoreError::InjectedFault`] exactly once if `point` is
    /// the armed boundary, and disarms itself so a retry of the same durable
    /// sequence can make progress.
    pub(crate) fn check(&mut self, point: FaultPoint) -> Result<(), StoreError> {
        if self.armed == Some(point) {
            #[cfg(test)]
            if self.matching_occurrences_to_skip > 0 {
                self.matching_occurrences_to_skip -= 1;
                return Ok(());
            }
            self.armed = None;
            return Err(StoreError::InjectedFault { point });
        }
        Ok(())
    }
}
