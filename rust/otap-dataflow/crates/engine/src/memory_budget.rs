// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Minimal observe-only retained-work accounting proof of concept.
//!
//! This module intentionally does not enforce, shed, backpressure, lease, or
//! model hierarchy. It only proves the core ownership shape from the design:
//! a retained item gets a local ticket, the ticket is stored beside retained
//! state, and dropping the ticket refunds the observe-only counters.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;

/// Retained-work site used by the POC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RetainedSiteKind {
    /// Fallback for retained work that has no specific site label.
    Unknown = 0,
    /// Batch processor pending-input buffer.
    BatchPending = 1,
    /// Retry processor delayed retry buffer.
    RetryBuffer = 2,
}

impl RetainedSiteKind {
    /// Number of site slots in fixed-size counters.
    pub const COUNT: usize = 3;

    /// Returns the fixed counter index for this site.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Logical retained-size contract.
pub trait ChargedSize {
    /// Returns logical retained bytes, or `None` when this payload has no safe
    /// estimate yet.
    fn charged_size(&self) -> Option<u64>;
}

impl ChargedSize for u64 {
    fn charged_size(&self) -> Option<u64> {
        Some(*self)
    }
}

impl ChargedSize for usize {
    fn charged_size(&self) -> Option<u64> {
        Some(*self as u64)
    }
}

impl<T: ChargedSize + ?Sized> ChargedSize for &T {
    fn charged_size(&self) -> Option<u64> {
        (*self).charged_size()
    }
}

/// Snapshot of current observe-only retained work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryBudgetSnapshot {
    /// Known logical retained bytes currently owned by tickets.
    pub charged_bytes: u64,
    /// Current count of retained items whose logical size is unknown.
    pub unknown_count: u64,
    /// Known logical retained bytes by retained-work site.
    pub charged_bytes_by_site: [u64; RetainedSiteKind::COUNT],
    /// Unknown-size retained item counts by retained-work site.
    pub unknown_count_by_site: [u64; RetainedSiteKind::COUNT],
}

/// Runtime-local observe-only account.
#[derive(Debug, Default)]
pub struct RuntimeMemoryAccount {
    charged_bytes: Cell<u64>,
    unknown_count: Cell<u64>,
    charged_bytes_by_site: [Cell<u64>; RetainedSiteKind::COUNT],
    unknown_count_by_site: [Cell<u64>; RetainedSiteKind::COUNT],
    _not_send: PhantomData<Rc<()>>,
}

impl RuntimeMemoryAccount {
    /// Charges retained work at `site` and returns the owning ticket.
    #[must_use]
    pub fn charge_at(
        self: &Rc<Self>,
        site: RetainedSiteKind,
        size: impl ChargedSize,
    ) -> LocalMemoryTicket {
        match size.charged_size() {
            Some(bytes) => {
                self.charged_bytes
                    .set(self.charged_bytes.get().saturating_add(bytes));
                self.site_charge(site, bytes);
                LocalMemoryTicket {
                    account: Rc::clone(self),
                    charge: LocalMemoryCharge::Known(bytes),
                    site,
                    active: true,
                    _not_send: PhantomData,
                }
            }
            None => {
                self.unknown_count
                    .set(self.unknown_count.get().saturating_add(1));
                self.site_unknown_inc(site);
                LocalMemoryTicket {
                    account: Rc::clone(self),
                    charge: LocalMemoryCharge::Unknown,
                    site,
                    active: true,
                    _not_send: PhantomData,
                }
            }
        }
    }

    /// Returns current observe-only counters.
    #[must_use]
    pub fn snapshot(&self) -> MemoryBudgetSnapshot {
        MemoryBudgetSnapshot {
            charged_bytes: self.charged_bytes.get(),
            unknown_count: self.unknown_count.get(),
            charged_bytes_by_site: std::array::from_fn(|i| self.charged_bytes_by_site[i].get()),
            unknown_count_by_site: std::array::from_fn(|i| self.unknown_count_by_site[i].get()),
        }
    }

    fn refund(&self, site: RetainedSiteKind, bytes: u64) {
        self.charged_bytes
            .set(self.charged_bytes.get().saturating_sub(bytes));
        let slot = &self.charged_bytes_by_site[site.index()];
        slot.set(slot.get().saturating_sub(bytes));
    }

    fn refund_unknown(&self, site: RetainedSiteKind) {
        self.unknown_count
            .set(self.unknown_count.get().saturating_sub(1));
        let slot = &self.unknown_count_by_site[site.index()];
        slot.set(slot.get().saturating_sub(1));
    }

    fn site_charge(&self, site: RetainedSiteKind, bytes: u64) {
        let slot = &self.charged_bytes_by_site[site.index()];
        slot.set(slot.get().saturating_add(bytes));
    }

    fn site_unknown_inc(&self, site: RetainedSiteKind) {
        let slot = &self.unknown_count_by_site[site.index()];
        slot.set(slot.get().saturating_add(1));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalMemoryCharge {
    Known(u64),
    Unknown,
}

/// RAII owner for one local retained-work charge.
#[derive(Debug)]
pub struct LocalMemoryTicket {
    account: Rc<RuntimeMemoryAccount>,
    charge: LocalMemoryCharge,
    site: RetainedSiteKind,
    active: bool,
    _not_send: PhantomData<Rc<()>>,
}

impl LocalMemoryTicket {
    /// Returns bytes owned by this ticket, if known.
    #[must_use]
    pub const fn bytes(&self) -> Option<u64> {
        match self.charge {
            LocalMemoryCharge::Known(bytes) => Some(bytes),
            LocalMemoryCharge::Unknown => None,
        }
    }
}

impl Drop for LocalMemoryTicket {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        match self.charge {
            LocalMemoryCharge::Known(bytes) => self.account.refund(self.site, bytes),
            LocalMemoryCharge::Unknown => self.account.refund_unknown(self.site),
        }
        self.active = false;
    }
}

thread_local! {
    static CURRENT_RUNTIME_MEMORY_ACCOUNT: RefCell<Option<Rc<RuntimeMemoryAccount>>> =
        const { RefCell::new(None) };
}

/// Guard returned by [`set_current_runtime_memory_account`].
#[derive(Debug)]
#[must_use = "the runtime memory account remains installed until this guard is dropped"]
pub struct RuntimeMemoryAccountGuard {
    previous: Option<Rc<RuntimeMemoryAccount>>,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for RuntimeMemoryAccountGuard {
    fn drop(&mut self) {
        CURRENT_RUNTIME_MEMORY_ACCOUNT.with(|slot| {
            let _ = slot.replace(self.previous.take());
        });
    }
}

/// Installs a runtime-local account for observe-only charge sites.
pub fn set_current_runtime_memory_account(
    account: Option<Rc<RuntimeMemoryAccount>>,
) -> RuntimeMemoryAccountGuard {
    let previous = CURRENT_RUNTIME_MEMORY_ACCOUNT.with(|slot| slot.replace(account));
    RuntimeMemoryAccountGuard {
        previous,
        _not_send: PhantomData,
    }
}

/// Returns the current runtime-local observe-only account, if installed.
#[must_use]
pub fn current_runtime_memory_account() -> Option<Rc<RuntimeMemoryAccount>> {
    CURRENT_RUNTIME_MEMORY_ACCOUNT.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UnknownSize;

    impl ChargedSize for UnknownSize {
        fn charged_size(&self) -> Option<u64> {
            None
        }
    }

    #[test]
    fn ticket_refunds_known_bytes_on_drop() {
        let account = Rc::new(RuntimeMemoryAccount::default());
        let ticket = account.charge_at(RetainedSiteKind::BatchPending, 42_u64);
        assert_eq!(ticket.bytes(), Some(42));
        assert_eq!(account.snapshot().charged_bytes, 42);
        assert_eq!(
            account.snapshot().charged_bytes_by_site[RetainedSiteKind::BatchPending.index()],
            42
        );

        drop(ticket);

        assert_eq!(account.snapshot().charged_bytes, 0);
        assert_eq!(
            account.snapshot().charged_bytes_by_site[RetainedSiteKind::BatchPending.index()],
            0
        );
    }

    #[test]
    fn ticket_refunds_unknown_size_on_drop() {
        let account = Rc::new(RuntimeMemoryAccount::default());
        let ticket = account.charge_at(RetainedSiteKind::RetryBuffer, UnknownSize);
        assert_eq!(ticket.bytes(), None);
        assert_eq!(account.snapshot().unknown_count, 1);
        assert_eq!(
            account.snapshot().unknown_count_by_site[RetainedSiteKind::RetryBuffer.index()],
            1
        );

        drop(ticket);

        assert_eq!(account.snapshot().unknown_count, 0);
        assert_eq!(
            account.snapshot().unknown_count_by_site[RetainedSiteKind::RetryBuffer.index()],
            0
        );
    }
}
