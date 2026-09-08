// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic startup rollback, shutdown, and singleton ownership.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::BackendError;

// A process-global atomic is intentional here: two independent profiler
// instances must never race to attach duplicate system-wide perf events. No
// runtime data crosses cores through this flag.
static LEASE_HELD: AtomicBool = AtomicBool::new(false);

/// Process-local proof that this backend owns the singleton profiler lease.
#[derive(Debug)]
pub struct SingletonLease {
    held: bool,
}

impl SingletonLease {
    /// Attempts to acquire the process-local profiler lease.
    pub fn acquire() -> Result<Self, BackendError> {
        let _previous = LEASE_HELD
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                BackendError::Unsupported("profiler singleton already active".to_owned())
            })?;
        Ok(Self { held: true })
    }

    /// Explicitly releases the lease. Repeated calls are harmless.
    pub fn release(&mut self) {
        if self.held {
            LEASE_HELD.store(false, Ordering::Release);
            self.held = false;
        }
    }
}

impl Drop for SingletonLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// A resource that can be deterministically stopped after partial startup.
pub trait ManagedResource {
    /// Stops the resource. Implementations must be idempotent.
    fn shutdown(&mut self);
}

/// Collects resources until startup either commits or rolls back.
#[derive(Debug)]
pub struct StartupTransaction<R: ManagedResource> {
    resources: Vec<R>,
    committed: bool,
}

impl<R: ManagedResource> StartupTransaction<R> {
    /// Creates an empty startup transaction.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resources: Vec::new(),
            committed: false,
        }
    }

    /// Records a successfully created resource.
    pub fn push(&mut self, resource: R) {
        self.resources.push(resource);
    }

    /// Commits startup and transfers resources into a running owner.
    #[must_use]
    pub fn commit(mut self) -> RunningResources<R> {
        self.committed = true;
        RunningResources {
            resources: std::mem::take(&mut self.resources),
            stopped: false,
        }
    }
}

impl<R: ManagedResource> Default for StartupTransaction<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: ManagedResource> Drop for StartupTransaction<R> {
    fn drop(&mut self) {
        if !self.committed {
            for resource in self.resources.iter_mut().rev() {
                resource.shutdown();
            }
        }
    }
}

/// Running resources with reverse-order idempotent shutdown.
#[derive(Debug)]
pub struct RunningResources<R: ManagedResource> {
    resources: Vec<R>,
    stopped: bool,
}

impl<R: ManagedResource> RunningResources<R> {
    /// Stops every resource in reverse construction order.
    pub fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        for resource in self.resources.iter_mut().rev() {
            resource.shutdown();
        }
        self.stopped = true;
    }
}

impl<R: ManagedResource> Drop for RunningResources<R> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug)]
    struct Resource {
        id: u8,
        events: Rc<RefCell<Vec<u8>>>,
        stopped: bool,
    }

    impl ManagedResource for Resource {
        fn shutdown(&mut self) {
            if !self.stopped {
                self.events.borrow_mut().push(self.id);
                self.stopped = true;
            }
        }
    }

    /// Scenario: Startup fails after three resources have been created.
    /// Guarantees: Dropping the uncommitted transaction rolls resources back
    /// exactly once in reverse startup order.
    #[test]
    fn partial_startup_rolls_back_in_reverse() {
        let events = Rc::new(RefCell::new(Vec::new()));
        {
            let mut transaction = StartupTransaction::new();
            for id in 1..=3 {
                transaction.push(Resource {
                    id,
                    events: Rc::clone(&events),
                    stopped: false,
                });
            }
        }
        assert_eq!(*events.borrow(), [3, 2, 1]);
    }

    /// Scenario: Shutdown is requested twice after successful startup.
    /// Guarantees: Each resource is stopped once and repeated shutdown has no
    /// additional side effects.
    #[test]
    fn shutdown_is_idempotent() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut transaction = StartupTransaction::new();
        transaction.push(Resource {
            id: 1,
            events: Rc::clone(&events),
            stopped: false,
        });
        let mut running = transaction.commit();
        running.shutdown();
        running.shutdown();
        assert_eq!(*events.borrow(), [1]);
    }

    /// Scenario: Two backend instances attempt to own the process singleton.
    /// Guarantees: The second acquisition fails and succeeds after the first
    /// lease is released.
    #[test]
    fn singleton_lease_excludes_overlap() {
        let mut first = SingletonLease::acquire().expect("first lease");
        assert!(SingletonLease::acquire().is_err());
        first.release();
        assert!(SingletonLease::acquire().is_ok());
    }
}
