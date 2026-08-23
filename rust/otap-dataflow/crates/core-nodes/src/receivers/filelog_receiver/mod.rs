// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Filelog receiver (Phase 1, under construction).
//!
//! This module currently ships three Phase 1 implementation-plan
//! deliverables:
//!
//! - Stage 2: the exact version-1 durable checkpoint byte format and its
//!   codec, specified in
//!   [`docs/filelog-checkpoint-format.md`](../../../../../../docs/filelog-checkpoint-format.md)
//!   and referenced from
//!   [`docs/filelog-receiver.md`](../../../../../../docs/filelog-receiver.md)
//!   Appendix B.
//! - The user-facing configuration schema, its validation into a
//!   runtime-ready form, and the shared logical-record-size function,
//!   specified in `docs/filelog-receiver.md` Appendix C.
//! - This stage: the durable, file-backed checkpoint store built on that
//!   codec ([`checkpoint::store`]) -- namespace ownership locking,
//!   generation selection and recovery, WAL appends and sync policy,
//!   atomic compaction, and retention -- specified in
//!   `docs/filelog-receiver.md` Appendix B.
//! - The process-local runtime-locator lease registry ([`lease`]), which
//!   gives each receiver a preallocated, bounded ownership scope and prevents
//!   two filelog readers in one engine process from controlling the same live
//!   file.
//!
//! Runtime receiver wiring -- discovery, framing, identity/recovery
//! matching, the read/checkpoint thread that drives the store, and component
//! factory registration -- is implemented in a later stage and does not
//! exist yet. [`FILELOG_RECEIVER_URN`] is exported so that stage can register
//! a `ReceiverFactory` without renaming anything here, but no factory is
//! registered yet: there is no `distributed_slice` entry and no receiver
//! implementation in this module.
#![allow(dead_code)] // Config validation and the checkpoint codec are wired up so far, but nothing constructs a receiver yet.

/// Durable checkpoint state: the version-1 snapshot/WAL byte format and the
/// file-backed store that persists it.
///
/// The codec modules encode, decode, and replay checkpoint bytes in memory
/// and perform no I/O. [`checkpoint::store`] owns the namespace on disk; it
/// blocks, so it belongs on the receiver's dedicated read/checkpoint OS
/// thread. Neither performs OS-specific locator lookups; those belong to a
/// later implementation stage.
pub mod checkpoint;

mod config;
mod lease;

pub use config::{
    BatchConfig, CheckpointConfig, Config, DiscoveryConfig, Encoding, FILELOG_RECEIVER_URN,
    FramingConfig, IdentityConfig, LimitsConfig, MaxLogSizeBehavior, MetadataConfig,
    MultilineConfig, OnDecodeError, OnNack, OnRecoveryMismatch, OnTruncate, RegexProfile,
    RetryConfig, RotationConfig, StartAt,
};
