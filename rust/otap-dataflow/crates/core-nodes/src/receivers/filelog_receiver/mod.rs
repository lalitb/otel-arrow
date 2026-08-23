// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Filelog receiver (Phase 1, under construction).
//!
//! This module currently ships two Phase 1 implementation-plan deliverables:
//!
//! - Stage 2: the exact version-1 durable checkpoint byte format and its
//!   codec, specified in
//!   [`docs/filelog-checkpoint-format.md`](../../../../../../docs/filelog-checkpoint-format.md)
//!   and referenced from
//!   [`docs/filelog-receiver.md`](../../../../../../docs/filelog-receiver.md)
//!   Appendix B.
//! - This stage: the user-facing configuration schema, its validation into
//!   a runtime-ready form, and the shared logical-record-size function,
//!   specified in `docs/filelog-receiver.md` Appendix C.
//!
//! Runtime receiver wiring -- discovery, framing, the durable checkpoint
//! store (locking, atomic compaction, fsync), identity/recovery matching,
//! and component factory registration -- is implemented in a later stage and
//! does not exist yet. [`FILELOG_RECEIVER_URN`] is exported so that stage can
//! register a `ReceiverFactory` without renaming anything here, but no
//! factory is registered yet: there is no `distributed_slice` entry and no
//! receiver implementation in this module.
#![allow(dead_code)] // Config validation and the checkpoint codec are wired up so far, but nothing constructs a receiver yet.

/// Durable checkpoint codec: the version-1 snapshot/WAL byte format.
///
/// This module only encodes, decodes, and replays checkpoint bytes in
/// memory. It performs no filesystem I/O, no OS locking, and no OS-specific
/// locator lookups; those belong to a later implementation stage.
pub mod checkpoint;

mod config;

pub use config::{
    BatchConfig, CheckpointConfig, Config, DiscoveryConfig, Encoding, FILELOG_RECEIVER_URN,
    FramingConfig, IdentityConfig, LimitsConfig, MaxLogSizeBehavior, MetadataConfig,
    MultilineConfig, OnDecodeError, OnNack, OnRecoveryMismatch, OnTruncate, RegexProfile,
    RetryConfig, RotationConfig, StartAt,
};
