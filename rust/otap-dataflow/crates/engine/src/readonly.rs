// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Read-only marking and copy-on-write helpers for pipeline data (PData).
//!
//! This abstraction enables fan-out processors to provide:
//! - Isolation for mutating downstream consumers (they receive owned copies).
//! - Shared, read-only views for consumers that promise not to mutate.
//!
//! ## Implementation Phases
//!
//! **Phase 1 (Current)**: Mutation isolation via cloning
//! - `clone_read_only()` performs full clone (safe, simple, correct)
//! - `mark_read_only()` is no-op (no shared state to protect yet)
//! - Guarantees isolation at cost of extra clones
//! - No `is_read_only()` method needed (no upstream pre-marking exists)
//!
//! **Phase 2 (Future)**: Copy-on-write optimization
//! - Add `is_read_only()` query method to trait
//! - Implement `mark_read_only()` with Arc<RwLock<>> or similar
//! - Check `is_read_only()` before giving original to last mutator
//! - Share readonly clones instead of creating N independent copies
//! - Integrate with interior mutability for true COW semantics
//!
//! ## Rationale for Phased Approach
//!
//! Phase 1 establishes **correct semantics** (mutation isolation) with simple
//! implementation (full clones). This allows:
//! - Proving correctness before adding optimization complexity
//! - Testing mutation isolation guarantees independently
//! - Avoiding premature optimization of unstable pdata types
//! - Clear extension points when COW layer is justified by profiling
//!
//! Safety / Guarantees:
//! - Downstream code SHOULD treat data obtained via `clone_read_only()` as immutable.
//! - The engine does not yet enforce immutability; this is cooperative until
//!   mutation-aware interests and runtime checks are added.

/// Marker + helper trait providing read-only cloning semantics for pipeline data.
///
/// Types implementing `Clone` automatically implement this via the blanket impl below.
/// The trait supplies:
/// - `clone_read_only()`: intended for sharing among non-mutating consumers.
/// - `mark_read_only()`: future hook for enforcing immutability.
///
/// See module-level documentation for design notes and TODO items.
pub trait ReadonlyMarkable: Clone {
    /// Returns a read-only clone suitable for sharing among non-mutating consumers.
    #[inline]
    fn clone_read_only(&self) -> Self {
        self.clone()
    }

    /// Marks this instance as read-only (placeholder).
    #[inline]
    fn mark_read_only(&mut self) {}
}

/// Blanket implementation: all `Clone` types gain `ReadonlyMarkable`.
impl<T: Clone> ReadonlyMarkable for T {}
