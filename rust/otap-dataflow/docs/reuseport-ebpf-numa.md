# NUMA-Local Reuseport: Design and Phases

This document describes the experimental NUMA-local reuseport stack in the
engine. It is split across three phases. Phase 1 (NUMA topology discovery)
and Phase 2 (coordinated listener manager scaffolding) are implemented in
the engine today. Phase 3 (eBPF selector attach) ships as an opt-in
prototype loader, but no production code path enables it yet.

## Phases

### Phase 1 -- NUMA topology discovery (implemented)

`crates/engine/src/topology.rs` parses
`/sys/devices/system/node/node*/cpulist` once at controller startup and
caches a `Vec<Option<u32>>` keyed by CPU id, value = NUMA node id. It uses
no `libnuma`/`hwloc` dependency. Non-Linux hosts and unreadable sysfs
degrade safely to an empty mapping; lookups for unknown CPUs return
`None`, and callers that need a concrete value (e.g. the
`engine.numa_node_id` telemetry attribute) fall back to node `0` to
preserve pre-Phase-1 behaviour.

The topology is owned by `ControllerContext` and exposed via
`ControllerContext::topology()`. `PipelineContext` looks up
`numa_node_or_zero(core_id)` from this topology when assembling the
engine attribute set, replacing the previous hard-coded `numa_node_id: 0`
placeholder.

### Phase 2 -- Coordinated listener manager (engine scaffolding)

`crates/engine/src/listener_group/` defines:

- `Protocol::{Tcp, Udp}`
- `ListenerGroupKey { pipeline_group_id, receiver_node_id, addr,
  protocol, bind_device }` -- the **full identity** of a planned
  group. Keying on the complete tuple avoids collisions when two
  unrelated receiver groups in different pipelines or nodes happen to
  share the same bind address. `bind_device` participates in the key
  as identity only; the current materialisation path does **not**
  apply `SO_BINDTODEVICE`.
- `ListenerGroupMember { listener_id, core_id, numa_node }`
- `ListenerGroupPlan { key, expected_members }`
- `ListenerGroupManager` (held in `ControllerContext`)

Lifecycle:

1. The controller (or pipeline-assembly layer) calls
   `manager.register_plan(plan)` for each receiver bind address it
   intends to share across pinned cores.
2. Each receiver pipeline calls `manager.acquire(key, core_id, timeout)`
   during startup. Acquires block on a per-group condition variable
   until the *last* expected member arrives.
3. On the last arrival, the manager eagerly creates one
   `std::net::TcpListener` per expected member with the same
   `SO_REUSEADDR + SO_REUSEPORT` settings as
   `EffectHandlerCore::tcp_listener`. Sockets are built into a local
   `Vec` first and only swapped into the group state when all N have
   succeeded; partial materialisation is dropped to avoid leaking
   half-bound ports.
4. Each acquirer wakes up and receives the `TcpListener` mapped to its
   own `core_id`. The acquiring receiver converts to a Tokio listener
   on its own current-thread runtime via
   `tokio::net::TcpListener::from_std(...)` -- this avoids
   cross-runtime reactor-affinity issues.
5. If quorum is not reached within `timeout`, the group transitions to
   `Fallback` and every acquirer (current and future) receives
   `AcquireOutcome::FallbackToIndependent`. The receiver should then
   bind a listener independently using today's path so startup never
   deadlocks.

Materialisation `io::Error`s (e.g. address already bound by an
unrelated process without `SO_REUSEPORT`) are surfaced as a hard
`AcquireOutcome::MaterialisationFailed` to the first acquirer and
replayed for any later acquirer, matching the existing
`EffectHandler::tcp_listener` failure semantics.

The manager never stores long-lived `RawFd`s. Listeners are owned by the
manager only between materialisation and acquire; once distributed they
are owned by the receiving pipeline.

`crates/engine/src/listener_group/metrics.rs` defines a
`ListenerGroupMetrics` set with `plans_registered`, `groups_ready`,
`fallback_timeout`, and `materialisation_failed` counters, plus a
`ListenerGroupAttributeSet`. **These metric types are prepared but
not yet emitted**: nothing in `ListenerGroupManager` increments them.
Counter wiring is deferred to the same Phase 2.5 patch that integrates
the manager into `EffectHandler::tcp_listener`, so the counters reflect
a real production code path.

### Phase 2.5 -- Production wiring (implemented)

`EffectHandler::tcp_listener(addr)` consults the manager when:

1. `OTAP_DF_REUSEPORT_EBPF=1` is set in the engine process
   environment (the **single user-facing switch** -- see
   "Environment variables" below), **and**
2. the controller has registered a [`ListenerGroupPlan`] covering
   `(pipeline_group_id, receiver_node_id, addr, Tcp)`.

If either condition is unmet, `tcp_listener` falls through to the
existing per-receiver bind path with byte-identical behaviour. With
the env var unset, the code path is dead. The handle is plumbed by:

- `EffectHandlerCore` carrying an optional `ListenerGroupHandle`
  (`pipeline_group_id`, `receiver_node_id`, `core_id`, manager Arc),
- `ReceiverWrapper::start(...)` taking the handle as a parameter,
- `runtime_pipeline.rs` constructing it from `PipelineContext` for
  every receiver before starting the pipeline thread, via
  `ListenerGroupHandle::for_receiver(...)`.

Plans are populated by a small conservative helper at
`engine::listener_group::extraction::extract_plans_for_pipeline(...)`.
The controller calls it for every pipeline, before launching its
threads, and registers one plan per recognised receiver URN
(`urn:otel:receiver:{otlp,otap,syslog_cef}`). The helper reads the
node config as a raw `serde_json::Value`, extracting
`listening_addr` either at the top level or under `grpc.listening_addr`
/ `http.listening_addr` so the controller does not need to depend on
receiver-specific typed configs.

`listening_addr` values must be a literal `IP:port` (`SocketAddr`
parseable string, e.g. `127.0.0.1:4318` or `[::1]:4318`). Hostnames
are **not** resolved by the extractor; resolve them in your
deployment config before launching the engine.

The plan-side and runtime-side keys are constructed via the
single-source helpers `ListenerGroupKey::tcp_for_receiver(...)` and
`ListenerGroupHandle::for_receiver(...)` so the two cannot drift on
stringification of the receiver node identifier.

Listener `core_id`s come from `requested_cores` for the pipeline;
each member's `numa_node` is looked up via
`ControllerContext::topology()` (Phase 1).

The first arriver owns a fixed quorum deadline of
`QUORUM_TIMEOUT = 5s`. Late arrivers wait against the same deadline;
on expiry the group transitions to fallback for everyone.

#### Failure modes

- `acquire` returns `NoPlan` (no plan covers this `(addr, core_id)`):
  the seam falls back to today's independent bind.
- `acquire` returns `FallbackToIndependent` (quorum timeout, default
  5 s): same fallback. The receiver still binds successfully; the
  group has lost its coordinated-reuseport guarantee.
- `acquire` returns `AlreadyAcquired` (caller already took its
  listener): the seam returns a hard `Error::IoError` rather than
  silently rebinding a fresh listener. Receivers must call
  `tcp_listener` at most once per `(addr, core)` per startup.
- `acquire` returns `MaterialisationFailed`: same hard `Error::IoError`
  as today's bind would have produced.

### Phase 3 -- Optional eBPF selector attach (implemented, opt-in)

`OTAP_DF_REUSEPORT_EBPF=1` is the **single user-facing switch** for
the experimental NUMA reuseport stack. Setting it both:

1. activates coordinated listener planning + manager acquire
   (Phase 2.5), and
2. installs the eBPF NUMA selector attach hook on the listener-group
   manager via `ebpf_attach_hook(...)`.

The hook fires exactly once per materialised group, after all sockets
are bound and before any acquirer receives its listener. It calls
`reuseport_ebpf::libbpf::load_default_and_attach(...)` and stores the
returned `ReuseportEbpf` handle in the group state so the kernel-side
attachment lives as long as the manager owns the group.

- On non-Linux or when the `reuseport-ebpf` feature is not compiled
  in, the hook is a no-op that emits a one-shot warning. The env var
  is therefore safe to set unconditionally on any build; it just
  continues with coordinated plain `SO_REUSEPORT`.
- On attach failure, the default behaviour is **log and continue**:
  the group falls back to plain `SO_REUSEPORT` and connections still
  flow. Set `OTAP_DF_REUSEPORT_EBPF_STRICT=1` to fail startup
  instead. `OTAP_DF_REUSEPORT_EBPF_STRICT=1` is meaningful only
  when `OTAP_DF_REUSEPORT_EBPF=1` is also set; it does not enable
  anything by itself.
- `df_engine` does **not** require root for the coordinated reuseport
  path; only the eBPF attach itself needs `CAP_BPF + CAP_NET_ADMIN`
  (or `CAP_SYS_ADMIN` on older kernels). When those capabilities are
  missing the hook surfaces the kernel error and follows the strict /
  non-strict policy above.

### Environment variables

| Variable | Default | Effect |
| --- | --- | --- |
| `OTAP_DF_REUSEPORT_EBPF` | unset | **Single user-facing switch.** When `1`, activates coordinated listener planning + manager acquire end-to-end and installs the eBPF attach hook. On non-Linux / no-feature, the hook is a no-op and the engine continues with coordinated plain `SO_REUSEPORT`. |
| `OTAP_DF_REUSEPORT_EBPF_STRICT` | unset | Modifier. When `1`, eBPF attach failures abort startup. Default is log-and-continue. Meaningful only when `OTAP_DF_REUSEPORT_EBPF=1`. |

Default (env unset): the engine behaves identically to `main`. Each
receiver binds independently in `EffectHandler::tcp_listener`; no
manager acquire, no plan registration, no eBPF attach.

### Note on OTLP / gRPC connection fanout

This stack balances *new TCP connections* across listener
sockets/cores within a NUMA node. It does **not** rebalance HTTP/2
streams multiplexed inside a single long-lived gRPC connection. For
useful balancing of OTLP/gRPC ingest, upstream collectors must open
multiple TCP connections (e.g. via `MaxConnectionAge` or
client-side connection-pool fan-out). Otherwise every RPC from a given
upstream pins to a single receiver core for the connection's lifetime
regardless of what this selector does. Adding upstream connection
fanout is a separate benchmark follow-up, not part of this patch.

## Selection Policy (Phase 3 eBPF)

For each new connection the selector:

1. Reads the packet's current NUMA node via `bpf_get_numa_node_id()`.
2. If that node has at least one listener in `numa_ranges`, picks one socket
   inside the node's contiguous sub-range using a per-NUMA round-robin counter
   in `selection_counters` (a shared `BPF_MAP_TYPE_ARRAY` updated with
   `__sync_fetch_and_add`, modulo range length). The shared counter ensures
   every new connection landing on the same NUMA node advances the same
   counter, so listener selection is globally fair across all RX CPUs rather
   than per-CPU.
3. Otherwise falls back to a global round-robin counter at key
   `MAX_NUMA_NODES`, modulo the total socket count.
4. Calls `bpf_sk_select_reuseport(...)`. The selector always returns
   `SK_PASS`, so any helper failure leaves the kernel free to apply its
   default reuseport hash; the program never drops a connection.

Round-robin replaces the previous `ctx->hash % len` placement because hash
collisions can leave some listeners idle when the working set of connections
is small. Round-robin balances **new TCP connections** across listener
sockets/cores within the local NUMA node -- it does not balance individual
requests, HTTP/2 streams inside an existing connection, or bytes per
connection. Long-lived connections still pin to whichever socket the
selector originally chose. The selector is therefore a good fit for
benchmarks where each upstream opens a similar-weight connection into the
downstream, and for production workloads only when per-connection load is
roughly balanced.

The atomic counter requires BPF ISA v3 (clang `-mcpu=v3`, set by the build
script) and therefore Linux >= 5.12. The `selection_counters` map is left
zero-initialised by the kernel; the loader does not seed it.

## Build and Attach Shape

The Linux build path is feature-gated and inert by default. When
`otap-df-engine` is built on Linux with the `reuseport-ebpf` feature, the crate
build script compiles the selector into `OUT_DIR/reuseport_numa_kern.bpf.o` and
exposes that path to the loader.

1. Provide `vmlinux.h` for the target kernel.
2. Compile `reuseport_numa_kern.c` with clang and libbpf headers into a BPF
   object.
3. Create all listener sockets in one coordinated reuseport group.
4. Populate the Rust `ListenerFd` list with `fd`, `listener_id`, `core_id`, and
   `numa_node`.
5. Call `reuseport_ebpf::libbpf::load_default_and_attach(...)`.

The build script resolves `vmlinux.h` in this order:

1. `OTAP_DF_REUSEPORT_EBPF_VMLINUX_H`, either as a path to `vmlinux.h` or a
   directory containing it.
2. `crates/engine/bpf/vmlinux.h`, if one is checked in for a controlled build
   environment.
3. If `OTAP_DF_REUSEPORT_EBPF_GENERATE_VMLINUX_H=1` is set,
   `bpftool btf dump file /sys/kernel/btf/vmlinux format c`.

Set `CLANG` to override the clang executable used by the build script. Set
`OTAP_DF_REUSEPORT_EBPF_INCLUDE_DIR` when `bpf/bpf_helpers.h` is not available
through the compiler's default include path or `pkg-config --cflags libbpf`.

The loader:

- opens and loads the BPF object with libbpf-rs,
- sets the program type to `SkReuseport`,
- sets the attach type to `SkReuseportSelect`,
- fills `reuseport_socks`, `numa_ranges`, and `total_sockets`
  (the `selection_counters` map starts at zero and is left untouched),
- attaches the selector to the reuseport group using
  `setsockopt(SO_ATTACH_REUSEPORT_EBPF)`.

The returned `ReuseportEbpf` value keeps userspace object and map descriptors
open. The reuseport group also holds a kernel reference to the attached program;
dropping `ReuseportEbpf` closes the userspace descriptors but does not detach
the selector from sockets that remain open.

## Operational Requirements

This prototype requires Linux with `BPF_PROG_TYPE_SK_REUSEPORT` and
`BPF_MAP_TYPE_REUSEPORT_SOCKARRAY` support. The shared atomic round-robin
counter additionally requires BPF ISA v3 (clang `-mcpu=v3`, set by the build
script) and `BPF_ATOMIC_FETCH_ADD`, which raises the kernel floor to Linux
>= 5.12. Loading and attaching require `CAP_BPF` plus `CAP_NET_ADMIN` on
newer kernels, or `CAP_SYS_ADMIN` on older kernels. The `bpftool`
header-generation path also requires kernel BTF data at
`/sys/kernel/btf/vmlinux`.

Build prerequisites for the `reuseport-ebpf` feature are:

- clang >= 12 with BPF backend support (for `-mcpu=v3` and atomic builtins),
- libbpf headers providing `bpf/bpf_helpers.h`,
- `vmlinux.h` for the target kernel,
- bpftool only when using the opt-in BTF header-generation path.

This feature only preserves locality if Linux receives packets on NUMA-local
CPUs. Configure NIC RSS and IRQ affinity so RX queue interrupts run on CPUs from
the intended NUMA node. Then pin `df_engine` workers to the same CPU set.

Useful checks:

```bash
lscpu -e=CPU,NODE
cat /sys/class/net/<iface>/device/numa_node
ethtool -l <iface>
ethtool -x <iface>
cat /proc/interrupts
cat /proc/irq/<irq>/smp_affinity_list
```

## Current Limitation

`EffectHandler::tcp_listener` does not yet consult the listener-group
manager, so until Phase 2.5 wires the seam, every receiver still creates
its listener independently. The Phase 2 scaffolding above documents the
follow-up work needed to close this gap.

## Alternatives

For deployments that do not need NUMA-local placement, simpler primitives
exist and may be a better fit:

- `SO_ATTACH_REUSEPORT_CBPF` -- a tiny classic-BPF program that selects
  a listener by RX-CPU index modulo the listener count is enough to
  give per-CPU connection fairness on a single-NUMA host. It needs no
  BTF, no `vmlinux.h`, no `CAP_BPF`, no kernel-5.12+ floor, and ships
  inline in the binary. Apache Traffic Server uses this exact pattern.
- `SO_INCOMING_CPU` is **not** a safe alternative on most production
  kernels: it is broken between Linux 4.1 and 6.1 and only fixed from
  6.2 onward.

The eBPF selector remains justified when:

- the host is multi-NUMA (the locality half is meaningful), and
- the workload benefits from NUMA-local range selection plus
  per-NUMA round-robin within range plus a global fallback, which
  cBPF cannot express, and
- a future Phase 4 (see below) wants graceful listener migration on
  worker upgrade, which neither cBPF nor `SO_INCOMING_CPU` can express.

Realistic performance gains from locality-aware reuseport are in the
single-digit percent range on representative ingestion workloads (cf.
Apache Traffic Server's published 5--7% wrk2 result with cBPF + RX
queue alignment). This stack does not change that ceiling.

## Future Phase 4 -- `tcp_migrate_req` / `sk_reuseport/migrate`

Linux 5.14+ adds `net.ipv4.tcp_migrate_req` and the
`BPF_SK_REUSEPORT_SELECT_OR_MIGRATE` program type, which together let a
custom selector pick an alive replacement listener when a peer in the
reuseport group is closed. This enables hot-standby / zero-downtime
worker upgrades without dropping in-flight handshakes -- the
operational property that no simpler alternative delivers.

It is intentionally **not** part of the current implementation. The
migrate path has its own design considerations (e.g., the kernel
warning that "migration between listeners with different settings can
crash applications") and should land as a separate phase after Phase
2.5 / Phase 3 are wired and exercised on a real multi-NUMA host.
