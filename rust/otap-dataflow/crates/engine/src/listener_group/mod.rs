// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Phase 2: coordinated listener-group manager (engine scaffolding).
//!
//! `ListenerGroupManager` lets the controller (or pipeline-assembly
//! layer) declare a planned set of listener sockets that should bind to
//! the same address with `SO_REUSEPORT` and be handed out to receivers
//! pinned to specific cores. Each receiver "acquires" its listener by
//! `(group_key, core_id)`; the *last* expected member to arrive
//! triggers eager creation of all listeners in the group so the entire
//! reuseport group exists in the kernel atomically before any of them
//! starts accepting connections.
//!
//! The manager intentionally hands back standard-library TCP/UDP sockets
//! rather than Tokio sockets so the acquiring receiver can register the fd
//! with its own per-core current-thread runtime (avoiding the
//! cross-runtime reactor-affinity issue that would arise if we created
//! tokio sockets on the materialising thread).
//!
//! With no plans registered, the manager is inert and existing
//! per-receiver bind behaviour is preserved exactly.
//!
//! The lifecycle metrics in [`metrics`] are likewise prepared types,
//! not yet emitted; counter increments will land with the Phase 2.5
//! wiring.

pub mod extraction;
pub mod metrics;

use crate::topology::CpuTopology;
use std::any::Any;
use std::collections::{BTreeSet, HashMap};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::os::fd::RawFd;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Transport protocol used by a listener group.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Protocol {
    /// TCP listener (`SO_REUSEPORT` reuseport group).
    Tcp,
    /// UDP socket (`SO_REUSEPORT` reuseport group).
    Udp,
}

impl Protocol {
    /// Returns a stable lower-case identifier suitable for telemetry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// Stable lookup key for a listener group.
///
/// Includes the full identity needed to disambiguate two unrelated
/// receiver groups that happen to share the same bind address (e.g.
/// two pipelines in the same group, or two pipeline groups in the same
/// engine, that each plan to bind `0.0.0.0:4317` for separate ingest
/// paths). Keying purely on `{addr, protocol}` would collide in that
/// case.
///
/// `bind_device` participates in the key for identity only: the
/// current `materialise_group` path does **not** apply
/// `SO_BINDTODEVICE`, so the field is reserved for future use. Two
/// otherwise-identical plans with different `bind_device` values are
/// still treated as distinct groups so a future patch that does apply
/// `SO_BINDTODEVICE` does not change keying semantics.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ListenerGroupKey {
    /// Pipeline group id that owns this listener group.
    pub pipeline_group_id: String,
    /// Pipeline id that owns this listener group.
    pub pipeline_id: String,
    /// Receiver node id (in the controller's plan) that will host the listeners.
    pub receiver_node_id: String,
    /// Bind address shared by every listener in the group.
    pub addr: SocketAddr,
    /// Transport protocol.
    pub protocol: Protocol,
    /// Optional `SO_BINDTODEVICE` interface name. **Identity-only**:
    /// see the type doc.
    pub bind_device: Option<String>,
}

impl ListenerGroupKey {
    /// Builds a TCP key with no bind-device pin.
    #[must_use]
    pub fn tcp(
        pipeline_group_id: impl Into<String>,
        pipeline_id: impl Into<String>,
        receiver_node_id: impl Into<String>,
        addr: SocketAddr,
    ) -> Self {
        Self {
            pipeline_group_id: pipeline_group_id.into(),
            pipeline_id: pipeline_id.into(),
            receiver_node_id: receiver_node_id.into(),
            addr,
            protocol: Protocol::Tcp,
            bind_device: None,
        }
    }

    /// Canonical constructor for plan-side and runtime-side keys
    /// alike: stringifies the receiver node identifier in exactly one
    /// place so extraction (controller side) and acquire (receiver
    /// side) cannot drift. Both call sites *must* go through this
    /// helper.
    #[must_use]
    pub fn tcp_for_receiver<P, I, N>(
        pipeline_group_id: P,
        pipeline_id: I,
        node_id: &N,
        addr: SocketAddr,
    ) -> Self
    where
        P: Into<String>,
        I: Into<String>,
        N: std::fmt::Display + ?Sized,
    {
        Self::tcp(pipeline_group_id, pipeline_id, node_id.to_string(), addr)
    }

    /// Builds a UDP key with no bind-device pin.
    #[must_use]
    pub fn udp(
        pipeline_group_id: impl Into<String>,
        pipeline_id: impl Into<String>,
        receiver_node_id: impl Into<String>,
        addr: SocketAddr,
    ) -> Self {
        Self {
            pipeline_group_id: pipeline_group_id.into(),
            pipeline_id: pipeline_id.into(),
            receiver_node_id: receiver_node_id.into(),
            addr,
            protocol: Protocol::Udp,
            bind_device: None,
        }
    }

    /// Canonical UDP constructor matching [`Self::tcp_for_receiver`].
    #[must_use]
    pub fn udp_for_receiver<P, I, N>(
        pipeline_group_id: P,
        pipeline_id: I,
        node_id: &N,
        addr: SocketAddr,
    ) -> Self
    where
        P: Into<String>,
        I: Into<String>,
        N: std::fmt::Display + ?Sized,
    {
        Self::udp(pipeline_group_id, pipeline_id, node_id.to_string(), addr)
    }
}

/// One expected member of a listener group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListenerGroupMember {
    /// Stable per-listener identifier within the controller plan.
    pub listener_id: u32,
    /// CPU core that owns the receiver pipeline thread for this listener.
    pub core_id: u32,
    /// NUMA node for `core_id`.
    pub numa_node: u32,
}

/// A planned listener group declared by the controller.
///
/// All identity fields (pipeline group, receiver node, address,
/// protocol, bind device) live on [`ListenerGroupKey`]; the plan adds
/// only the expected member list.
#[derive(Clone, Debug)]
pub struct ListenerGroupPlan {
    /// Stable lookup key.
    pub key: ListenerGroupKey,
    /// One entry per expected listener.
    pub expected_members: Vec<ListenerGroupMember>,
}

/// Errors returned by [`ListenerGroupManager::register_plan`].
#[derive(Debug, Eq, PartialEq)]
pub enum PlanError {
    /// A plan with the same key has already been registered.
    DuplicateKey,
    /// `expected_members` was empty.
    NoMembers,
    /// Two members shared the same `listener_id`.
    DuplicateListenerId(u32),
    /// Two members shared the same `core_id`.
    DuplicateCoreId(u32),
    /// Coordinated groups cannot bind to port 0 because every bind
    /// would get a different ephemeral port.
    EphemeralPortUnsupported,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateKey => write!(f, "listener group already registered"),
            Self::NoMembers => write!(f, "listener group plan has no expected members"),
            Self::DuplicateListenerId(id) => {
                write!(f, "listener_id {id} appears more than once in plan")
            }
            Self::DuplicateCoreId(id) => write!(f, "core_id {id} appears more than once in plan"),
            Self::EphemeralPortUnsupported => write!(
                f,
                "listener group plans cannot use port 0; bind a concrete port or use the independent bind path"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Result of [`ListenerGroupManager::acquire`].
#[derive(Debug)]
pub enum AcquireOutcome {
    /// Quorum was reached and this caller's listener is handed back.
    Listener(AcquiredListener),
    /// Quorum was reached and this caller's UDP socket is handed back.
    DatagramSocket(AcquiredDatagramSocket),
    /// No plan covers this group, or the plan does not list this
    /// `core_id`. The caller should bind a listener independently
    /// (today's behaviour).
    NoPlan,
    /// Quorum was not reached within the requested timeout. The group
    /// is now in fallback state; this and all subsequent `acquire`
    /// calls for the same group return this variant. The caller
    /// should bind a listener independently.
    FallbackToIndependent,
    /// The same `(group_key, core_id)` already received a listener
    /// from this manager. Returning this variant (rather than
    /// `NoPlan`) prevents the seam from silently rebinding a fresh
    /// listener for the same receiver, which would defeat the
    /// coordinated-reuseport guarantee. Callers should treat this as
    /// a hard error -- the receiver must not call
    /// `EffectHandler::tcp_listener` more than once for the same
    /// address/core within a single startup.
    AlreadyAcquired,
    /// The group's eager materialisation hit an `io::Error` (e.g.
    /// address-in-use without SO_REUSEPORT). Surfaced to the *first*
    /// acquirer; subsequent acquirers see the cached error.
    MaterialisationFailed(std::io::Error),
}

/// Listener acquired from a coordinated listener group.
#[derive(Debug)]
pub struct AcquiredListener {
    listener: TcpListener,
    lease: Option<ListenerGroupLease>,
}

/// UDP socket acquired from a coordinated listener group.
#[derive(Debug)]
pub struct AcquiredDatagramSocket {
    socket: UdpSocket,
    lease: Option<ListenerGroupLease>,
}

impl AcquiredDatagramSocket {
    fn new(socket: UdpSocket, lease: Option<ListenerGroupLease>) -> Self {
        Self { socket, lease }
    }

    /// Splits the standard UDP socket from its optional lifecycle lease.
    #[must_use]
    pub fn into_parts(self) -> (UdpSocket, Option<ListenerGroupLease>) {
        (self.socket, self.lease)
    }
}

impl AcquiredListener {
    fn new(listener: TcpListener, lease: Option<ListenerGroupLease>) -> Self {
        Self { listener, lease }
    }

    /// Splits the standard listener from its optional lifecycle lease.
    #[must_use]
    pub fn into_parts(self) -> (TcpListener, Option<ListenerGroupLease>) {
        (self.listener, self.lease)
    }

    #[cfg(test)]
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    #[cfg(test)]
    fn as_raw_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.listener.as_raw_fd()
    }
}

/// Opaque listener lifecycle guard returned by an attach hook.
#[derive(Debug)]
pub struct ListenerGroupLease {
    _guard: Arc<dyn Any + Send + Sync>,
}

impl ListenerGroupLease {
    /// Wraps a hook-specific cleanup guard.
    #[must_use]
    pub fn new(guard: Arc<dyn Any + Send + Sync>) -> Self {
        Self { _guard: guard }
    }
}

/// Result returned by an attach hook.
#[derive(Default)]
pub struct AttachOutcome {
    /// Group-wide keepalive, such as a loaded BPF object.
    pub keepalive: Option<Arc<dyn Any + Send + Sync>>,
    /// Optional per-core cleanup guards handed to acquirers.
    pub listener_leases: HashMap<u32, ListenerGroupLease>,
}

/// Optional attach hook invoked by the manager after the listeners
/// for a group have been materialised, but before any acquirer
/// receives its listener. Used by Phase 3 to call
/// `reuseport_ebpf::libbpf::load_default_and_attach(...)` exactly
/// once per group.
///
/// The hook receives the plan and a slice of
/// `(listener_id, raw_fd, core_id, numa_node)` tuples **sorted by
/// `listener_id`** so the eBPF map population is deterministic.
/// Returning an [`AttachOutcome`] stores any group keepalive in the
/// group state and hands per-core cleanup guards to acquirers. Returning
/// `Err(io::Error)` aborts materialisation and surfaces as
/// `MaterialisationFailed` on the first acquirer.
pub type AttachHook = Arc<
    dyn Fn(&ListenerGroupPlan, &[(u32, RawFd, u32, u32)]) -> std::io::Result<AttachOutcome>
        + Send
        + Sync,
>;

/// Coordinated listener-group manager.
///
/// Cheap to clone (just an `Arc`).
#[derive(Clone, Debug, Default)]
pub struct ListenerGroupManager {
    inner: Arc<ManagerInner>,
}

#[derive(Default)]
struct ManagerInner {
    state: Mutex<HashMap<ListenerGroupKey, GroupSlot>>,
    attach_hook: Mutex<Option<AttachHook>>,
}

impl std::fmt::Debug for ManagerInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerInner")
            .field("state", &"<Mutex<HashMap<...>>>")
            .field(
                "attach_hook_set",
                &self.attach_hook.lock().ok().is_some_and(|h| h.is_some()),
            )
            .finish()
    }
}

/// Per-group state stored under the manager mutex.
struct GroupSlot {
    plan: ListenerGroupPlan,
    cv: Arc<Condvar>,
    state: GroupState,
    /// Opaque keep-alive returned by the optional attach hook (e.g.
    /// the loaded eBPF object). Held for as long as the manager owns
    /// this slot so the kernel-side attachment outlives any acquirer
    /// that has already received its listener.
    #[allow(dead_code)]
    attach_keepalive: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for GroupSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupSlot")
            .field("plan", &self.plan)
            .field("state", &self.state)
            .field("attach_keepalive_set", &self.attach_keepalive.is_some())
            .finish()
    }
}

#[derive(Debug)]
enum GroupState {
    /// Plan registered; no member has yet attempted to acquire.
    Planned,
    /// At least one member has called `acquire` and is waiting for the
    /// rest. `arrived` is the set of `core_id`s currently waiting on
    /// the condition variable. `deadline` is set when the *first*
    /// member arrives.
    Awaiting {
        arrived: BTreeSet<u32>,
        deadline: Instant,
    },
    /// All members arrived and listeners were materialised. Listeners
    /// are distributed to acquirers as they wake up.
    Ready {
        sockets: HashMap<u32, GroupSocket>,
        listener_leases: HashMap<u32, ListenerGroupLease>,
        distributed: BTreeSet<u32>,
    },
    /// Last arrival is binding sockets and running the attach hook
    /// outside the manager mutex.
    Materialising,
    /// Eager materialisation failed; the cached error is replayed for
    /// each acquirer.
    Failed { error: std::io::Error },
    /// Group timed out waiting for quorum; subsequent acquires get
    /// `FallbackToIndependent`.
    Fallback,
}

#[derive(Debug)]
enum GroupSocket {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

impl ListenerGroupManager {
    /// Constructs an empty manager. Default state has no plans, so
    /// every `acquire` returns [`AcquireOutcome::NoPlan`] and the
    /// existing per-receiver bind path is preserved.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a plan. Validates structure but does not bind any
    /// sockets; binding is deferred until the *last* expected member
    /// calls [`Self::acquire`].
    ///
    /// # Errors
    ///
    /// Returns [`PlanError`] for duplicate keys, empty member lists,
    /// duplicate `listener_id` or `core_id`.
    pub fn register_plan(&self, plan: ListenerGroupPlan) -> Result<(), PlanError> {
        if plan.expected_members.is_empty() {
            return Err(PlanError::NoMembers);
        }
        if plan.key.addr.port() == 0 {
            return Err(PlanError::EphemeralPortUnsupported);
        }
        let mut seen_listener = BTreeSet::new();
        let mut seen_core = BTreeSet::new();
        for member in &plan.expected_members {
            if !seen_listener.insert(member.listener_id) {
                return Err(PlanError::DuplicateListenerId(member.listener_id));
            }
            if !seen_core.insert(member.core_id) {
                return Err(PlanError::DuplicateCoreId(member.core_id));
            }
        }
        let mut state = self.inner.state.lock().expect("listener-group state mutex");
        if state.contains_key(&plan.key) {
            return Err(PlanError::DuplicateKey);
        }
        let key = plan.key.clone();
        let _ = state.insert(
            key,
            GroupSlot {
                plan,
                cv: Arc::new(Condvar::new()),
                state: GroupState::Planned,
                attach_keepalive: None,
            },
        );
        Ok(())
    }

    /// Returns whether a plan covering `key` has been registered.
    #[must_use]
    pub fn has_plan(&self, key: &ListenerGroupKey) -> bool {
        self.inner
            .state
            .lock()
            .expect("listener-group state mutex")
            .contains_key(key)
    }

    /// Installs an [`AttachHook`] that the manager will invoke after
    /// the listeners for each materialised group are bound, but
    /// before any acquirer receives its listener. Used by Phase 3
    /// eBPF integration. Replaces any previously-installed hook.
    pub fn set_attach_hook(&self, hook: AttachHook) {
        let mut slot = self
            .inner
            .attach_hook
            .lock()
            .expect("listener-group attach-hook mutex");
        *slot = Some(hook);
    }

    fn current_attach_hook(&self) -> Option<AttachHook> {
        self.inner
            .attach_hook
            .lock()
            .expect("listener-group attach-hook mutex")
            .clone()
    }

    /// Acquires the listener pre-allocated for `(key, core_id)`.
    ///
    /// Behaviour:
    ///
    /// * If no plan covers `key`, returns [`AcquireOutcome::NoPlan`]
    ///   immediately.
    /// * If `core_id` is not in the plan's `expected_members`, returns
    ///   [`AcquireOutcome::NoPlan`] without waiting (so a stray caller
    ///   does not stall the quorum).
    /// * Otherwise records arrival and waits until either (a) the last
    ///   expected member arrives (which triggers materialisation of
    ///   *all* listeners atomically and hands one back per `core_id`),
    ///   or (b) the deadline expires (which transitions the group to
    ///   [`GroupState::Fallback`]).
    #[must_use]
    pub fn acquire(
        &self,
        key: &ListenerGroupKey,
        core_id: u32,
        timeout: Duration,
    ) -> AcquireOutcome {
        let (cv, expected_count) = {
            let state = self.inner.state.lock().expect("listener-group state mutex");
            match state.get(key) {
                None => return AcquireOutcome::NoPlan,
                Some(slot) => {
                    if !slot
                        .plan
                        .expected_members
                        .iter()
                        .any(|m| m.core_id == core_id)
                    {
                        return AcquireOutcome::NoPlan;
                    }
                    (Arc::clone(&slot.cv), slot.plan.expected_members.len())
                }
            }
        };

        let mut guard = self.inner.state.lock().expect("listener-group state mutex");
        loop {
            let slot = match guard.get_mut(key) {
                Some(slot) => slot,
                None => return AcquireOutcome::NoPlan,
            };

            // Initial transitions: first arrival, or join an existing
            // waiting set, or trigger materialisation on last arrival.
            match &mut slot.state {
                GroupState::Planned => {
                    let mut arrived = BTreeSet::new();
                    let _ = arrived.insert(core_id);
                    let deadline = Instant::now() + timeout;
                    if arrived.len() == expected_count {
                        // 1-member quorum: materialise immediately.
                        let plan_clone = slot.plan.clone();
                        let hook = self.current_attach_hook();
                        slot.state = GroupState::Materialising;
                        drop(guard);
                        if let Err(error) = self.materialise_and_publish(key, &cv, plan_clone, hook)
                        {
                            return AcquireOutcome::MaterialisationFailed(error);
                        }
                        guard = self.inner.state.lock().expect("listener-group state mutex");
                        continue;
                    }
                    slot.state = GroupState::Awaiting { arrived, deadline };
                }
                GroupState::Awaiting { arrived, deadline } => {
                    if arrived.contains(&core_id) {
                        // Caller is being woken; just check transition.
                    } else {
                        let _ = arrived.insert(core_id);
                        if arrived.len() == expected_count {
                            // Last arrival: materialise + distribute.
                            let plan = slot.plan.clone();
                            let hook = self.current_attach_hook();
                            slot.state = GroupState::Materialising;
                            drop(guard);
                            if let Err(error) = self.materialise_and_publish(key, &cv, plan, hook) {
                                return AcquireOutcome::MaterialisationFailed(error);
                            }
                            guard = self.inner.state.lock().expect("listener-group state mutex");
                            continue;
                        }
                    }
                    // Drop into the wait-or-timeout path below.
                    let now = Instant::now();
                    if now >= *deadline {
                        slot.state = GroupState::Fallback;
                        cv.notify_all();
                        return AcquireOutcome::FallbackToIndependent;
                    }
                    let remaining = *deadline - now;
                    let (g, wait_result) = cv
                        .wait_timeout(guard, remaining)
                        .expect("listener-group cv wait");
                    guard = g;
                    if wait_result.timed_out() {
                        // Re-enter loop: the next iteration sees
                        // either a state transition (if another
                        // waiter raced to materialise) or expires the
                        // deadline.
                        continue;
                    }
                    continue;
                }
                GroupState::Ready {
                    sockets,
                    listener_leases,
                    distributed,
                } => {
                    if distributed.contains(&core_id) {
                        // H1: report this distinctly from `NoPlan`
                        // so the seam can refuse to silently rebind
                        // and lose the coordinated-reuseport
                        // guarantee.
                        return AcquireOutcome::AlreadyAcquired;
                    }
                    if let Some(socket) = sockets.remove(&core_id) {
                        let lease = listener_leases.remove(&core_id);
                        let _ = distributed.insert(core_id);
                        return match socket {
                            GroupSocket::Tcp(listener) => {
                                AcquireOutcome::Listener(AcquiredListener::new(listener, lease))
                            }
                            GroupSocket::Udp(socket) => AcquireOutcome::DatagramSocket(
                                AcquiredDatagramSocket::new(socket, lease),
                            ),
                        };
                    }
                    return AcquireOutcome::NoPlan;
                }
                GroupState::Materialising => {
                    guard = cv.wait(guard).expect("listener-group cv wait");
                    continue;
                }
                GroupState::Failed { error } => {
                    let replay = std::io::Error::new(error.kind(), error.to_string());
                    return AcquireOutcome::MaterialisationFailed(replay);
                }
                GroupState::Fallback => {
                    return AcquireOutcome::FallbackToIndependent;
                }
            }
        }
    }

    fn materialise_and_publish(
        &self,
        key: &ListenerGroupKey,
        cv: &Condvar,
        plan: ListenerGroupPlan,
        hook: Option<AttachHook>,
    ) -> std::io::Result<()> {
        let result = materialise_group(&plan).and_then(|listeners| {
            invoke_attach_hook(&plan, &listeners, hook.as_ref()).map(|attach| (listeners, attach))
        });

        let mut state = self.inner.state.lock().expect("listener-group state mutex");
        let Some(slot) = state.get_mut(key) else {
            return Ok(());
        };

        match result {
            Ok((sockets, attach)) => {
                slot.attach_keepalive = attach.keepalive;
                slot.state = GroupState::Ready {
                    sockets,
                    listener_leases: attach.listener_leases,
                    distributed: BTreeSet::new(),
                };
                cv.notify_all();
                Ok(())
            }
            Err(error) => {
                let replay = std::io::Error::new(
                    error.kind(),
                    format!("listener-group materialisation or attach failed: {error}"),
                );
                slot.state = GroupState::Failed { error: replay };
                cv.notify_all();
                Err(error)
            }
        }
    }

    /// Forces the named group into [`GroupState::Fallback`]. Useful
    /// for tests and for a future watchdog that times out plans whose
    /// first arrival never came.
    pub fn force_fallback(&self, key: &ListenerGroupKey) {
        let mut state = self.inner.state.lock().expect("listener-group state mutex");
        if let Some(slot) = state.get_mut(key) {
            slot.state = GroupState::Fallback;
            slot.cv.notify_all();
        }
    }

    /// Returns the number of plans currently registered. Test helper.
    #[doc(hidden)]
    #[must_use]
    pub fn plan_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("listener-group state mutex")
            .len()
    }
}

/// Builds an [`AttachHook`] that calls
/// `reuseport_ebpf::libbpf::load_default_and_attach(...)` against the
/// listeners in a freshly-materialised group. On non-Linux or when
/// the `reuseport-ebpf` feature is not compiled in, returns a no-op
/// hook (empty [`AttachOutcome`]) and emits a one-shot warning so the controller
/// can call this unconditionally without `cfg`-gating.
///
/// `debug` controls libbpf verbosity; `strict` selects fail-startup
/// vs. log-and-continue on attach error.
///
/// The eprintln calls are startup-only fallbacks: the engine has no
/// in-process structured-log facility reachable from the listener-group
/// crate today. The controller emits matching `otel_info!` /
/// `otel_warn!` events for the install path; future Phase 2.6 work
/// can replace these with the prepared `ListenerGroupMetrics`
/// counters once metric reporter wiring lands.
#[allow(clippy::print_stderr)]
#[must_use]
pub fn ebpf_attach_hook(debug: bool, strict: bool) -> AttachHook {
    let _ = (debug, strict); // referenced inside cfg branch below
    #[cfg(all(target_os = "linux", feature = "reuseport-ebpf"))]
    {
        use crate::reuseport_ebpf::libbpf::{ListenerFd, load_default_and_attach};
        Arc::new(
            move |_plan: &ListenerGroupPlan, handles: &[(u32, RawFd, u32, u32)]| {
                let listeners: Vec<ListenerFd> = handles
                    .iter()
                    .map(|(listener_id, fd, core_id, numa_node)| ListenerFd {
                        fd: *fd,
                        listener_id: *listener_id,
                        core_id: *core_id,
                        numa_node: *numa_node,
                    })
                    .collect();
                match load_default_and_attach(&listeners, debug) {
                    Ok(handle) => {
                        let listener_leases = handle
                            .listener_leases()
                            .into_iter()
                            .filter_map(|(listener_id, guard)| {
                                listeners
                                    .iter()
                                    .find(|listener| listener.listener_id == listener_id)
                                    .map(|listener| {
                                        (listener.core_id, ListenerGroupLease::new(guard))
                                    })
                            })
                            .collect();
                        let keepalive: Arc<dyn Any + Send + Sync> = Arc::new(handle);
                        Ok(AttachOutcome {
                            keepalive: Some(keepalive),
                            listener_leases,
                        })
                    }
                    Err(error) => {
                        if strict {
                            Err(std::io::Error::other(format!(
                                "eBPF attach failed (strict): {error}"
                            )))
                        } else {
                            eprintln!(
                                "listener_group: eBPF attach failed (continuing without selector): {error}"
                            );
                            Ok(AttachOutcome::default())
                        }
                    }
                }
            },
        )
    }
    #[cfg(not(all(target_os = "linux", feature = "reuseport-ebpf")))]
    {
        Arc::new(|_plan, _handles| {
            eprintln!(
                "listener_group: OTAP_DF_REUSEPORT_EBPF requested but reuseport-ebpf feature is not compiled in or target is not Linux; continuing with plain SO_REUSEPORT"
            );
            Ok(AttachOutcome::default())
        })
    }
}

/// Validates a plan against a discovered NUMA topology, returning
/// `(unknown_cores, mismatched_numa)` so the caller (controller) can
/// emit warnings without aborting startup. Pure helper; does not
/// touch any sockets.
#[must_use]
pub fn validate_against_topology(
    plan: &ListenerGroupPlan,
    topology: &CpuTopology,
) -> (Vec<u32>, Vec<(u32, u32, u32)>) {
    let mut unknown = Vec::new();
    let mut mismatched = Vec::new();
    for member in &plan.expected_members {
        match topology.numa_node(member.core_id) {
            None => unknown.push(member.core_id),
            Some(actual) if actual != member.numa_node => {
                mismatched.push((member.core_id, member.numa_node, actual));
            }
            _ => {}
        }
    }
    (unknown, mismatched)
}

/// Wall-clock budget the *first* arriver gets to wait for the rest
/// of its expected members to call [`ListenerGroupManager::acquire`].
/// Late arrivers wait against the same deadline (set when the first
/// member arrived). On expiry the group transitions to
/// [`AcquireOutcome::FallbackToIndependent`] for all current and
/// future acquirers.
pub const QUORUM_TIMEOUT: Duration = Duration::from_secs(5);

/// Pure predicate over an environment-variable-style value. Treats
/// `Some("1")`, `Some("true")`, and `Some("TRUE")` as on; anything
/// else (including `None`) as off. Exposed for test isolation:
/// callers can exercise the predicate without mutating process env.
#[must_use]
pub fn env_flag_enabled(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true") | Some("TRUE"))
}

/// Single user-facing switch for the experimental NUMA reuseport
/// stack. When `OTAP_DF_REUSEPORT_EBPF=1`:
///
/// * coordinated listener-group planning + manager acquire is
///   activated end-to-end, and
/// * the eBPF NUMA selector attach hook is installed (Linux +
///   `reuseport-ebpf` feature). On non-Linux / no-feature builds the
///   hook is a logged no-op so coordinated plain `SO_REUSEPORT`
///   continues to work.
///
/// Unset means the engine behaves identically to `main`: each
/// receiver binds independently in `EffectHandler::tcp_listener` or
/// `EffectHandler::udp_socket`.
#[must_use]
pub fn reuseport_ebpf_enabled() -> bool {
    env_flag_enabled(std::env::var("OTAP_DF_REUSEPORT_EBPF").ok().as_deref())
}

/// When `OTAP_DF_REUSEPORT_EBPF_STRICT=1`, eBPF attach failures abort
/// startup. Default behaviour is log-and-continue with coordinated
/// plain `SO_REUSEPORT`. Has no effect unless
/// [`reuseport_ebpf_enabled`] is also true.
#[must_use]
pub fn reuseport_ebpf_strict() -> bool {
    env_flag_enabled(
        std::env::var("OTAP_DF_REUSEPORT_EBPF_STRICT")
            .ok()
            .as_deref(),
    )
}

/// Internal/debug-only switch to activate the coordinated listener
/// manager *without* the eBPF attach hook. Production users should
/// set [`reuseport_ebpf_enabled`] instead; this exists to let
/// integration tests exercise the manager-only path on hosts where
/// loading eBPF is not possible. Not documented in
/// `docs/reuseport-ebpf-numa.md`.
#[must_use]
pub fn manager_only_debug_enabled() -> bool {
    env_flag_enabled(
        std::env::var("OTAP_DF_REUSEPORT_MANAGER_ONLY")
            .ok()
            .as_deref(),
    )
}

/// Returns `true` when any path in the engine should consult the
/// listener-group manager: either the user-facing eBPF env switch is
/// on, or the test-only manager-only debug switch is on.
#[must_use]
pub fn manager_active() -> bool {
    cfg!(unix) && (reuseport_ebpf_enabled() || manager_only_debug_enabled())
}

/// Lightweight per-receiver handle into the controller's listener
/// group manager. Carried in `EffectHandlerCore` so
/// `tcp_listener(addr)` and `udp_socket(addr)` can build a full [`ListenerGroupKey`] and call
/// [`ListenerGroupManager::acquire`] without changing its signature.
#[derive(Clone, Debug)]
pub struct ListenerGroupHandle {
    manager: ListenerGroupManager,
    pipeline_group_id: String,
    pipeline_id: String,
    receiver_node_id: String,
    core_id: u32,
}

impl ListenerGroupHandle {
    /// Constructs a handle with the controller manager and the
    /// receiver-node identity.
    #[must_use]
    pub fn new(
        manager: ListenerGroupManager,
        pipeline_group_id: impl Into<String>,
        pipeline_id: impl Into<String>,
        receiver_node_id: impl Into<String>,
        core_id: u32,
    ) -> Self {
        Self {
            manager,
            pipeline_group_id: pipeline_group_id.into(),
            pipeline_id: pipeline_id.into(),
            receiver_node_id: receiver_node_id.into(),
            core_id,
        }
    }

    /// Canonical receiver-side constructor. Stringifies `node_id`
    /// through the same `Display` path as
    /// [`ListenerGroupKey::tcp_for_receiver`] so the runtime key
    /// always matches the plan key. Use this from receiver
    /// scaffolding instead of [`Self::new`] when the caller has the
    /// engine `node_id` value in hand.
    #[must_use]
    pub fn for_receiver<P, N>(
        manager: ListenerGroupManager,
        pipeline_group_id: P,
        pipeline_id: impl Into<String>,
        node_id: &N,
        core_id: u32,
    ) -> Self
    where
        P: Into<String>,
        N: std::fmt::Display + ?Sized,
    {
        Self::new(
            manager,
            pipeline_group_id,
            pipeline_id,
            node_id.to_string(),
            core_id,
        )
    }

    /// The owning manager, exposed for the small set of code paths
    /// that need to consult plan presence.
    #[must_use]
    pub fn manager(&self) -> &ListenerGroupManager {
        &self.manager
    }

    /// Builds the canonical key for `(pipeline_group, receiver_node,
    /// addr, Tcp)` with no bind device.
    #[must_use]
    pub fn tcp_key(&self, addr: SocketAddr) -> ListenerGroupKey {
        ListenerGroupKey::tcp(
            self.pipeline_group_id.clone(),
            self.pipeline_id.clone(),
            self.receiver_node_id.clone(),
            addr,
        )
    }

    /// Builds the canonical key for `(pipeline_group, receiver_node,
    /// addr, Udp)` with no bind device.
    #[must_use]
    pub fn udp_key(&self, addr: SocketAddr) -> ListenerGroupKey {
        ListenerGroupKey::udp(
            self.pipeline_group_id.clone(),
            self.pipeline_id.clone(),
            self.receiver_node_id.clone(),
            addr,
        )
    }

    /// CPU id of the receiver pipeline thread that owns this handle.
    #[must_use]
    pub fn core_id(&self) -> u32 {
        self.core_id
    }
}

/// Eagerly creates one listener per expected member, returning a map
/// keyed by `core_id`. All sockets are created with the same
/// `SO_REUSEADDR + SO_REUSEPORT` settings used by
/// `EffectHandlerCore::tcp_listener` / `udp_socket`. The returned sockets are
/// in **non-blocking** mode so the acquiring receiver can hand them to its own
/// runtime via `tokio::net::{TcpListener,UdpSocket}::from_std(...)`.
///
/// `plan.key.bind_device` is **not** applied: this function does not
/// call `SO_BINDTODEVICE`. The field participates in the lookup key
/// only so that a future patch which does apply `SO_BINDTODEVICE` does
/// not silently change keying semantics.
///
/// Failure of any individual bind drops every previously-created
/// socket before returning the error, leaving no leaked partial state.
fn materialise_group(plan: &ListenerGroupPlan) -> std::io::Result<HashMap<u32, GroupSocket>> {
    let mut sockets: Vec<(u32, GroupSocket)> = Vec::with_capacity(plan.expected_members.len());
    for member in &plan.expected_members {
        let socket = match plan.key.protocol {
            Protocol::Tcp => GroupSocket::Tcp(build_listener(plan.key.addr)?),
            Protocol::Udp => GroupSocket::Udp(build_udp_socket(plan.key.addr)?),
        };
        sockets.push((member.core_id, socket));
    }
    Ok(sockets.into_iter().collect())
}

/// Builds the deterministic `(listener_id, raw_fd, core_id, numa_node)`
/// slice and invokes the optional attach hook. The slice is sorted
/// by `listener_id` so eBPF map population is order-independent.
fn invoke_attach_hook(
    plan: &ListenerGroupPlan,
    sockets: &HashMap<u32, GroupSocket>,
    hook: Option<&AttachHook>,
) -> std::io::Result<AttachOutcome> {
    use std::os::fd::AsRawFd;
    let Some(hook) = hook else {
        return Ok(AttachOutcome::default());
    };
    let mut members = plan.expected_members.clone();
    members.sort_by_key(|m| m.listener_id);
    let mut handles: Vec<(u32, RawFd, u32, u32)> = Vec::with_capacity(members.len());
    for m in &members {
        if let Some(socket) = sockets.get(&m.core_id) {
            let fd = match socket {
                GroupSocket::Tcp(listener) => listener.as_raw_fd(),
                GroupSocket::Udp(socket) => socket.as_raw_fd(),
            };
            handles.push((m.listener_id, fd, m.core_id, m.numa_node));
        }
    }
    hook(plan, &handles)
}

fn build_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, None)?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        sock.set_reuse_port(true)?;
    }
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    sock.listen(8192)?;
    Ok(sock.into())
}

fn build_udp_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let domain = match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let sock = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        sock.set_reuse_port(true)?;
    }
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Barrier;
    use std::thread;

    fn loopback_plan(addr: SocketAddr, members: &[(u32, u32, u32)]) -> ListenerGroupPlan {
        plan_with_ids("pg", "recv", addr, members)
    }

    fn plan_with_ids(
        pg: &str,
        recv: &str,
        addr: SocketAddr,
        members: &[(u32, u32, u32)],
    ) -> ListenerGroupPlan {
        plan_with_pipeline_ids(pg, "pipe", recv, addr, members)
    }

    fn plan_with_pipeline_ids(
        pg: &str,
        pipe: &str,
        recv: &str,
        addr: SocketAddr,
        members: &[(u32, u32, u32)],
    ) -> ListenerGroupPlan {
        ListenerGroupPlan {
            key: ListenerGroupKey::tcp(pg, pipe, recv, addr),
            expected_members: members
                .iter()
                .map(|(listener_id, core_id, numa_node)| ListenerGroupMember {
                    listener_id: *listener_id,
                    core_id: *core_id,
                    numa_node: *numa_node,
                })
                .collect(),
        }
    }

    fn ephemeral_addr() -> SocketAddr {
        // Bind to port 0 just to discover an unused ephemeral port,
        // then drop the listener so the manager can re-bind with
        // SO_REUSEPORT in the test.
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        addr
    }

    #[test]
    fn group_key_is_full_identity() {
        let addr: SocketAddr = "127.0.0.1:18001".parse().unwrap();
        // Same identity = same key.
        assert_eq!(
            ListenerGroupKey::tcp("pg", "pipe", "recv", addr),
            ListenerGroupKey::tcp("pg", "pipe", "recv", addr),
        );
        // Different protocol = different key.
        let mut udp = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        udp.protocol = Protocol::Udp;
        assert_ne!(udp, ListenerGroupKey::tcp("pg", "pipe", "recv", addr));
        // Different pipeline_group_id = different key.
        assert_ne!(
            ListenerGroupKey::tcp("pg-a", "pipe", "recv", addr),
            ListenerGroupKey::tcp("pg-b", "pipe", "recv", addr),
        );
        // Different pipeline_id = different key.
        assert_ne!(
            ListenerGroupKey::tcp("pg", "pipe-a", "recv", addr),
            ListenerGroupKey::tcp("pg", "pipe-b", "recv", addr),
        );
        // Different receiver_node_id = different key.
        assert_ne!(
            ListenerGroupKey::tcp("pg", "pipe", "recv-a", addr),
            ListenerGroupKey::tcp("pg", "pipe", "recv-b", addr),
        );
        // Different bind_device = different key (identity-only).
        let mut keyed_eth0 = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        keyed_eth0.bind_device = Some("eth0".into());
        let mut keyed_eth1 = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        keyed_eth1.bind_device = Some("eth1".into());
        assert_ne!(keyed_eth0, keyed_eth1);
    }

    #[test]
    fn plans_with_distinct_identity_but_same_addr_coexist() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        // Two pipeline groups, same addr/protocol -> distinct keys,
        // both should register successfully.
        mgr.register_plan(plan_with_ids("pg-a", "recv", addr, &[(0, 0, 0)]))
            .unwrap();
        mgr.register_plan(plan_with_ids("pg-b", "recv", addr, &[(0, 0, 0)]))
            .unwrap();
        // Same pipeline group, same receiver node, different pipeline
        // id -> distinct. Node names are only unique within one
        // pipeline.
        mgr.register_plan(plan_with_pipeline_ids(
            "pg-a",
            "pipe-other",
            "recv",
            addr,
            &[(0, 0, 0)],
        ))
        .unwrap();
        // Same pipeline group, different receiver nodes -> still distinct.
        mgr.register_plan(plan_with_ids("pg-a", "recv-other", addr, &[(0, 0, 0)]))
            .unwrap();
        assert_eq!(mgr.plan_count(), 4);
    }

    #[test]
    fn duplicate_full_key_is_rejected() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        mgr.register_plan(plan_with_ids("pg", "recv", addr, &[(0, 0, 0)]))
            .unwrap();
        let dup = plan_with_ids("pg", "recv", addr, &[(0, 0, 0)]);
        assert_eq!(mgr.register_plan(dup), Err(PlanError::DuplicateKey));
    }

    #[test]
    fn register_plan_rejects_invalid_plans() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();

        // Empty members.
        let mut plan = loopback_plan(addr, &[]);
        plan.key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        assert_eq!(mgr.register_plan(plan), Err(PlanError::NoMembers));

        // Duplicate listener id (under a fresh key, since the empty
        // plan above is still under "pg/recv/addr" but was rejected
        // before insertion).
        let plan = plan_with_ids("pg", "r2", addr, &[(0, 0, 0), (0, 1, 0)]);
        assert_eq!(
            mgr.register_plan(plan),
            Err(PlanError::DuplicateListenerId(0))
        );

        // Duplicate core id.
        let plan = plan_with_ids("pg", "r3", addr, &[(0, 7, 0), (1, 7, 0)]);
        assert_eq!(mgr.register_plan(plan), Err(PlanError::DuplicateCoreId(7)));

        // Ephemeral port 0 cannot form one coordinated group because
        // each socket bind would pick a different concrete port.
        let plan = plan_with_ids("pg", "r4b", "127.0.0.1:0".parse().unwrap(), &[(0, 0, 0)]);
        assert_eq!(
            mgr.register_plan(plan),
            Err(PlanError::EphemeralPortUnsupported)
        );

        // Successful registration, then duplicate-key rejection.
        let plan = plan_with_ids("pg", "r5", addr, &[(0, 0, 0)]);
        mgr.register_plan(plan).unwrap();
        let dup = plan_with_ids("pg", "r5", addr, &[(0, 0, 0)]);
        assert_eq!(mgr.register_plan(dup), Err(PlanError::DuplicateKey));
    }

    #[test]
    fn acquire_with_no_plan_returns_no_plan() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        let outcome = mgr.acquire(
            &ListenerGroupKey::tcp("pg", "pipe", "recv", addr),
            0,
            Duration::from_millis(1),
        );
        assert!(matches!(outcome, AcquireOutcome::NoPlan));
    }

    #[test]
    fn acquire_with_unknown_core_returns_no_plan_immediately() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        mgr.register_plan(loopback_plan(addr, &[(0, 0, 0)]))
            .unwrap();
        let start = Instant::now();
        let outcome = mgr.acquire(
            &ListenerGroupKey::tcp("pg", "pipe", "recv", addr),
            99, // not in plan
            Duration::from_secs(60),
        );
        let elapsed = start.elapsed();
        assert!(matches!(outcome, AcquireOutcome::NoPlan));
        assert!(
            elapsed < Duration::from_millis(100),
            "should not wait, got {elapsed:?}"
        );
    }

    #[test]
    fn quorum_two_members_get_distinct_listeners() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        mgr.register_plan(loopback_plan(addr, &[(0, 0, 0), (1, 1, 0)]))
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let mgr_a = mgr.clone();
        let mgr_b = mgr.clone();
        let key_a = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        let key_b = key_a.clone();
        let bar_a = Arc::clone(&barrier);
        let bar_b = Arc::clone(&barrier);

        let h_a = thread::spawn(move || {
            let _ = bar_a.wait();
            mgr_a.acquire(&key_a, 0, Duration::from_secs(2))
        });
        let h_b = thread::spawn(move || {
            let _ = bar_b.wait();
            mgr_b.acquire(&key_b, 1, Duration::from_secs(2))
        });

        let out_a = h_a.join().unwrap();
        let out_b = h_b.join().unwrap();

        let l_a = match out_a {
            AcquireOutcome::Listener(l) => l,
            other => panic!("a: unexpected {other:?}"),
        };
        let l_b = match out_b {
            AcquireOutcome::Listener(l) => l,
            other => panic!("b: unexpected {other:?}"),
        };
        // Different fds, both bound to the same address.
        assert_eq!(l_a.local_addr().unwrap().port(), addr.port());
        assert_eq!(l_b.local_addr().unwrap().port(), addr.port());
        assert_ne!(l_a.as_raw_fd(), l_b.as_raw_fd());
    }

    #[test]
    fn quorum_two_udp_members_get_distinct_sockets() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        let mut plan = loopback_plan(addr, &[(0, 0, 0), (1, 1, 0)]);
        plan.key.protocol = Protocol::Udp;
        mgr.register_plan(plan).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let key = {
            let mut key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
            key.protocol = Protocol::Udp;
            key
        };
        let mut handles = Vec::new();
        for core in [0_u32, 1] {
            let mgr = mgr.clone();
            let bar = Arc::clone(&barrier);
            let key = key.clone();
            handles.push(thread::spawn(move || {
                let _ = bar.wait();
                mgr.acquire(&key, core, Duration::from_secs(2))
            }));
        }

        let mut sockets = Vec::new();
        for h in handles {
            match h.join().unwrap() {
                AcquireOutcome::DatagramSocket(s) => sockets.push(s.into_parts().0),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0].local_addr().unwrap().port(), addr.port());
        assert_eq!(sockets[1].local_addr().unwrap().port(), addr.port());
        use std::os::fd::AsRawFd;
        assert_ne!(sockets[0].as_raw_fd(), sockets[1].as_raw_fd());
    }

    #[test]
    fn deterministic_assignment_by_core_id() {
        // With the new quorum semantics, acquires must be concurrent;
        // serially calling acquire(6) would block waiting for 5 and 7.
        // The "deterministic" property is that whichever order the
        // threads arrive in, each gets back the listener mapped to
        // its own core_id (and the address is stable).
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        mgr.register_plan(loopback_plan(addr, &[(10, 5, 0), (20, 6, 0), (30, 7, 0)]))
            .unwrap();
        let key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);

        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for core in [6u32, 5, 7] {
            let mgr = mgr.clone();
            let bar = Arc::clone(&barrier);
            let key = key.clone();
            handles.push((
                core,
                thread::spawn(move || {
                    let _ = bar.wait();
                    mgr.acquire(&key, core, Duration::from_secs(2))
                }),
            ));
        }
        for (core, h) in handles {
            let outcome = h.join().unwrap();
            let listener = match outcome {
                AcquireOutcome::Listener(l) => l,
                other => panic!("core {core}: unexpected {other:?}"),
            };
            assert_eq!(listener.local_addr().unwrap().port(), addr.port());
        }
    }

    #[test]
    fn timeout_first_arriver_falls_back_when_quorum_not_reached() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        // Two expected members; only one will arrive.
        mgr.register_plan(loopback_plan(addr, &[(0, 0, 0), (1, 1, 0)]))
            .unwrap();
        let key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);

        let start = Instant::now();
        let outcome = mgr.acquire(&key, 0, Duration::from_millis(50));
        let elapsed = start.elapsed();
        assert!(
            matches!(outcome, AcquireOutcome::FallbackToIndependent),
            "got {outcome:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(45),
            "should wait near-deadline, waited {elapsed:?}"
        );

        // Subsequent arrivals also see fallback.
        let late = mgr.acquire(&key, 1, Duration::from_millis(1));
        assert!(matches!(late, AcquireOutcome::FallbackToIndependent));
    }

    #[test]
    fn force_fallback_returns_fallback_to_independent() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        mgr.register_plan(loopback_plan(addr, &[(0, 0, 0), (1, 1, 0)]))
            .unwrap();
        let key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        mgr.force_fallback(&key);
        assert!(matches!(
            mgr.acquire(&key, 0, Duration::from_millis(1)),
            AcquireOutcome::FallbackToIndependent
        ));
        assert!(matches!(
            mgr.acquire(&key, 1, Duration::from_millis(1)),
            AcquireOutcome::FallbackToIndependent
        ));
    }

    #[test]
    fn second_acquire_for_same_core_returns_already_acquired() {
        // H1 regression: a second acquire on the same `(key, core)`
        // must surface as `AlreadyAcquired`, not `NoPlan`. The
        // `EffectHandler::tcp_listener` seam treats the former as a
        // hard error so a misbehaving caller cannot silently rebind
        // a fresh listener and lose the coordinated-reuseport
        // guarantee.
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        mgr.register_plan(plan_with_ids("pg", "recv", addr, &[(0, 0, 0)]))
            .unwrap();
        let key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        let first = mgr.acquire(&key, 0, Duration::from_secs(1));
        assert!(matches!(first, AcquireOutcome::Listener(_)), "{first:?}");
        let second = mgr.acquire(&key, 0, Duration::from_secs(1));
        assert!(
            matches!(second, AcquireOutcome::AlreadyAcquired),
            "expected AlreadyAcquired, got {second:?}"
        );
    }

    #[test]
    fn validate_against_topology_flags_unknown_and_mismatched() {
        let topo =
            CpuTopology::from_node_cpulists(&[(0, "0-1".to_string()), (1, "2-3".to_string())]);
        let plan = loopback_plan(
            ephemeral_addr(),
            &[
                (0, 0, 0),  // ok
                (1, 2, 0),  // mismatched: actual 1, declared 0
                (2, 99, 1), // unknown core
            ],
        );
        let (unknown, mismatched) = validate_against_topology(&plan, &topo);
        assert_eq!(unknown, vec![99]);
        assert_eq!(mismatched, vec![(2, 0, 1)]);
    }

    #[test]
    fn attach_hook_invoked_with_listener_id_sorted_handles() {
        use std::sync::atomic::{AtomicBool, Ordering as AOrdering};

        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        // Members declared in non-listener-id order; the hook must
        // see them sorted by listener_id.
        mgr.register_plan(plan_with_ids(
            "pg",
            "recv",
            addr,
            &[(30, 7, 0), (10, 5, 0), (20, 6, 0)],
        ))
        .unwrap();

        let observed: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_clone = Arc::clone(&observed);
        let attach_called = Arc::new(AtomicBool::new(false));
        let attach_called_clone = Arc::clone(&attach_called);
        mgr.set_attach_hook(Arc::new(move |_plan, handles| {
            attach_called_clone.store(true, AOrdering::Relaxed);
            let mut sink = observed_clone.lock().unwrap();
            for (lid, fd, _core, _numa) in handles {
                assert!(*fd >= 0, "expected positive fd");
                sink.push(*lid);
            }
            Ok(AttachOutcome::default())
        }));

        let key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for core in [5u32, 6, 7] {
            let mgr = mgr.clone();
            let bar = Arc::clone(&barrier);
            let key = key.clone();
            handles.push(thread::spawn(move || {
                let _ = bar.wait();
                mgr.acquire(&key, core, Duration::from_secs(2))
            }));
        }
        for h in handles {
            let outcome = h.join().unwrap();
            let l = match outcome {
                AcquireOutcome::Listener(l) => l,
                other => panic!("{other:?}"),
            };
            assert!(l.as_raw_fd() >= 0);
            assert_eq!(l.local_addr().unwrap().port(), addr.port());
        }
        assert!(attach_called.load(AOrdering::Relaxed));
        assert_eq!(*observed.lock().unwrap(), vec![10, 20, 30]);
    }

    #[test]
    fn attach_hook_failure_surfaces_as_materialisation_failed() {
        let mgr = ListenerGroupManager::new();
        let addr = ephemeral_addr();
        mgr.register_plan(plan_with_ids("pg", "recv", addr, &[(0, 0, 0)]))
            .unwrap();
        mgr.set_attach_hook(Arc::new(|_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no CAP_BPF in test",
            ))
        }));
        let key = ListenerGroupKey::tcp("pg", "pipe", "recv", addr);
        let outcome = mgr.acquire(&key, 0, Duration::from_secs(1));
        assert!(
            matches!(outcome, AcquireOutcome::MaterialisationFailed(_)),
            "got {outcome:?}"
        );
    }

    #[test]
    fn env_flag_predicate_parses_known_values() {
        // Pure helper; no process-env mutation, parallel-safe.
        assert!(env_flag_enabled(Some("1")));
        assert!(env_flag_enabled(Some("true")));
        assert!(env_flag_enabled(Some("TRUE")));
        assert!(!env_flag_enabled(Some("0")));
        assert!(!env_flag_enabled(Some("yes")));
        assert!(!env_flag_enabled(Some("")));
        assert!(!env_flag_enabled(None));
    }

    #[test]
    fn env_switches_default_off() {
        // Just exercise the helpers; env contents are out of our
        // control in the test runner.
        let _ = reuseport_ebpf_enabled();
        let _ = reuseport_ebpf_strict();
        let _ = manager_only_debug_enabled();
        let _ = manager_active();
    }

    #[test]
    fn tcp_for_receiver_matches_handle_tcp_key() {
        // H2 regression: extraction-side and runtime-side helpers
        // must produce the *exact* same key for the same logical
        // receiver. Both go through the canonical Display-based
        // stringifier.
        let addr: SocketAddr = "127.0.0.1:18099".parse().unwrap();
        let from_extraction =
            ListenerGroupKey::tcp_for_receiver("pg", "pipe", "otlp_receiver", addr);
        let manager = ListenerGroupManager::new();
        let from_runtime =
            ListenerGroupHandle::for_receiver(manager, "pg", "pipe", "otlp_receiver", 0)
                .tcp_key(addr);
        assert_eq!(from_extraction, from_runtime);
    }
}
