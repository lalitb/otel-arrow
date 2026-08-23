// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Filelog receiver (Phase 1, under construction).
//!
//! This module currently ships only the Stage 2 deliverable of the Phase 1
//! filelog receiver implementation plan: the exact version-1 durable
//! checkpoint byte format and its codec, specified in
//! [`docs/filelog-checkpoint-format.md`](../../../../../../docs/filelog-checkpoint-format.md)
//! and referenced from
//! [`docs/filelog-receiver.md`](../../../../../../docs/filelog-receiver.md)
//! Appendix B.
//!
//! Runtime receiver wiring -- configuration, discovery, the durable
//! checkpoint store (locking, atomic compaction, fsync), identity/recovery
//! matching, and component factory registration -- is implemented in a later
//! stage and does not exist yet. There is intentionally no receiver
//! implementation, config struct, or URN registration in this module yet.
#![allow(dead_code)] // Only the checkpoint codec is wired up so far.

/// Durable checkpoint codec: the version-1 snapshot/WAL byte format.
///
/// This module only encodes, decodes, and replays checkpoint bytes in
/// memory. It performs no filesystem I/O, no OS locking, and no OS-specific
/// locator lookups; those belong to a later implementation stage.
pub mod checkpoint;
