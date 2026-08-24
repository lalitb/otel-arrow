// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Process-local single-reader leases for live file identities.
//!
//! The durable checkpoint namespace lock serializes one logical receiver
//! across deployment generations. This narrower registry prevents two
//! independently configured filelog receivers in the same process from
//! controlling the same live runtime locator. It stores no progress or
//! telemetry data and has no internal wait queue.

use std::collections::{TryReserveError, hash_map::RandomState};
use std::hash::BuildHasher;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use super::checkpoint::Locator;

/// Maximum number of simultaneously registered filelog receiver scopes.
///
/// This fixed directory keeps scope lookup allocation-free and O(1) while
/// holding the process-wide mutex. It is intentionally far above any
/// practical engine node population.
const PROCESS_SCOPE_SLOTS: usize = 65_536;
/// Number of buckets in the process-wide locator index.
const PROCESS_INDEX_BUCKETS: usize = 65_536;
/// Maximum entries examined in one locator bucket while holding the mutex.
const MAX_BUCKET_DEPTH: usize = 64;

static PROCESS_WIDE_REGISTRY: LazyLock<RuntimeLeaseRegistry> =
    LazyLock::new(|| RuntimeLeaseRegistry::new(PROCESS_SCOPE_SLOTS, PROCESS_INDEX_BUCKETS));

/// Registers one receiver with the only production runtime-locator registry.
pub(crate) fn register_receiver_scope(
    max_tracked_files: u32,
) -> Result<ReceiverLeaseScope, LeaseError> {
    PROCESS_WIDE_REGISTRY.register_receiver(max_tracked_files)
}

/// Fail-closed errors from the process-local runtime-locator registry.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LeaseError {
    /// A locator is unavailable and cannot safely distinguish live files.
    #[error("a runtime file lease requires a platform file locator")]
    UnspecifiedLocator,
    /// Another receiver already controls this live locator.
    #[error("runtime file locator {locator:?} is already leased")]
    Contended {
        /// The contended runtime locator.
        locator: Locator,
    },
    /// One receiver reached its configured tracked-file population.
    #[error(
        "runtime file lease capacity is exhausted for receiver scope {scope_slot}: \
         held {held}, maximum {max}"
    )]
    ScopeCapacityExhausted {
        /// Internal receiver-scope slot.
        scope_slot: usize,
        /// Number of leases currently held by the scope.
        held: u32,
        /// Configured maximum for this scope.
        max: u32,
    },
    /// The configured population cannot be represented on this target.
    #[error("runtime file lease capacity {max} is too large for this target")]
    ScopeCapacityTooLarge {
        /// Configured tracked-file maximum.
        max: u32,
    },
    /// All fixed process-local receiver slots are already occupied.
    #[error("runtime file lease registry supports at most {max_scopes} active receiver scopes")]
    ReceiverScopeCapacityExhausted {
        /// Fixed process-local receiver-scope bound.
        max_scopes: usize,
    },
    /// One keyed locator bucket reached its bounded collision depth.
    #[error("runtime file lease locator bucket {bucket} reached its maximum depth {max_depth}")]
    LocatorBucketCapacityExhausted {
        /// Bucket that reached its collision bound.
        bucket: usize,
        /// Maximum permitted entries in one bucket.
        max_depth: usize,
    },
    /// Registering another receiver would overflow aggregate accounting.
    #[error("runtime file lease aggregate capacity overflowed")]
    AggregateCapacityOverflow,
    /// Preallocating one receiver's bounded entry table failed.
    #[error("runtime file lease registry allocation failed")]
    AllocationFailed {
        /// Allocation failure reported by the collection.
        #[source]
        source: TryReserveError,
    },
    /// The process-local mutex was poisoned.
    #[error("runtime file lease registry is poisoned")]
    RegistryPoisoned,
    /// Internal ownership accounting no longer agrees.
    #[error("runtime file lease registry integrity check failed")]
    RegistryInconsistent,
    /// Explicit scope closure was attempted while lease guards still exist.
    #[error("runtime file lease scope still has {leases} outstanding lease guards")]
    OutstandingLeases {
        /// Number of guards that must be released before closure.
        leases: usize,
    },
    /// The receiver scope has already closed.
    #[error("runtime file lease scope is already closed")]
    ScopeClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryId(NonZeroU64);

impl EntryId {
    fn new(scope_slot: usize, entry_slot: u32) -> Option<Self> {
        let encoded_scope = u32::try_from(scope_slot).ok()?.checked_add(1)?;
        let raw = (u64::from(encoded_scope) << 32) | u64::from(entry_slot);
        NonZeroU64::new(raw).map(Self)
    }

    fn scope_slot(self) -> Option<usize> {
        let encoded = u32::try_from(self.0.get() >> 32).ok()?;
        usize::try_from(encoded.checked_sub(1)?).ok()
    }

    fn entry_slot(self) -> u32 {
        self.0.get() as u32
    }
}

#[derive(Clone, Copy, Debug)]
enum EntrySlot {
    Free {
        next: Option<u32>,
    },
    Active {
        hash: u64,
        locator: Locator,
        next: Option<EntryId>,
    },
}

#[derive(Clone, Copy, Debug)]
struct ActiveEntry {
    hash: u64,
    locator: Locator,
    next: Option<EntryId>,
}

#[derive(Debug)]
struct ScopeEntries {
    max_leases: u32,
    held_leases: u32,
    free_head: Option<u32>,
    entries: Vec<EntrySlot>,
}

impl ScopeEntries {
    fn allocate(max_leases: u32) -> Result<Box<Self>, LeaseError> {
        let max = usize::try_from(max_leases)
            .map_err(|_| LeaseError::ScopeCapacityTooLarge { max: max_leases })?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(max)
            .map_err(|source| LeaseError::AllocationFailed { source })?;
        for index in 0..max_leases {
            entries.push(EntrySlot::Free {
                next: index.checked_add(1).filter(|next| *next < max_leases),
            });
        }
        Ok(Box::new(Self {
            max_leases,
            held_leases: 0,
            free_head: (max_leases > 0).then_some(0),
            entries,
        }))
    }

    fn active(&self, entry_slot: u32) -> Option<ActiveEntry> {
        let index = usize::try_from(entry_slot).ok()?;
        match *self.entries.get(index)? {
            EntrySlot::Active {
                hash,
                locator,
                next,
            } => Some(ActiveEntry {
                hash,
                locator,
                next,
            }),
            EntrySlot::Free { .. } => None,
        }
    }

    fn active_mut(&mut self, entry_slot: u32) -> Option<&mut EntrySlot> {
        let index = usize::try_from(entry_slot).ok()?;
        let entry = self.entries.get_mut(index)?;
        matches!(entry, EntrySlot::Active { .. }).then_some(entry)
    }

    fn insert(
        &mut self,
        scope_slot: usize,
        hash: u64,
        locator: Locator,
        next: Option<EntryId>,
    ) -> Result<EntryId, LeaseError> {
        if self.held_leases >= self.max_leases {
            return Err(LeaseError::ScopeCapacityExhausted {
                scope_slot,
                held: self.held_leases,
                max: self.max_leases,
            });
        }
        let entry_slot = self.free_head.ok_or(LeaseError::RegistryInconsistent)?;
        let index = usize::try_from(entry_slot).map_err(|_| LeaseError::RegistryInconsistent)?;
        let next_free = match self.entries.get(index) {
            Some(EntrySlot::Free { next }) => *next,
            Some(EntrySlot::Active { .. }) | None => {
                return Err(LeaseError::RegistryInconsistent);
            }
        };
        let entry_id =
            EntryId::new(scope_slot, entry_slot).ok_or(LeaseError::RegistryInconsistent)?;
        self.entries[index] = EntrySlot::Active {
            hash,
            locator,
            next,
        };
        self.free_head = next_free;
        self.held_leases = self
            .held_leases
            .checked_add(1)
            .ok_or(LeaseError::RegistryInconsistent)?;
        Ok(entry_id)
    }

    fn remove(&mut self, entry_slot: u32) -> Result<(), LeaseError> {
        let index = usize::try_from(entry_slot).map_err(|_| LeaseError::RegistryInconsistent)?;
        if !matches!(self.entries.get(index), Some(EntrySlot::Active { .. }))
            || self.held_leases == 0
        {
            return Err(LeaseError::RegistryInconsistent);
        }
        self.entries[index] = EntrySlot::Free {
            next: self.free_head,
        };
        self.free_head = Some(entry_slot);
        self.held_leases -= 1;
        Ok(())
    }
}

#[derive(Debug)]
struct RegistryState {
    scope_count: u64,
    lease_count: u64,
    aggregate_capacity: u64,
    scopes: Box<[Option<Box<ScopeEntries>>]>,
    free_scope_slots: Vec<u32>,
    buckets: Box<[Option<EntryId>]>,
}

impl RegistryState {
    fn new(scope_slots: usize, buckets: usize) -> Self {
        assert!(
            scope_slots <= u32::MAX as usize,
            "scope directory must fit EntryId"
        );
        assert!(
            buckets.is_power_of_two(),
            "locator bucket count must be a power of two"
        );
        let scopes: Vec<Option<Box<ScopeEntries>>> =
            std::iter::repeat_with(|| None).take(scope_slots).collect();
        let mut free_scope_slots = Vec::with_capacity(scope_slots);
        for index in (0..scope_slots).rev() {
            free_scope_slots.push(index as u32);
        }
        Self {
            scope_count: 0,
            lease_count: 0,
            aggregate_capacity: 0,
            scopes: scopes.into_boxed_slice(),
            free_scope_slots,
            buckets: vec![None; buckets].into_boxed_slice(),
        }
    }

    fn scope(&self, scope_slot: usize) -> Option<&ScopeEntries> {
        self.scopes.get(scope_slot)?.as_deref()
    }

    fn scope_mut(&mut self, scope_slot: usize) -> Option<&mut ScopeEntries> {
        self.scopes.get_mut(scope_slot)?.as_deref_mut()
    }

    fn active_entry(&self, entry_id: EntryId) -> Result<ActiveEntry, LeaseError> {
        let scope_slot = entry_id
            .scope_slot()
            .ok_or(LeaseError::RegistryInconsistent)?;
        self.scope(scope_slot)
            .and_then(|scope| scope.active(entry_id.entry_slot()))
            .ok_or(LeaseError::RegistryInconsistent)
    }

    fn active_entry_mut(&mut self, entry_id: EntryId) -> Result<&mut EntrySlot, LeaseError> {
        let scope_slot = entry_id
            .scope_slot()
            .ok_or(LeaseError::RegistryInconsistent)?;
        self.scope_mut(scope_slot)
            .and_then(|scope| scope.active_mut(entry_id.entry_slot()))
            .ok_or(LeaseError::RegistryInconsistent)
    }

    fn bucket_index(&self, hash: u64) -> usize {
        hash as usize & (self.buckets.len() - 1)
    }

    fn find_locator(
        &self,
        bucket: usize,
        hash: u64,
        locator: Locator,
    ) -> Result<(Option<EntryId>, usize), LeaseError> {
        let mut current = self.buckets[bucket];
        let mut depth = 0;
        while let Some(entry_id) = current {
            if depth >= MAX_BUCKET_DEPTH {
                return Err(LeaseError::RegistryInconsistent);
            }
            let entry = self.active_entry(entry_id)?;
            if entry.hash == hash && entry.locator == locator {
                return Ok((Some(entry_id), depth + 1));
            }
            current = entry.next;
            depth += 1;
        }
        Ok((None, depth))
    }

    fn find_in_bucket(
        &self,
        bucket: usize,
        target: EntryId,
    ) -> Result<(Option<EntryId>, ActiveEntry), LeaseError> {
        let mut previous = None;
        let mut current = self.buckets[bucket];
        let mut depth = 0;
        while let Some(entry_id) = current {
            if depth >= MAX_BUCKET_DEPTH {
                return Err(LeaseError::RegistryInconsistent);
            }
            let entry = self.active_entry(entry_id)?;
            if entry_id == target {
                return Ok((previous, entry));
            }
            previous = Some(entry_id);
            current = entry.next;
            depth += 1;
        }
        Err(LeaseError::RegistryInconsistent)
    }
}

#[derive(Debug)]
struct RegistryInner {
    state: Mutex<RegistryState>,
    inconsistent: AtomicBool,
    hash_builder: RandomState,
}

/// Private handle to the process-local runtime-locator registry.
///
/// Keeping this type private prevents production code from accidentally
/// constructing an isolated registry and bypassing process-wide exclusion.
#[derive(Clone, Debug)]
struct RuntimeLeaseRegistry {
    inner: Arc<RegistryInner>,
}

impl RuntimeLeaseRegistry {
    fn new(scope_slots: usize, buckets: usize) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                state: Mutex::new(RegistryState::new(scope_slots, buckets)),
                inconsistent: AtomicBool::new(false),
                hash_builder: RandomState::new(),
            }),
        }
    }

    fn register_receiver(&self, max_tracked_files: u32) -> Result<ReceiverLeaseScope, LeaseError> {
        // Allocate every per-file entry before taking the process-wide
        // mutex. Acquire and release never allocate or grow storage.
        let scope = ScopeEntries::allocate(max_tracked_files)?;
        let mut state = self.lock_state()?;
        let scope_slot = state.free_scope_slots.last().copied().ok_or(
            LeaseError::ReceiverScopeCapacityExhausted {
                max_scopes: state.scopes.len(),
            },
        )?;
        let scope_slot = usize::try_from(scope_slot).map_err(|_| self.mark_inconsistent())?;
        let scope_count = state
            .scope_count
            .checked_add(1)
            .ok_or_else(|| self.mark_inconsistent())?;
        let aggregate_capacity = state
            .aggregate_capacity
            .checked_add(u64::from(max_tracked_files))
            .ok_or(LeaseError::AggregateCapacityOverflow)?;
        if state.scopes.get(scope_slot).is_none_or(Option::is_some) {
            return Err(self.mark_inconsistent());
        }

        let _removed_slot = state.free_scope_slots.pop();
        state.scopes[scope_slot] = Some(scope);
        state.scope_count = scope_count;
        state.aggregate_capacity = aggregate_capacity;
        drop(state);

        Ok(ReceiverLeaseScope {
            token: Some(Arc::new(ScopeToken {
                registry: self.clone(),
                scope_slot,
                registered: AtomicBool::new(true),
            })),
        })
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, RegistryState>, LeaseError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| LeaseError::RegistryPoisoned)?;
        // Check after taking the mutex so no operation that observed an old
        // flag value can proceed after another holder detects corruption.
        if self.inner.inconsistent.load(Ordering::Acquire) {
            drop(state);
            return Err(LeaseError::RegistryInconsistent);
        }
        Ok(state)
    }

    fn ensure_healthy(&self) -> Result<(), LeaseError> {
        drop(self.lock_state()?);
        Ok(())
    }

    fn ensure_healthy_fast(&self) -> Result<(), LeaseError> {
        if self.inner.state.is_poisoned() {
            return Err(LeaseError::RegistryPoisoned);
        }
        if self.inner.inconsistent.load(Ordering::Acquire) {
            return Err(LeaseError::RegistryInconsistent);
        }
        Ok(())
    }

    fn locator_hash(&self, locator: Locator) -> u64 {
        self.inner.hash_builder.hash_one(locator)
    }

    fn acquire(
        &self,
        token: &Arc<ScopeToken>,
        locator: Locator,
    ) -> Result<RuntimeFileLease, LeaseError> {
        if locator == Locator::Unspecified {
            return Err(LeaseError::UnspecifiedLocator);
        }
        // Keyed locator hashing is deliberately outside the registry mutex.
        let hash = self.locator_hash(locator);
        let mut state = self.lock_state()?;
        let bucket = state.bucket_index(hash);
        let (existing, depth) = state
            .find_locator(bucket, hash, locator)
            .map_err(|_| self.mark_inconsistent())?;
        if existing.is_some() {
            return Err(LeaseError::Contended { locator });
        }
        if depth >= MAX_BUCKET_DEPTH {
            return Err(LeaseError::LocatorBucketCapacityExhausted {
                bucket,
                max_depth: MAX_BUCKET_DEPTH,
            });
        }
        let lease_count = state
            .lease_count
            .checked_add(1)
            .ok_or_else(|| self.mark_inconsistent())?;
        let head = state.buckets[bucket];
        let entry_id = state
            .scope_mut(token.scope_slot)
            .ok_or_else(|| self.mark_inconsistent())?
            .insert(token.scope_slot, hash, locator, head)?;
        state.buckets[bucket] = Some(entry_id);
        state.lease_count = lease_count;
        drop(state);

        Ok(RuntimeFileLease {
            token: Arc::clone(token),
            locator,
            hash,
            entry_id,
            released: false,
        })
    }

    fn release(
        &self,
        scope_slot: usize,
        locator: Locator,
        hash: u64,
        entry_id: EntryId,
    ) -> Result<(), LeaseError> {
        let mut state = self.lock_state()?;
        if entry_id.scope_slot() != Some(scope_slot) {
            return Err(self.mark_inconsistent());
        }
        let bucket = state.bucket_index(hash);
        let (previous, entry) = state
            .find_in_bucket(bucket, entry_id)
            .map_err(|_| self.mark_inconsistent())?;
        let scope = state
            .scope(scope_slot)
            .ok_or_else(|| self.mark_inconsistent())?;
        let scope_entry = scope
            .active(entry_id.entry_slot())
            .ok_or_else(|| self.mark_inconsistent())?;
        // Validate every relationship before mutating the bucket chain,
        // entry table, or counters. A bad release cannot erase another
        // owner's lease.
        if scope.held_leases == 0
            || entry.hash != hash
            || entry.locator != locator
            || scope_entry.hash != hash
            || scope_entry.locator != locator
            || scope_entry.next != entry.next
        {
            return Err(self.mark_inconsistent());
        }
        let lease_count = state
            .lease_count
            .checked_sub(1)
            .ok_or_else(|| self.mark_inconsistent())?;
        if let Some(previous) = previous {
            match state
                .active_entry_mut(previous)
                .map_err(|_| self.mark_inconsistent())?
            {
                EntrySlot::Active { next, .. } => *next = entry.next,
                EntrySlot::Free { .. } => return Err(self.mark_inconsistent()),
            }
        } else {
            state.buckets[bucket] = entry.next;
        }
        state
            .scope_mut(scope_slot)
            .ok_or_else(|| self.mark_inconsistent())?
            .remove(entry_id.entry_slot())
            .map_err(|_| self.mark_inconsistent())?;
        state.lease_count = lease_count;
        Ok(())
    }

    fn unregister_scope(&self, scope_slot: usize) -> Result<(), LeaseError> {
        let mut state = self.lock_state()?;
        let scope = state
            .scope(scope_slot)
            .ok_or_else(|| self.mark_inconsistent())?;
        if scope.held_leases != 0 {
            return Err(self.mark_inconsistent());
        }
        let scope_count = state
            .scope_count
            .checked_sub(1)
            .ok_or_else(|| self.mark_inconsistent())?;
        let aggregate_capacity = state
            .aggregate_capacity
            .checked_sub(u64::from(scope.max_leases))
            .ok_or_else(|| self.mark_inconsistent())?;
        if state.free_scope_slots.len() >= state.scopes.len() {
            return Err(self.mark_inconsistent());
        }
        let removed = state.scopes[scope_slot]
            .take()
            .ok_or_else(|| self.mark_inconsistent())?;
        state
            .free_scope_slots
            .push(u32::try_from(scope_slot).map_err(|_| self.mark_inconsistent())?);
        state.scope_count = scope_count;
        state.aggregate_capacity = aggregate_capacity;
        drop(state);
        // Drop the preallocated table after releasing the global mutex.
        drop(removed);
        Ok(())
    }

    fn mark_inconsistent(&self) -> LeaseError {
        self.inner.inconsistent.store(true, Ordering::Release);
        LeaseError::RegistryInconsistent
    }

    #[cfg(test)]
    fn counts(&self) -> Result<(u64, u64, u64), LeaseError> {
        let state = self.lock_state()?;
        Ok((
            state.scope_count,
            state.lease_count,
            state.aggregate_capacity,
        ))
    }

    #[cfg(test)]
    fn raw_locator_is_leased(&self, locator: Locator) -> bool {
        let hash = self.locator_hash(locator);
        let state = self.inner.state.lock().expect("test registry locks");
        let bucket = state.bucket_index(hash);
        state
            .find_locator(bucket, hash, locator)
            .expect("raw test lookup is structurally valid")
            .0
            .is_some()
    }

    #[cfg(test)]
    fn release_as_for_test(
        &self,
        scope_slot: usize,
        locator: Locator,
        entry_id: EntryId,
    ) -> Result<(), LeaseError> {
        self.release(scope_slot, locator, self.locator_hash(locator), entry_id)
    }

    #[cfg(test)]
    fn poison_for_test(&self) {
        let _guard = self.inner.state.lock().expect("test registry locks");
        panic!("poison the test-only runtime lease registry");
    }
}

/// One filelog receiver's bounded share of the process-wide lease registry.
#[derive(Debug)]
pub(crate) struct ReceiverLeaseScope {
    token: Option<Arc<ScopeToken>>,
}

impl ReceiverLeaseScope {
    fn token(&self) -> Result<&Arc<ScopeToken>, LeaseError> {
        self.token.as_ref().ok_or(LeaseError::ScopeClosed)
    }

    /// Acquires one live runtime locator without waiting.
    pub(crate) fn try_acquire(&self, locator: Locator) -> Result<RuntimeFileLease, LeaseError> {
        let token = self.token()?;
        token.registry.acquire(token, locator)
    }

    /// Fails closed if any release, cleanup, or mutex operation discovered
    /// that process-local single-reader enforcement is no longer reliable.
    pub(crate) fn ensure_healthy(&self) -> Result<(), LeaseError> {
        self.token()?.registry.ensure_healthy()
    }

    /// Checks the fail-closed poison and integrity indicators without taking
    /// the process-wide mutex.
    ///
    /// The read worker uses this on its scheduling path so corruption
    /// detected by another receiver cannot leave existing readers running.
    /// Acquire, release, and explicit lifecycle checks still take the mutex
    /// and validate the protected indexes.
    pub(crate) fn ensure_healthy_fast(&self) -> Result<(), LeaseError> {
        self.token()?.registry.ensure_healthy_fast()
    }

    /// Explicitly unregisters the receiver and reports cleanup failure before
    /// the worker announces a successful drain.
    pub(crate) fn close(&mut self) -> Result<(), LeaseError> {
        let token = self.token()?;
        let leases = Arc::strong_count(token).saturating_sub(1);
        if leases != 0 {
            return Err(LeaseError::OutstandingLeases { leases });
        }
        token.registry.unregister_scope(token.scope_slot)?;
        token.registered.store(false, Ordering::Release);
        let _closed = self.token.take();
        Ok(())
    }
}

#[derive(Debug)]
struct ScopeToken {
    registry: RuntimeLeaseRegistry,
    scope_slot: usize,
    registered: AtomicBool,
}

impl Drop for ScopeToken {
    fn drop(&mut self) {
        if self.registered.swap(false, Ordering::AcqRel) {
            let _result = self.registry.unregister_scope(self.scope_slot);
        }
    }
}

/// RAII ownership of one live runtime locator.
///
/// This guard belongs to the logical reader, not to an open file descriptor,
/// so descriptor rotation cannot accidentally release single-reader
/// ownership. Normal finalization calls [`Self::release`] and observes
/// integrity failures; Drop remains the panic/unwinding fallback.
#[derive(Debug)]
pub(crate) struct RuntimeFileLease {
    token: Arc<ScopeToken>,
    locator: Locator,
    hash: u64,
    entry_id: EntryId,
    released: bool,
}

impl RuntimeFileLease {
    /// The normalized runtime locator protected by this lease.
    #[must_use]
    pub(crate) const fn locator(&self) -> Locator {
        self.locator
    }

    /// Explicitly releases the locator and reports any registry failure to
    /// the owning worker.
    pub(crate) fn release(mut self) -> Result<(), LeaseError> {
        self.released = true;
        self.token.registry.release(
            self.token.scope_slot,
            self.locator,
            self.hash,
            self.entry_id,
        )
    }
}

impl Drop for RuntimeFileLease {
    fn drop(&mut self) {
        if !self.released {
            let _result = self.token.registry.release(
                self.token.scope_slot,
                self.locator,
                self.hash,
                self.entry_id,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Barrier;

    use super::*;

    fn registry() -> RuntimeLeaseRegistry {
        RuntimeLeaseRegistry::new(16, 64)
    }

    fn locator(ino: u64) -> Locator {
        Locator::PosixDevIno { dev: 7, ino }
    }

    /// Scenario: two receiver scopes request the same live runtime locator,
    /// then the first guard is explicitly released.
    /// Guarantees: contention has no wait queue, duplicate ownership is
    /// refused, and an observed release immediately permits acquisition.
    #[test]
    fn duplicate_locator_is_refused_until_guard_release() {
        let registry = registry();
        let mut first_scope = registry.register_receiver(1).expect("scope registers");
        let mut second_scope = registry.register_receiver(1).expect("scope registers");
        let first = first_scope
            .try_acquire(locator(1))
            .expect("first lease acquires");

        assert!(matches!(
            second_scope.try_acquire(locator(1)),
            Err(LeaseError::Contended { .. })
        ));
        first.release().expect("release is observed");
        let second = second_scope
            .try_acquire(locator(1))
            .expect("released locator acquires");
        assert_eq!(second.locator(), locator(1));
        second.release().expect("release is observed");
        first_scope.close().expect("scope closes");
        second_scope.close().expect("scope closes");
    }

    /// Scenario: two threads simultaneously acquire the same locator through
    /// distinct receiver scopes.
    /// Guarantees: acquire, duplicate verification, and insertion are one
    /// atomic mutex operation, so exactly one thread obtains the lease.
    #[test]
    fn simultaneous_acquisition_has_exactly_one_winner() {
        let registry = registry();
        let first_scope = registry.register_receiver(1).expect("scope registers");
        let second_scope = registry.register_receiver(1).expect("scope registers");
        let start = Arc::new(Barrier::new(3));
        let finish = Arc::new(Barrier::new(3));
        let spawn = |scope: ReceiverLeaseScope| {
            let start = Arc::clone(&start);
            let finish = Arc::clone(&finish);
            std::thread::spawn(move || {
                let _started = start.wait();
                let lease = scope.try_acquire(locator(1));
                let _finished = finish.wait();
                lease.is_ok()
            })
        };
        let first = spawn(first_scope);
        let second = spawn(second_scope);
        let _started = start.wait();
        let _finished = finish.wait();

        let winners = u8::from(first.join().expect("thread joins"))
            + u8::from(second.join().expect("thread joins"));
        assert_eq!(winners, 1);
    }

    /// Scenario: one receiver reaches its configured tracked-file lease
    /// population while another receiver owns independent capacity.
    /// Guarantees: per-receiver lease populations and aggregate accounting
    /// stay bounded without preventing distinct scopes from using their own
    /// preallocated slots.
    #[test]
    fn receiver_scope_capacity_bounds_live_leases() {
        let registry = registry();
        let first_scope = registry.register_receiver(1).expect("scope registers");
        let second_scope = registry.register_receiver(2).expect("scope registers");
        let _first = first_scope
            .try_acquire(locator(1))
            .expect("first lease acquires");
        assert!(matches!(
            first_scope.try_acquire(locator(2)),
            Err(LeaseError::ScopeCapacityExhausted {
                held: 1,
                max: 1,
                ..
            })
        ));
        let _second = second_scope
            .try_acquire(locator(2))
            .expect("independent scope acquires");
        assert_eq!(registry.counts().expect("registry is healthy"), (2, 2, 3));
    }

    /// Scenario: a logical reader temporarily closes its file descriptor
    /// while retaining the runtime-file lease guard.
    /// Guarantees: descriptor lifetime does not release logical ownership;
    /// only releasing the reader-owned guard makes the locator available.
    #[test]
    fn lease_survives_temporary_descriptor_closure() {
        let registry = registry();
        let scope = registry.register_receiver(1).expect("scope registers");
        let lease = scope.try_acquire(locator(1)).expect("lease acquires");
        let file = File::open(std::env::current_exe().expect("test executable path"))
            .expect("test executable opens");
        drop(file);

        assert!(matches!(
            scope.try_acquire(locator(1)),
            Err(LeaseError::Contended { .. })
        ));
        lease.release().expect("guard release succeeds");
        let _reacquired = scope.try_acquire(locator(1)).expect("release is visible");
    }

    /// Scenario: a panic unwinds through a logical reader that owns a
    /// runtime-file lease.
    /// Guarantees: the RAII fallback releases during unwinding, so a
    /// recovered receiver can acquire the same live locator.
    #[test]
    fn panic_unwinding_releases_the_runtime_lease() {
        let registry = registry();
        let scope = registry.register_receiver(1).expect("scope registers");
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _lease = scope.try_acquire(locator(1)).expect("lease acquires");
            panic!("reader panic");
        }));
        assert!(unwind.is_err());
        let _reacquired = scope
            .try_acquire(locator(1))
            .expect("unwinding released the locator");
    }

    /// Scenario: explicit scope closure is attempted before and after all
    /// lease guards are released.
    /// Guarantees: successful drain cannot hide outstanding reader leases;
    /// final closure releases scope and aggregate accounting observably.
    #[test]
    fn explicit_scope_close_requires_all_guards() {
        let registry = registry();
        let mut scope = registry.register_receiver(2).expect("scope registers");
        let first = scope.try_acquire(locator(1)).expect("lease acquires");
        let second = scope.try_acquire(locator(2)).expect("lease acquires");
        assert!(matches!(
            scope.close(),
            Err(LeaseError::OutstandingLeases { leases: 2 })
        ));
        first.release().expect("lease releases");
        second.release().expect("lease releases");
        scope.close().expect("scope closes");
        assert_eq!(registry.counts().expect("registry is healthy"), (0, 0, 0));
        assert!(matches!(
            scope.ensure_healthy(),
            Err(LeaseError::ScopeClosed)
        ));
    }

    /// Scenario: a caller attempts to lease the sentinel used when required
    /// platform identity is unavailable.
    /// Guarantees: `Unspecified` can never collapse unrelated live files
    /// into one lease key or bypass platform identity requirements.
    #[test]
    fn unspecified_locator_is_refused() {
        let registry = registry();
        let scope = registry.register_receiver(1).expect("scope registers");
        assert!(matches!(
            scope.try_acquire(Locator::Unspecified),
            Err(LeaseError::UnspecifiedLocator)
        ));
        assert_eq!(registry.counts().expect("registry is healthy"), (1, 0, 1));
    }

    /// Scenario: a deliberately tiny locator index receives more distinct
    /// entries than one bucket may examine under the global mutex.
    /// Guarantees: insertion fails with an explicit bounded-overflow error
    /// rather than allowing mutex work to grow with aggregate lease count.
    #[test]
    fn locator_bucket_depth_is_strictly_bounded() {
        let registry = RuntimeLeaseRegistry::new(2, 1);
        let scope = registry
            .register_receiver((MAX_BUCKET_DEPTH + 1) as u32)
            .expect("scope registers");
        let mut leases = Vec::new();
        for ino in 0..MAX_BUCKET_DEPTH {
            leases.push(
                scope
                    .try_acquire(locator(ino as u64))
                    .expect("bounded bucket entry acquires"),
            );
        }
        assert!(matches!(
            scope.try_acquire(locator(MAX_BUCKET_DEPTH as u64)),
            Err(LeaseError::LocatorBucketCapacityExhausted {
                max_depth: MAX_BUCKET_DEPTH,
                ..
            })
        ));
    }

    /// Scenario: an invalid release names a scope that does not own another
    /// scope's live entry.
    /// Guarantees: validation precedes mutation, the valid lease remains
    /// installed, and every operation after the detected inconsistency fails
    /// closed.
    #[test]
    fn inconsistent_release_cannot_erase_another_scope_lease() {
        let registry = registry();
        let first_scope = registry.register_receiver(1).expect("scope registers");
        let second_scope = registry.register_receiver(1).expect("scope registers");
        let lease = first_scope
            .try_acquire(locator(1))
            .expect("first lease acquires");

        let error = registry
            .release_as_for_test(
                second_scope.token().expect("scope is open").scope_slot,
                locator(1),
                lease.entry_id,
            )
            .expect_err("wrong-owner release fails");
        assert!(matches!(error, LeaseError::RegistryInconsistent));
        assert!(registry.raw_locator_is_leased(locator(1)));
        assert!(matches!(
            second_scope.try_acquire(locator(1)),
            Err(LeaseError::RegistryInconsistent)
        ));
        assert!(matches!(
            first_scope.ensure_healthy(),
            Err(LeaseError::RegistryInconsistent)
        ));
        assert!(matches!(
            first_scope.ensure_healthy_fast(),
            Err(LeaseError::RegistryInconsistent)
        ));
    }

    /// Scenario: a thread panics while holding a test-local registry mutex
    /// after a receiver scope has already registered.
    /// Guarantees: active scopes and explicit scope closure observe poisoning
    /// as terminal; no acquisition silently bypasses single-reader
    /// enforcement.
    #[test]
    fn poisoned_registry_fails_active_scopes_and_close_closed() {
        let registry = registry();
        let mut scope = registry.register_receiver(1).expect("scope registers");
        let poison = registry.clone();
        let result = std::thread::spawn(move || poison.poison_for_test()).join();
        assert!(result.is_err());

        assert!(matches!(
            scope.ensure_healthy(),
            Err(LeaseError::RegistryPoisoned)
        ));
        assert!(matches!(
            scope.ensure_healthy_fast(),
            Err(LeaseError::RegistryPoisoned)
        ));
        assert!(matches!(
            scope.try_acquire(locator(1)),
            Err(LeaseError::RegistryPoisoned)
        ));
        assert!(matches!(scope.close(), Err(LeaseError::RegistryPoisoned)));
    }

    /// Scenario: two production registration calls request the same unique
    /// test locator through independently constructed receiver scopes.
    /// Guarantees: production callers cannot construct isolated registries;
    /// both scopes contend through the one process-wide authority.
    #[test]
    fn production_registration_handles_share_one_registry() {
        static NEXT_LOCATOR: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1_000_000);
        let locator = locator(NEXT_LOCATOR.fetch_add(1, Ordering::Relaxed));
        let mut first_scope = register_receiver_scope(1).expect("scope registers");
        let mut second_scope = register_receiver_scope(1).expect("scope registers");
        let first = first_scope.try_acquire(locator).expect("lease acquires");
        assert!(matches!(
            second_scope.try_acquire(locator),
            Err(LeaseError::Contended { .. })
        ));
        first.release().expect("lease releases");
        first_scope.close().expect("scope closes");
        second_scope.close().expect("scope closes");
    }
}
