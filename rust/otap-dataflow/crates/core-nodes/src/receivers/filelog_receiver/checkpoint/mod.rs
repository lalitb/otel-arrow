// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Version-1 filelog durable checkpoint codec.
//!
//! This module implements exactly the byte format specified in
//! [`docs/filelog-checkpoint-format.md`](../../../../../../../docs/filelog-checkpoint-format.md):
//! the `CURRENT` marker, the snapshot file, the append-only WAL, the eight
//! logical WAL operations (`register_file`, `update_progress`,
//! `reset_after_truncate`, `update_fingerprint`, `update_metadata`,
//! `quarantine_file`, `reset_quarantined_file`, `remove_file`), and the
//! framing-profile canonical serialization and digest.
//!
//! Scope note (Stage 2 of the Phase 1 filelog receiver implementation plan):
//! this module only encodes, decodes, and replays checkpoint bytes against
//! an in-memory table. It performs no filesystem I/O, no namespace locking,
//! no atomic file replacement, and no OS-specific locator lookups; the
//! durable checkpoint store (Stage 4) builds on top of this codec in a later
//! implementation stage.
//!
//! Locators are represented purely as normalized data ([`primitives::Locator`])
//! with no OS FFI, so this module and its tests compile and run identically
//! on Unix and non-Unix targets.

pub mod apply;
pub mod current_marker;
pub mod error;
pub mod framing_profile;
pub mod primitives;
pub mod snapshot;
pub mod wal;

#[cfg(test)]
mod test_vectors;
#[cfg(test)]
mod tests;

pub use apply::{CheckpointTable, TableRecord};
pub use error::{ApplyError, DecodeError, EncodeError};
pub use primitives::{FileId, FramingResume, LifecycleState, Locator};
pub use snapshot::{QuarantineEvidence, SnapshotContents, SnapshotRecord};
pub use wal::WalContents;
