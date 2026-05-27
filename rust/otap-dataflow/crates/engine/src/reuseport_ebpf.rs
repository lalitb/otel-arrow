// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Experimental NUMA-local `SO_REUSEPORT` eBPF support.
//!
//! This module is intentionally small and disabled by default. It contains the
//! userspace policy model shared with the C eBPF program in
//! `bpf/reuseport_numa_kern.c`, plus a Linux-only libbpf-rs loader behind the
//! `reuseport-ebpf` feature.
//!
//! The eBPF program only affects new socket selection inside a reuseport group.
//! It does not migrate established TCP connections.

/// Maximum number of sockets supported by the initial eBPF policy.
pub const MAX_REUSEPORT_SOCKS: usize = 256;

/// Maximum number of NUMA nodes supported by the initial eBPF policy.
pub const MAX_NUMA_NODES: usize = 64;

/// Name of the BPF reuseport socket array map.
pub const SOCKARRAY_MAP: &str = "reuseport_socks";

/// Name of the BPF NUMA range map.
pub const NUMA_RANGES_MAP: &str = "numa_ranges";

/// Name of the BPF total-socket map.
pub const TOTAL_SOCKETS_MAP: &str = "total_sockets";

/// Name of the BPF round-robin selection-counter map.
///
/// Keys `0..MAX_NUMA_NODES` are the per-NUMA counters, key `MAX_NUMA_NODES`
/// is the global fallback counter. Implemented as a shared
/// `BPF_MAP_TYPE_ARRAY` updated via `__sync_fetch_and_add` so every new
/// connection landing on the same NUMA node advances the same counter and
/// rotates across that node's listeners. The loader does not seed it because
/// BPF array entries are zero-initialised by the kernel.
pub const SELECTION_COUNTERS_MAP: &str = "selection_counters";

/// Name of the BPF selector program.
pub const SELECTOR_PROGRAM: &str = "select_numa_reuseport";

/// Listener metadata needed to build the NUMA-local socket map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListenerSocket {
    /// Original listener position in the engine/controller plan.
    pub listener_id: u32,
    /// CPU core that owns the listener's pipeline thread.
    pub core_id: u32,
    /// NUMA node for `core_id`.
    pub numa_node: u32,
}

/// Compact BPF map placement for one listener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketPlacement {
    /// Key used in `BPF_MAP_TYPE_REUSEPORT_SOCKARRAY`.
    pub map_index: u32,
    /// Original listener position in the engine/controller plan.
    pub listener_id: u32,
    /// CPU core that owns the listener's pipeline thread.
    pub core_id: u32,
    /// NUMA node for `core_id`.
    pub numa_node: u32,
}

/// Inclusive start and length of socket-array entries for one NUMA node.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NumaRange {
    /// First socket-array map key for this NUMA node.
    pub start: u32,
    /// Number of socket-array entries for this NUMA node.
    pub len: u32,
}

/// Compact layout consumed by the userspace loader and mirrored by the BPF maps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NumaReuseportLayout {
    /// Per-listener map placement.
    pub placements: Vec<SocketPlacement>,
    /// Per-NUMA contiguous socket ranges.
    pub ranges: Vec<NumaRange>,
}

/// Builds a compact socket-array layout grouped by NUMA node.
///
/// The eBPF program can select a socket with one range lookup and one modulo
/// operation when all sockets for a NUMA node are contiguous in the sockarray
/// map. This function preserves input order within each NUMA node.
///
/// # Errors
///
/// Returns an error when the layout exceeds the fixed limits used by the BPF
/// object.
pub fn build_numa_layout(
    sockets: &[ListenerSocket],
) -> Result<NumaReuseportLayout, NumaLayoutError> {
    if sockets.len() > MAX_REUSEPORT_SOCKS {
        return Err(NumaLayoutError::TooManySockets {
            count: sockets.len(),
            max: MAX_REUSEPORT_SOCKS,
        });
    }

    let mut by_numa: Vec<(u32, Vec<ListenerSocket>)> = Vec::new();
    for socket in sockets {
        if socket.numa_node as usize >= MAX_NUMA_NODES {
            return Err(NumaLayoutError::NumaNodeOutOfRange {
                numa_node: socket.numa_node,
                max: MAX_NUMA_NODES,
            });
        }

        match by_numa
            .iter_mut()
            .find(|(numa_node, _)| *numa_node == socket.numa_node)
        {
            Some((_, entries)) => entries.push(*socket),
            None => by_numa.push((socket.numa_node, vec![*socket])),
        }
    }
    by_numa.sort_by_key(|(numa_node, _)| *numa_node);

    let mut placements = Vec::with_capacity(sockets.len());
    let mut ranges = vec![NumaRange::default(); MAX_NUMA_NODES];
    for (numa_node, entries) in by_numa {
        let start = u32::try_from(placements.len()).expect("socket count fits in u32");
        let len = u32::try_from(entries.len()).expect("socket count fits in u32");
        ranges[numa_node as usize] = NumaRange { start, len };

        for socket in entries {
            let map_index = u32::try_from(placements.len()).expect("socket count fits in u32");
            placements.push(SocketPlacement {
                map_index,
                listener_id: socket.listener_id,
                core_id: socket.core_id,
                numa_node: socket.numa_node,
            });
        }
    }

    Ok(NumaReuseportLayout { placements, ranges })
}

/// Layout construction errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NumaLayoutError {
    /// More sockets were provided than the BPF object supports.
    TooManySockets {
        /// Number of sockets requested.
        count: usize,
        /// Maximum number of supported sockets.
        max: usize,
    },
    /// NUMA node id cannot be represented in the fixed BPF range map.
    NumaNodeOutOfRange {
        /// NUMA node id from userspace topology discovery.
        numa_node: u32,
        /// Maximum number of supported NUMA nodes.
        max: usize,
    },
}

impl std::fmt::Display for NumaLayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManySockets { count, max } => {
                write!(f, "too many reuseport sockets: {count} > {max}")
            }
            Self::NumaNodeOutOfRange { numa_node, max } => {
                write!(
                    f,
                    "NUMA node {numa_node} is outside supported range 0..{max}"
                )
            }
        }
    }
}

impl std::error::Error for NumaLayoutError {}

#[cfg(all(target_os = "linux", feature = "reuseport-ebpf"))]
pub mod libbpf {
    //! Linux userspace loader for the NUMA-local reuseport selector.

    use super::{
        ListenerSocket, NUMA_RANGES_MAP, SELECTOR_PROGRAM, SOCKARRAY_MAP, TOTAL_SOCKETS_MAP,
        build_numa_layout,
    };
    use libbpf_rs::{MapCore, MapFlags, Object, ObjectBuilder, ProgramAttachType, ProgramType};
    use std::collections::HashSet;
    use std::error::Error;
    use std::mem::size_of_val;
    use std::os::fd::{AsFd, AsRawFd, RawFd};
    use std::path::Path;
    use std::sync::Arc;

    /// Listener socket plus NUMA/core metadata used to populate the BPF maps.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ListenerFd {
        /// Raw file descriptor for a listening socket in the reuseport group.
        pub fd: RawFd,
        /// Original listener position in the engine/controller plan.
        pub listener_id: u32,
        /// CPU core that owns the listener's pipeline thread.
        pub core_id: u32,
        /// NUMA node for `core_id`.
        pub numa_node: u32,
    }

    /// Loaded and attached reuseport eBPF selector.
    ///
    /// Keeps the userspace BPF object and map file descriptors open.
    ///
    /// The reuseport group holds its own reference to the attached program.
    /// Dropping this value closes the userspace object/map descriptors, but it
    /// does not detach the selector from sockets that remain open.
    #[derive(Debug)]
    pub struct ReuseportEbpf {
        _object: Object,
        sockarray_map_fd: RawFd,
        listener_map_indices: Vec<(u32, u32)>,
    }

    impl ReuseportEbpf {
        /// Loads the build-script compiled BPF object and attaches it to a reuseport group.
        ///
        /// # Errors
        ///
        /// Returns an error when the feature was built without an object path,
        /// or when loading or attaching the BPF object fails.
        pub fn load_default_and_attach(
            listeners: &[ListenerFd],
            debug: bool,
        ) -> Result<Self, Box<dyn Error + Send + Sync>> {
            load_default_and_attach(listeners, debug)
        }

        /// Loads the provided BPF object and attaches it to a reuseport group.
        ///
        /// # Errors
        ///
        /// Returns an error when the BPF object cannot be opened/loaded, when a
        /// required map/program is missing, when map updates fail, or when
        /// `setsockopt(SO_ATTACH_REUSEPORT_EBPF)` fails.
        pub fn load_and_attach<P: AsRef<Path>>(
            object_path: P,
            listeners: &[ListenerFd],
            debug: bool,
        ) -> Result<Self, Box<dyn Error + Send + Sync>> {
            load_and_attach(object_path, listeners, debug)
        }

        /// Builds per-listener cleanup guards for sockarray entries.
        ///
        /// Dropping a guard deletes that listener's sockarray entry. The
        /// delete is best-effort because process shutdown may close BPF
        /// descriptors before every receiver guard is dropped.
        #[must_use]
        pub fn listener_leases(&self) -> Vec<(u32, Arc<dyn std::any::Any + Send + Sync>)> {
            self.listener_map_indices
                .iter()
                .map(|(listener_id, map_index)| {
                    let lease: Arc<dyn std::any::Any + Send + Sync> =
                        Arc::new(SockarrayEntryLease {
                            map_fd: self.sockarray_map_fd,
                            map_index: *map_index,
                        });
                    (*listener_id, lease)
                })
                .collect()
        }
    }

    /// Path to the BPF object compiled by this crate's build script.
    #[must_use]
    pub fn default_object_path() -> Option<&'static Path> {
        option_env!("OTAP_DF_REUSEPORT_EBPF_OBJECT").map(Path::new)
    }

    /// Loads the build-script compiled BPF object and attaches it to a reuseport group.
    ///
    /// # Errors
    ///
    /// Returns an error when the feature was built without an object path, or
    /// when [`load_and_attach`] fails.
    pub fn load_default_and_attach(
        listeners: &[ListenerFd],
        debug: bool,
    ) -> Result<ReuseportEbpf, Box<dyn Error + Send + Sync>> {
        let object_path = default_object_path().ok_or(
            "reuseport eBPF object path is unavailable; build on Linux with the `reuseport-ebpf` feature enabled",
        )?;
        load_and_attach(object_path, listeners, debug)
    }

    /// Loads the BPF object, populates maps, and attaches it to a reuseport group.
    ///
    /// # Errors
    ///
    /// Returns an error when the BPF object cannot be opened/loaded, when a
    /// required map/program is missing, when map updates fail, or when
    /// `setsockopt(SO_ATTACH_REUSEPORT_EBPF)` fails.
    pub fn load_and_attach<P: AsRef<Path>>(
        object_path: P,
        listeners: &[ListenerFd],
        debug: bool,
    ) -> Result<ReuseportEbpf, Box<dyn Error + Send + Sync>> {
        validate_listeners(listeners)?;
        let metadata: Vec<_> = listeners
            .iter()
            .map(|listener| ListenerSocket {
                listener_id: listener.listener_id,
                core_id: listener.core_id,
                numa_node: listener.numa_node,
            })
            .collect();
        let layout = build_numa_layout(&metadata)?;

        let mut open_object = ObjectBuilder::default()
            .debug(debug)
            .open_file(object_path.as_ref())?;

        for mut program in open_object.progs_mut() {
            if program.name().to_string_lossy() == SELECTOR_PROGRAM {
                program.set_prog_type(ProgramType::SkReuseport);
                program.set_attach_type(ProgramAttachType::SkReuseportSelect);
            }
        }

        let mut object = open_object.load()?;
        update_total_sockets(&mut object, listeners.len())?;
        update_numa_ranges(&mut object, &layout.ranges)?;
        // Populate maps before attach so the first packet handled by the
        // selector sees a complete socket array.
        update_sockarray(&mut object, &layout.placements, listeners)?;
        attach_selector(&object, listeners)?;
        let sockarray_map_fd = find_map_mut(&mut object, SOCKARRAY_MAP)?
            .as_fd()
            .as_raw_fd();
        let listener_map_indices = layout
            .placements
            .iter()
            .map(|placement| (placement.listener_id, placement.map_index))
            .collect();

        Ok(ReuseportEbpf {
            _object: object,
            sockarray_map_fd,
            listener_map_indices,
        })
    }

    #[derive(Debug)]
    struct SockarrayEntryLease {
        map_fd: RawFd,
        map_index: u32,
    }

    impl Drop for SockarrayEntryLease {
        fn drop(&mut self) {
            let key = self.map_index.to_ne_bytes();
            let _ = bpf_map_delete_elem(self.map_fd, key.as_ptr().cast());
        }
    }

    #[repr(C)]
    struct BpfMapElemAttr {
        map_fd: u32,
        _pad: u32,
        key: u64,
        value: u64,
        flags: u64,
    }

    #[allow(unsafe_code)]
    fn bpf_map_delete_elem(map_fd: RawFd, key: *const libc::c_void) -> std::io::Result<()> {
        const BPF_MAP_DELETE_ELEM: libc::c_uint = 3;
        let attr = BpfMapElemAttr {
            map_fd: map_fd as u32,
            _pad: 0,
            key: key as u64,
            value: 0,
            flags: 0,
        };
        // SAFETY: `attr` follows the leading layout of `union bpf_attr`
        // for BPF_MAP_DELETE_ELEM, and `key` points to the map key bytes
        // for the duration of the syscall.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_bpf,
                BPF_MAP_DELETE_ELEM,
                std::ptr::from_ref(&attr),
                size_of_val(&attr),
            )
        };
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn validate_listeners(listeners: &[ListenerFd]) -> Result<(), Box<dyn Error + Send + Sync>> {
        if listeners.is_empty() {
            return Err("at least one listener is required to attach reuseport eBPF".into());
        }

        let mut listener_ids = HashSet::with_capacity(listeners.len());
        let mut fds = HashSet::with_capacity(listeners.len());
        for listener in listeners {
            if listener.fd < 0 {
                return Err(format!(
                    "listener_id={} has invalid negative fd {}",
                    listener.listener_id, listener.fd
                )
                .into());
            }
            if !listener_ids.insert(listener.listener_id) {
                return Err(format!("duplicate listener_id={}", listener.listener_id).into());
            }
            if !fds.insert(listener.fd) {
                return Err(format!("duplicate listener fd={}", listener.fd).into());
            }
        }

        Ok(())
    }

    fn update_total_sockets(
        object: &mut Object,
        total: usize,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let key = 0_u32.to_ne_bytes();
        let value = u32::try_from(total)?.to_ne_bytes();
        let map = find_map_mut(object, TOTAL_SOCKETS_MAP)?;
        map.update(&key, &value, MapFlags::ANY)?;
        Ok(())
    }

    fn update_numa_ranges(
        object: &mut Object,
        ranges: &[super::NumaRange],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let map = find_map_mut(object, NUMA_RANGES_MAP)?;
        for (idx, range) in ranges.iter().enumerate() {
            let key = u32::try_from(idx)?.to_ne_bytes();
            let mut value = [0_u8; 8];
            value[..4].copy_from_slice(&range.start.to_ne_bytes());
            value[4..].copy_from_slice(&range.len.to_ne_bytes());
            map.update(&key, &value, MapFlags::ANY)?;
        }
        Ok(())
    }

    fn update_sockarray(
        object: &mut Object,
        placements: &[super::SocketPlacement],
        listeners: &[ListenerFd],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let map = find_map_mut(object, SOCKARRAY_MAP)?;
        for placement in placements {
            let listener = listeners
                .iter()
                .find(|listener| listener.listener_id == placement.listener_id)
                .ok_or_else(|| {
                    format!(
                        "missing listener fd for listener_id={}",
                        placement.listener_id
                    )
                })?;
            let key = placement.map_index.to_ne_bytes();
            let value = listener.fd.to_ne_bytes();
            map.update(&key, &value, MapFlags::ANY)?;
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn attach_selector(
        object: &Object,
        listeners: &[ListenerFd],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let first = listeners
            .first()
            .ok_or("at least one listener is required to attach reuseport eBPF")?;
        let program = object
            .progs()
            .find(|program| program.name().to_string_lossy() == SELECTOR_PROGRAM)
            .ok_or_else(|| format!("missing BPF program `{SELECTOR_PROGRAM}`"))?;
        let program_fd = program.as_fd().as_raw_fd();
        // SAFETY: `first.fd` is a caller-provided listener fd already validated
        // as non-negative, `program_fd` is borrowed from the loaded BPF object,
        // and the pointer/length pair references `program_fd` for this call.
        let rc = unsafe {
            libc::setsockopt(
                first.fd,
                libc::SOL_SOCKET,
                libc::SO_ATTACH_REUSEPORT_EBPF,
                std::ptr::from_ref(&program_fd).cast(),
                size_of_val(&program_fd) as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn find_map_mut<'a>(
        object: &'a mut Object,
        name: &str,
    ) -> Result<libbpf_rs::MapMut<'a>, Box<dyn Error + Send + Sync>> {
        object
            .maps_mut()
            .find(|map| map.name().to_string_lossy() == name)
            .ok_or_else(|| format!("missing BPF map `{name}`").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_layout_groups_sockets_by_numa_node() {
        let layout = build_numa_layout(&[
            ListenerSocket {
                listener_id: 0,
                core_id: 8,
                numa_node: 1,
            },
            ListenerSocket {
                listener_id: 1,
                core_id: 0,
                numa_node: 0,
            },
            ListenerSocket {
                listener_id: 2,
                core_id: 9,
                numa_node: 1,
            },
        ])
        .expect("layout");

        assert_eq!(
            layout.placements,
            vec![
                SocketPlacement {
                    map_index: 0,
                    listener_id: 1,
                    core_id: 0,
                    numa_node: 0,
                },
                SocketPlacement {
                    map_index: 1,
                    listener_id: 0,
                    core_id: 8,
                    numa_node: 1,
                },
                SocketPlacement {
                    map_index: 2,
                    listener_id: 2,
                    core_id: 9,
                    numa_node: 1,
                },
            ]
        );
        assert_eq!(layout.ranges[0], NumaRange { start: 0, len: 1 });
        assert_eq!(layout.ranges[1], NumaRange { start: 1, len: 2 });
    }

    #[test]
    fn build_layout_rejects_out_of_range_numa_node() {
        let err = build_numa_layout(&[ListenerSocket {
            listener_id: 0,
            core_id: 0,
            numa_node: MAX_NUMA_NODES as u32,
        }])
        .expect_err("out of range NUMA node should fail");

        assert_eq!(
            err,
            NumaLayoutError::NumaNodeOutOfRange {
                numa_node: MAX_NUMA_NODES as u32,
                max: MAX_NUMA_NODES,
            }
        );
    }
}
