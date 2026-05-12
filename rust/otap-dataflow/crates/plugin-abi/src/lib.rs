// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Stable C ABI shared between the otap-dataflow native plugin host and
//! cdylib plugins.
//!
//! # Phase 1 scope
//!
//! - Processor plugins only.
//! - Verbs: [`OtapPluginVerb::ForwardSame`], [`OtapPluginVerb::Drop`],
//!   [`OtapPluginVerb::Error`].
//! - Plugins receive an opaque [`OtapPdataHandle`] and inspect it through
//!   the host-supplied [`OtapHostVTable`]. Payload bytes are never copied
//!   across the FFI boundary.
//! - Versioned entry symbol [`REGISTER_SYMBOL_V1`] (`otap_plugin_register_v1`).
//!   Plugins built against future ABIs export a different versioned
//!   symbol so old hosts surface a clean "missing symbol" load error.
//!
//! # Lifetime / ownership rules (read these)
//!
//! - The host owns the underlying `OtapPdata`. The plugin receives an
//!   [`OtapPdataHandle`] borrowed for the duration of one
//!   [`OtapPluginVTable::process`] call. The plugin **must not** retain
//!   the handle past the return of `process`.
//! - **The plugin must not share the `OtapPdataHandle` across threads
//!   or call host accessors with the same handle from more than one
//!   thread, even within a single `process` invocation.** Host accessor
//!   state behind the handle is not internally synchronized; concurrent
//!   accessor calls against the same handle are undefined behavior.
//! - Out-pointers returned by host accessors (e.g. attribute values from
//!   [`OtapHostVTable::get_resource_attr_str`]) are borrowed and valid
//!   only until `process` returns. The plugin **must not** free, mutate,
//!   or retain them.
//! - All pointers returned by the plugin in [`OtapPluginDescriptorRaw`]
//!   and through `out_err_*` parameters must point to memory owned by
//!   the plugin and remain valid until the next call into the plugin
//!   that may invalidate them. The host copies these bytes into Rust
//!   strings before returning control.
//!
//! # Panic contract (read this)
//!
//! All ABI entry points are declared `extern "C"`. Unwinding across an
//! `extern "C"` boundary is undefined behavior in Rust and aborts the
//! process under modern rustc default settings. The host **does not**
//! catch plugin panics.
//!
//! Plugins **must** ensure no panic crosses this boundary by either:
//!
//! 1. Building with `panic = "abort"` (the recommended profile, used by
//!    the sample plugin), so any internal panic terminates the process
//!    cleanly rather than UB; or
//! 2. Wrapping the entirety of every exported function body in an
//!    in-plugin `catch_unwind` and converting any caught panic to
//!    `OtapPluginVerb::Error` (or a non-zero rc on `descriptor` /
//!    `validate_config`).
//!
//! Native plugins are part of the host trust boundary. The ABI provides
//! no isolation; a plugin that aborts terminates the whole collector.
//!
//! # Why plain C ABI (not abi_stable)
//!
//! The phase-1 surface is small (one register fn, ~5 vtable entries,
//! one host accessor at first). `abi_stable` would add a heavy dep and
//! lock plugin authors into its conventions; plain `#[repr(C)]` keeps
//! the door open for non-Rust plugins later (the same approach used by
//! Arrow C Data Interface, DuckDB extensions, Postgres `PG_MODULE_MAGIC`).

#![no_std]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![allow(unsafe_code)]

use core::ffi::c_void;

/// ABI major version recognized by this header.
///
/// Bumped only on breaking layout changes to [`OtapPluginVTable`],
/// [`OtapHostVTable`], [`OtapPluginDescriptorRaw`], or
/// [`OtapPluginVerb`]. Additive changes (new accessor slot in a
/// `_reserved` block, new verb tag) do not bump this version.
pub const OTAP_PLUGIN_ABI_VERSION_V1: u32 = 1;

/// Symbol the host looks up via `dlsym` after `dlopen`.
///
/// The version suffix is intentional: when a future ABI v2 ships the
/// plugin entry symbol becomes `otap_plugin_register_v2`. A plugin built
/// for v2 loaded by a v1 host fails with a clean missing-symbol error
/// rather than executing under a layout-mismatched vtable.
pub const REGISTER_SYMBOL_V1: &[u8] = b"otap_plugin_register_v1\0";

/// Verb returned by a processor plugin's `process` call.
///
/// Numeric tags are part of the ABI and must not be reused.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OtapPluginVerb {
    /// Host should forward the original `OtapPdata` to the default
    /// output unchanged. Zero-copy on the host side: no payload
    /// serialization, no allocation.
    ForwardSame = 0,
    /// Host should drop the `OtapPdata` (filter / no emit).
    Drop = 1,
    /// Plugin failed. The host treats this as a processor node error,
    /// not a silent drop. The plugin may write a message via the
    /// `out_err_*` parameters of `process`; if not, the host reports a
    /// generic "plugin returned Error" message.
    Error = 2,
}

impl OtapPluginVerb {
    /// Decode a `u32` returned across FFI into a verb enum.
    ///
    /// Unknown tags map to [`OtapPluginVerb::Error`] so the host
    /// fail-closed when a plugin returns garbage.
    #[must_use]
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::ForwardSame,
            1 => Self::Drop,
            _ => Self::Error,
        }
    }
}

/// Opaque handle to the host's `OtapPdata`.
///
/// The plugin treats this as an opaque token and only inspects it
/// through [`OtapHostVTable`] accessors. It is invalid after
/// [`OtapPluginVTable::process`] returns and must not be shared across
/// threads.
pub type OtapPdataHandle = *const c_void;

/// Opaque per-instance state owned by the plugin.
///
/// Created by [`OtapPluginVTable::instance_new`], destroyed by
/// [`OtapPluginVTable::instance_drop`]. The host treats it as opaque.
pub type OtapPluginInstance = *mut c_void;

/// Signal type tag passed to host accessors.
///
/// Numeric values match the rest of the codebase
/// (`SignalType::Logs = 0`, `Metrics = 1`, `Traces = 2`).
pub mod signal_tag {
    /// Logs signal.
    pub const LOGS: u32 = 0;
    /// Metrics signal.
    pub const METRICS: u32 = 1;
    /// Traces signal.
    pub const TRACES: u32 = 2;
}

/// Host accessor return code: success.
pub const HOST_OK: i32 = 0;
/// Host accessor return code: requested key not found.
pub const HOST_NOT_FOUND: i32 = 1;
/// Host accessor return code: payload kind doesn't support this lookup
/// (e.g. the pdata is OTAP Arrow records and the accessor is OTLP-specific).
pub const HOST_UNSUPPORTED: i32 = 2;
/// Host accessor return code: invalid argument (null pointer, bad utf-8).
pub const HOST_INVALID: i32 = 3;
/// Host accessor return code: internal host error decoding payload.
pub const HOST_INTERNAL: i32 = 4;

/// Host-supplied vtable of accessors a plugin may call during
/// [`OtapPluginVTable::process`].
///
/// All accessor slots are [`Option<unsafe extern "C" fn(...)>`] so the
/// plugin (and a future older host) can null-check before calling.
/// Phase-1 hosts populate every slot; null in any slot is a host bug
/// and the plugin should treat it as an unsupported operation.
#[repr(C)]
pub struct OtapHostVTable {
    /// ABI version this vtable was constructed against. Always equals
    /// [`OTAP_PLUGIN_ABI_VERSION_V1`] in phase 1.
    pub abi_version: u32,

    /// Return the signal type of the pdata behind `handle`.
    ///
    /// Returns one of [`signal_tag::LOGS`], [`signal_tag::METRICS`],
    /// [`signal_tag::TRACES`].
    pub signal_type: Option<unsafe extern "C" fn(handle: OtapPdataHandle) -> u32>,

    /// Look up a resource attribute string by key on the first resource
    /// of the pdata behind `handle`.
    ///
    /// On success, writes a borrowed pointer + length to `out_ptr` /
    /// `out_len` and returns [`HOST_OK`]. The pointer is valid only
    /// until `process` returns; the plugin must not retain or free it.
    ///
    /// On not found, returns [`HOST_NOT_FOUND`]; on unsupported payload
    /// shape, returns [`HOST_UNSUPPORTED`]; on invalid argument
    /// (e.g. null `key_ptr`), returns [`HOST_INVALID`].
    pub get_resource_attr_str: Option<
        unsafe extern "C" fn(
            handle: OtapPdataHandle,
            key_ptr: *const u8,
            key_len: usize,
            out_ptr: *mut *const u8,
            out_len: *mut usize,
        ) -> i32,
    >,

    /// Reserved accessor slots for future minor-version growth.
    /// Always zero in phase 1; the host writes zeros and a plugin must
    /// not read them.
    pub _reserved: [usize; 16],
}

/// Raw form of a [`OtapPluginDescriptor`](self) returned by
/// [`OtapPluginVTable::descriptor`].
///
/// All pointers are borrowed from the plugin and must remain valid
/// until the next plugin call that could invalidate them. The host
/// copies the contents into owned Rust strings before returning to its
/// own callers.
#[repr(C)]
pub struct OtapPluginDescriptorRaw {
    /// ABI version this descriptor was constructed against.
    pub abi_version: u32,
    /// Plugin name UTF-8 bytes.
    pub name_ptr: *const u8,
    /// Plugin name length in bytes.
    pub name_len: usize,
    /// Plugin version UTF-8 bytes.
    pub version_ptr: *const u8,
    /// Plugin version length in bytes.
    pub version_len: usize,
    /// Plugin API major version (matches `PluginApiVersion::major`).
    pub plugin_api_major: u32,
    /// Plugin API minor version.
    pub plugin_api_minor: u32,
    /// Components JSON array (UTF-8). Decoded as `Vec<ComponentDescriptor>`
    /// (defined in the `otap-df-plugin-api` crate). Embedding a JSON
    /// blob keeps the ABI struct stable while letting the descriptor
    /// schema evolve.
    pub components_json_ptr: *const u8,
    /// Components JSON length in bytes.
    pub components_json_len: usize,
}

/// The plugin's published vtable, returned by the
/// [`REGISTER_SYMBOL_V1`] entry point.
///
/// Function pointer slots are [`Option<unsafe extern "C" fn(...)>`] so
/// a non-Rust plugin author who leaves a slot unset trips a clean
/// load-time rejection (see the host's `validate_vtable`) instead of
/// undefined behavior on first call. Phase-1 hosts require every slot
/// to be `Some`.
#[repr(C)]
pub struct OtapPluginVTable {
    /// ABI version this vtable was built against.
    pub abi_version: u32,

    /// Fill the descriptor `out` with plugin name/version/api/components.
    /// Returns [`HOST_OK`] on success, nonzero on failure.
    pub descriptor: Option<unsafe extern "C" fn(out: *mut OtapPluginDescriptorRaw) -> i32>,

    /// Validate a JSON-encoded user config for a component URN.
    ///
    /// On success returns [`HOST_OK`]. On rejection writes a borrowed
    /// error message via `out_err_ptr` / `out_err_len` and returns
    /// nonzero. The message must remain valid until the next
    /// `validate_config` or `instance_new` call.
    pub validate_config: Option<
        unsafe extern "C" fn(
            urn_ptr: *const u8,
            urn_len: usize,
            config_json_ptr: *const u8,
            config_json_len: usize,
            out_err_ptr: *mut *const u8,
            out_err_len: *mut usize,
        ) -> i32,
    >,

    /// Construct a per-node instance. Returns null on failure (plugin
    /// may write a message via `out_err_*`). The host calls
    /// [`Self::instance_drop`] when the instance is no longer needed.
    pub instance_new: Option<
        unsafe extern "C" fn(
            urn_ptr: *const u8,
            urn_len: usize,
            config_json_ptr: *const u8,
            config_json_len: usize,
            host: *const OtapHostVTable,
            out_err_ptr: *mut *const u8,
            out_err_len: *mut usize,
        ) -> OtapPluginInstance,
    >,

    /// Destroy a per-node instance previously returned by
    /// [`Self::instance_new`]. Must be safe to call exactly once per
    /// instance.
    pub instance_drop: Option<unsafe extern "C" fn(instance: OtapPluginInstance)>,

    /// Process a single pdata handle.
    ///
    /// Returns one of the [`OtapPluginVerb`] tags. On
    /// [`OtapPluginVerb::Error`] the plugin may write a message via
    /// `out_err_*`. The host **does not** catch plugin panics; see the
    /// crate-level "Panic contract" section.
    pub process: Option<
        unsafe extern "C" fn(
            instance: OtapPluginInstance,
            pdata: OtapPdataHandle,
            host: *const OtapHostVTable,
            out_err_ptr: *mut *const u8,
            out_err_len: *mut usize,
        ) -> u32,
    >,

    /// Reserved vtable slots for future minor-version growth. Always
    /// zero in phase 1.
    pub _reserved: [usize; 8],
}

/// Signature of the v1 plugin entry symbol.
///
/// ```ignore
/// #[unsafe(no_mangle)]
/// pub unsafe extern "C" fn otap_plugin_register_v1() -> *const OtapPluginVTable {
///     &PLUGIN_VTABLE
/// }
/// ```
///
/// The pointer must remain valid for the lifetime of the loaded library
/// (typically a `static` in the plugin crate).
pub type OtapPluginRegisterV1 = unsafe extern "C" fn() -> *const OtapPluginVTable;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_decoding() {
        assert_eq!(OtapPluginVerb::from_u32(0), OtapPluginVerb::ForwardSame);
        assert_eq!(OtapPluginVerb::from_u32(1), OtapPluginVerb::Drop);
        assert_eq!(OtapPluginVerb::from_u32(2), OtapPluginVerb::Error);
        // Unknown tags fail-closed to Error.
        assert_eq!(OtapPluginVerb::from_u32(99), OtapPluginVerb::Error);
    }

    #[test]
    fn register_symbol_is_nul_terminated() {
        assert_eq!(*REGISTER_SYMBOL_V1.last().unwrap(), 0);
    }

    #[test]
    fn vtable_slot_size_matches_function_pointer() {
        // Option<unsafe extern "C" fn(...)> has the same layout as
        // unsafe extern "C" fn(...) (the niche optimization uses 0 for
        // None), so the on-disk vtable layout is identical to a plain
        // function-pointer table — what a C plugin writes.
        use core::mem::size_of;
        assert_eq!(
            size_of::<Option<unsafe extern "C" fn() -> i32>>(),
            size_of::<unsafe extern "C" fn() -> i32>()
        );
    }
}
