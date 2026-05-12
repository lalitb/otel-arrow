// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Native plugin runtime invocation surface.
//!
//! Mirrors `otap-df-plugin-host::runner` in spirit but operates on
//! borrowed [`OtapPdata`] handles instead of OTLP-bytes payloads.
//!
//! # Panic policy
//!
//! See `otap-df-plugin-abi`'s crate-level "Panic contract" section.
//! Plugin entry points are declared `extern "C"`; the host **does not**
//! wrap them in `catch_unwind`. Plugins must be built with
//! `panic = "abort"` (the recommended profile) or catch their own
//! panics inside the plugin and convert them to
//! [`OtapPluginVerb::Error`]. An uncaught panic crossing the ABI
//! boundary is undefined behavior and aborts the process under modern
//! rustc.

use std::fmt::Debug;
use std::ptr;
use std::sync::Arc;

use libloading::Library;

use otap_df_otap::pdata::OtapPdata;
use otap_df_plugin_abi::{
    HOST_OK, OTAP_PLUGIN_ABI_VERSION_V1, OtapPluginInstance, OtapPluginVTable, OtapPluginVerb,
};

use crate::handle::{HostHandleState, host_vtable};

/// Phase-1 verbs returned by a native processor plugin call.
#[derive(Debug)]
pub enum NativeVerb {
    /// Forward the original `OtapPdata` unchanged. Zero-copy on the
    /// host: the engine emits the original message to the default
    /// output port.
    ForwardSame,
    /// Drop the `OtapPdata` (filter / no emit).
    Drop,
    /// Plugin failed. Carries the host-owned error message.
    Error(String),
}

/// A loaded native plugin library, kept alive for the lifetime of every
/// runner that depends on it.
///
/// Wrapping the [`Library`] in [`Arc`] lets multiple runners share one
/// `dlopen`. Phase 1 never invokes `dlclose` explicitly; the library
/// remains mapped while any clone of the [`Arc`] is alive (and
/// `df_engine` keeps loaded plugin handles alive for the lifetime of
/// the process).
pub struct SharedPluginLibrary {
    /// The mapped shared library. Held to keep the dlopen mapping
    /// alive for the lifetime of every clone of this struct. Never
    /// read directly.
    #[allow(dead_code)]
    pub(crate) library: Library,
    pub(crate) vtable: *const OtapPluginVTable,
}

// SAFETY: The plugin vtable is a `*const` to a `'static` object inside
// the loaded library. The library is kept alive by `Arc<Library>` for
// the lifetime of every clone of `SharedPluginLibrary`. Per phase-1
// trust contract, plugin authors guarantee the vtable function pointers
// are thread-safe.
unsafe impl Send for SharedPluginLibrary {}
unsafe impl Sync for SharedPluginLibrary {}

impl Debug for SharedPluginLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedPluginLibrary")
            .field("vtable", &self.vtable)
            .finish_non_exhaustive()
    }
}

/// Validator wired through the plugin's `validate_config` export.
///
/// Implements the same `validate(config_json) -> Result<(), String>`
/// shape as `otap_df_plugin_host::PluginConfigValidator` so the
/// registry-bridge layer can convert it into the engine's
/// `ConfigValidator::Dynamic` variant uniformly with the wasm path.
pub struct NativePluginConfigValidator {
    pub(crate) library: Arc<SharedPluginLibrary>,
    pub(crate) urn: String,
}

impl Debug for NativePluginConfigValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativePluginConfigValidator")
            .field("urn", &self.urn)
            .finish()
    }
}

impl NativePluginConfigValidator {
    /// Validate a JSON config against the plugin's `validate_config`.
    ///
    /// Returns `Ok(())` on success, `Err(message)` on rejection.
    pub fn validate(&self, config_json: &str) -> Result<(), String> {
        let vtable = self.library.vtable;
        let urn = self.urn.as_bytes();
        let cfg = config_json.as_bytes();
        let mut out_ptr: *const u8 = ptr::null();
        let mut out_len: usize = 0;

        // SAFETY: vtable is a valid pointer for the lifetime of the
        // library Arc; `validate_config` was checked non-null at load
        // time by `validate_vtable`, so the unwrap is sound.
        let validate_fn = unsafe { (*vtable).validate_config }
            .expect("validate_config slot missing — should have been rejected at load");
        let rc = unsafe {
            validate_fn(
                urn.as_ptr(),
                urn.len(),
                cfg.as_ptr(),
                cfg.len(),
                &mut out_ptr,
                &mut out_len,
            )
        };

        if rc == HOST_OK {
            return Ok(());
        }
        // Copy the error message out of plugin-owned memory.
        let msg = if !out_ptr.is_null() && out_len > 0 {
            // SAFETY: the plugin promised a UTF-8 message of this
            // length. Garbage bytes still produce a String via
            // lossy-utf8 fallback so we never return Err(()).
            let bytes = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            format!("plugin validate_config returned non-zero rc={rc}")
        };
        Err(msg)
    }
}

/// Per-node native processor instance.
///
/// Owns the opaque [`OtapPluginInstance`] and ensures
/// [`OtapPluginVTable::instance_drop`] runs on drop. Construction goes
/// through [`PluginInstanceHandle::new`].
pub struct PluginInstanceHandle {
    library: Arc<SharedPluginLibrary>,
    instance: OtapPluginInstance,
}

// SAFETY: Same posture as SharedPluginLibrary — phase-1 contract puts
// thread-safety on the plugin author (one instance per (node, core);
// engine guarantees no concurrent calls into the same instance).
unsafe impl Send for PluginInstanceHandle {}
unsafe impl Sync for PluginInstanceHandle {}

impl Debug for PluginInstanceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginInstanceHandle")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl Drop for PluginInstanceHandle {
    fn drop(&mut self) {
        if self.instance.is_null() {
            return;
        }
        let vtable = self.library.vtable;
        // SAFETY: vtable lives with the Arc; instance_drop slot was
        // checked non-null at load. A panic across this boundary aborts
        // the process — that is the plugin author's responsibility per
        // the Panic Contract.
        let drop_fn = unsafe { (*vtable).instance_drop }
            .expect("instance_drop slot missing — should have been rejected at load");
        unsafe { drop_fn(self.instance) };
        self.instance = ptr::null_mut();
    }
}

impl PluginInstanceHandle {
    /// Construct a per-node plugin instance via `instance_new`.
    pub fn new(
        library: Arc<SharedPluginLibrary>,
        urn: &str,
        config_json: &str,
    ) -> Result<Self, String> {
        let vtable = library.vtable;
        let urn_bytes = urn.as_bytes();
        let cfg = config_json.as_bytes();
        let mut out_ptr: *const u8 = ptr::null();
        let mut out_len: usize = 0;
        // SAFETY: validated at load.
        let new_fn = unsafe { (*vtable).instance_new }
            .expect("instance_new slot missing — should have been rejected at load");
        let inst = unsafe {
            new_fn(
                urn_bytes.as_ptr(),
                urn_bytes.len(),
                cfg.as_ptr(),
                cfg.len(),
                host_vtable(),
                &mut out_ptr,
                &mut out_len,
            )
        };

        if inst.is_null() {
            let msg = if !out_ptr.is_null() && out_len > 0 {
                // SAFETY: see validator.
                let bytes = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
                String::from_utf8_lossy(bytes).into_owned()
            } else {
                "plugin instance_new returned NULL".to_string()
            };
            return Err(msg);
        }
        Ok(Self {
            library,
            instance: inst,
        })
    }
}

/// Executes a single `process` call against an instance using the
/// borrowed pdata. Returns a host-owned [`NativeVerb`].
///
/// This trait does **not** consume or copy the pdata. The caller
/// (typically an engine adapter) decides what to do with the original
/// pdata based on the returned verb (`ForwardSame` ⇒ emit original;
/// `Drop` ⇒ discard; `Error` ⇒ surface as a node error).
pub trait NativeProcessorRunner: Send + Sync + Debug {
    /// Invoke the plugin's `process` against `pdata`.
    fn process(&self, pdata: &OtapPdata) -> NativeVerb;
}

/// Concrete [`NativeProcessorRunner`] backed by a [`PluginInstanceHandle`].
pub struct NativeProcessorRunnerImpl {
    pub(crate) instance: PluginInstanceHandle,
}

impl NativeProcessorRunnerImpl {
    /// Construct a runner from an already-built [`PluginInstanceHandle`].
    #[must_use]
    pub fn new(instance: PluginInstanceHandle) -> Self {
        Self { instance }
    }
}

impl Debug for NativeProcessorRunnerImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeProcessorRunnerImpl")
            .field("instance", &self.instance)
            .finish()
    }
}

impl NativeProcessorRunner for NativeProcessorRunnerImpl {
    fn process(&self, pdata: &OtapPdata) -> NativeVerb {
        let vtable = self.instance.library.vtable;
        let inst = self.instance.instance;
        // Per-call host state. Drops at end of this call, invalidating
        // every borrowed pointer the plugin might have read.
        let state = HostHandleState::new(pdata);
        let handle = state.as_handle();
        let mut out_ptr: *const u8 = ptr::null();
        let mut out_len: usize = 0;

        // SAFETY: validated at load.
        let process_fn = unsafe { (*vtable).process }
            .expect("process slot missing — should have been rejected at load");
        let rc = unsafe { process_fn(inst, handle, host_vtable(), &mut out_ptr, &mut out_len) };

        let v = match OtapPluginVerb::from_u32(rc) {
            OtapPluginVerb::ForwardSame => NativeVerb::ForwardSame,
            OtapPluginVerb::Drop => NativeVerb::Drop,
            OtapPluginVerb::Error => {
                let msg = if !out_ptr.is_null() && out_len > 0 {
                    // SAFETY: plugin promised a UTF-8 buffer.
                    let bytes = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
                    String::from_utf8_lossy(bytes).into_owned()
                } else {
                    format!("plugin returned Error verb (rc={rc}) without message")
                };
                NativeVerb::Error(msg)
            }
        };
        // `state` drops here, invalidating the borrowed handle.
        v
    }
}

/// Validate a freshly-loaded plugin vtable: ABI version, all required
/// function-pointer slots present, and every reserved slot zero.
///
/// Returns `Err(message)` describing the first violation found. The
/// host rejects such a plugin at load time so a misfilled vtable from a
/// non-Rust plugin author surfaces immediately rather than as UB on
/// first call.
pub(crate) fn validate_vtable(vtable: &OtapPluginVTable) -> Result<(), String> {
    if vtable.abi_version != OTAP_PLUGIN_ABI_VERSION_V1 {
        return Err(format!(
            "plugin advertises ABI version {}, host supports only {}",
            vtable.abi_version, OTAP_PLUGIN_ABI_VERSION_V1
        ));
    }
    if vtable.descriptor.is_none() {
        return Err("plugin vtable: descriptor slot is null".into());
    }
    if vtable.validate_config.is_none() {
        return Err("plugin vtable: validate_config slot is null".into());
    }
    if vtable.instance_new.is_none() {
        return Err("plugin vtable: instance_new slot is null".into());
    }
    if vtable.instance_drop.is_none() {
        return Err("plugin vtable: instance_drop slot is null".into());
    }
    if vtable.process.is_none() {
        return Err("plugin vtable: process slot is null".into());
    }
    for (i, slot) in vtable._reserved.iter().enumerate() {
        if *slot != 0 {
            return Err(format!(
                "plugin vtable: _reserved[{i}] = {slot}; phase-1 hosts require all reserved slots to be zero"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_plugin_abi::{
        OTAP_PLUGIN_ABI_VERSION_V1, OtapPluginDescriptorRaw, OtapPluginInstance, OtapPluginVTable,
    };
    use std::ffi::c_void;

    // --- Stub function pointers used to populate a "valid" vtable. ---
    unsafe extern "C" fn stub_descriptor(_out: *mut OtapPluginDescriptorRaw) -> i32 {
        HOST_OK
    }
    unsafe extern "C" fn stub_validate_config(
        _: *const u8,
        _: usize,
        _: *const u8,
        _: usize,
        _: *mut *const u8,
        _: *mut usize,
    ) -> i32 {
        HOST_OK
    }
    unsafe extern "C" fn stub_instance_new(
        _: *const u8,
        _: usize,
        _: *const u8,
        _: usize,
        _: *const otap_df_plugin_abi::OtapHostVTable,
        _: *mut *const u8,
        _: *mut usize,
    ) -> OtapPluginInstance {
        ptr::null_mut::<c_void>()
    }
    unsafe extern "C" fn stub_instance_drop(_: OtapPluginInstance) {}
    unsafe extern "C" fn stub_process(
        _: OtapPluginInstance,
        _: otap_df_plugin_abi::OtapPdataHandle,
        _: *const otap_df_plugin_abi::OtapHostVTable,
        _: *mut *const u8,
        _: *mut usize,
    ) -> u32 {
        OtapPluginVerb::ForwardSame as u32
    }

    fn good_vtable() -> OtapPluginVTable {
        OtapPluginVTable {
            abi_version: OTAP_PLUGIN_ABI_VERSION_V1,
            descriptor: Some(stub_descriptor),
            validate_config: Some(stub_validate_config),
            instance_new: Some(stub_instance_new),
            instance_drop: Some(stub_instance_drop),
            process: Some(stub_process),
            _reserved: [0; 8],
        }
    }

    #[test]
    fn accepts_full_vtable() {
        validate_vtable(&good_vtable()).expect("valid vtable accepted");
    }

    #[test]
    fn rejects_wrong_abi_version() {
        let mut v = good_vtable();
        v.abi_version = OTAP_PLUGIN_ABI_VERSION_V1 + 1;
        let err = validate_vtable(&v).unwrap_err();
        assert!(err.contains("ABI version"), "{err}");
    }

    #[test]
    fn rejects_null_descriptor_slot() {
        let mut v = good_vtable();
        v.descriptor = None;
        let err = validate_vtable(&v).unwrap_err();
        assert!(err.contains("descriptor"), "{err}");
    }

    #[test]
    fn rejects_null_validate_config_slot() {
        let mut v = good_vtable();
        v.validate_config = None;
        let err = validate_vtable(&v).unwrap_err();
        assert!(err.contains("validate_config"), "{err}");
    }

    #[test]
    fn rejects_null_instance_new_slot() {
        let mut v = good_vtable();
        v.instance_new = None;
        let err = validate_vtable(&v).unwrap_err();
        assert!(err.contains("instance_new"), "{err}");
    }

    #[test]
    fn rejects_null_instance_drop_slot() {
        let mut v = good_vtable();
        v.instance_drop = None;
        let err = validate_vtable(&v).unwrap_err();
        assert!(err.contains("instance_drop"), "{err}");
    }

    #[test]
    fn rejects_null_process_slot() {
        let mut v = good_vtable();
        v.process = None;
        let err = validate_vtable(&v).unwrap_err();
        assert!(err.contains("process"), "{err}");
    }

    #[test]
    fn rejects_nonzero_reserved_slot() {
        let mut v = good_vtable();
        v._reserved[3] = 42;
        let err = validate_vtable(&v).unwrap_err();
        assert!(err.contains("_reserved[3]"), "{err}");
    }
}
