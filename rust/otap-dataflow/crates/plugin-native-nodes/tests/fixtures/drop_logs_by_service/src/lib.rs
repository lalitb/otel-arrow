// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Sample native plugin: drops logs whose `service.name` matches a
//! configured value, forwards everything else unchanged.
//!
//! Demonstrates the phase-1 native plugin ABI:
//! - registers via `otap_plugin_register_v1`
//! - publishes a single processor component
//! - reads the host's `service.name` resource attribute via the host
//!   vtable
//! - returns `ForwardSame` / `Drop` / `Error` according to config
//!
//! Build with `cargo build --release` from this crate's directory.

#![allow(unsafe_code)]

use core::ffi::c_void;
use std::ptr;
use std::slice;
use std::sync::Mutex;

use otap_df_plugin_abi::{
    HOST_INVALID, HOST_NOT_FOUND, HOST_OK, HOST_UNSUPPORTED, OTAP_PLUGIN_ABI_VERSION_V1,
    OtapHostVTable, OtapPdataHandle, OtapPluginDescriptorRaw, OtapPluginInstance,
    OtapPluginVTable, OtapPluginVerb,
};

static COMPONENTS_JSON: &str = r#"[
    {
        "urn": "urn:example:processor:drop_logs_by_service",
        "kind": "processor",
        "supported_payloads": ["otlp-proto-bytes"],
        "output_arity": "single"
    }
]"#;

static PLUGIN_NAME: &str = "drop_logs_by_service";
static PLUGIN_VERSION: &str = "0.1.0";

unsafe extern "C" fn descriptor_export(out: *mut OtapPluginDescriptorRaw) -> i32 {
    if out.is_null() {
        return HOST_INVALID;
    }
    let raw = OtapPluginDescriptorRaw {
        abi_version: OTAP_PLUGIN_ABI_VERSION_V1,
        name_ptr: PLUGIN_NAME.as_ptr(),
        name_len: PLUGIN_NAME.len(),
        version_ptr: PLUGIN_VERSION.as_ptr(),
        version_len: PLUGIN_VERSION.len(),
        plugin_api_major: 0,
        plugin_api_minor: 1,
        components_json_ptr: COMPONENTS_JSON.as_ptr(),
        components_json_len: COMPONENTS_JSON.len(),
    };
    unsafe {
        ptr::write(out, raw);
    }
    HOST_OK
}

#[derive(serde::Deserialize)]
struct Config {
    /// Value of `service.name` to match for dropping.
    drop_when_service_name_eq: String,
    /// When `true`, the plugin returns `Error` instead of `Drop` on a
    /// match; used to exercise the engine's error path.
    #[serde(default)]
    error_on_match: bool,
}

fn parse_config(bytes: &[u8]) -> Result<Config, String> {
    let s = std::str::from_utf8(bytes).map_err(|e| format!("config utf8: {e}"))?;
    serde_json::from_str(s).map_err(|e| format!("config decode: {e}"))
}

fn write_err(out_err_ptr: *mut *const u8, out_err_len: *mut usize, msg: &'static str) {
    if out_err_ptr.is_null() || out_err_len.is_null() {
        return;
    }
    unsafe {
        ptr::write(out_err_ptr, msg.as_ptr());
        ptr::write(out_err_len, msg.len());
    }
}

unsafe extern "C" fn validate_config_export(
    _urn_ptr: *const u8,
    _urn_len: usize,
    config_json_ptr: *const u8,
    config_json_len: usize,
    out_err_ptr: *mut *const u8,
    out_err_len: *mut usize,
) -> i32 {
    if config_json_ptr.is_null() {
        write_err(out_err_ptr, out_err_len, "null config pointer");
        return HOST_INVALID;
    }
    let cfg = unsafe { slice::from_raw_parts(config_json_ptr, config_json_len) };
    match parse_config(cfg) {
        Ok(_) => HOST_OK,
        Err(_msg) => {
            // Static error message: detailed message would require a
            // per-instance buffer.
            write_err(
                out_err_ptr,
                out_err_len,
                "drop_logs_by_service: invalid config (expected JSON object with `drop_when_service_name_eq` string)",
            );
            HOST_INVALID
        }
    }
}

struct Instance {
    config: Config,
    /// Per-instance scratch for the most recent error message returned
    /// across FFI. Held in a Mutex so concurrent callers (none in
    /// phase-1, but be defensive) do not race.
    last_error: Mutex<String>,
}

unsafe extern "C" fn instance_new_export(
    _urn_ptr: *const u8,
    _urn_len: usize,
    config_json_ptr: *const u8,
    config_json_len: usize,
    _host: *const OtapHostVTable,
    out_err_ptr: *mut *const u8,
    out_err_len: *mut usize,
) -> OtapPluginInstance {
    if config_json_ptr.is_null() {
        write_err(out_err_ptr, out_err_len, "null config pointer");
        return ptr::null_mut();
    }
    let cfg_bytes = unsafe { slice::from_raw_parts(config_json_ptr, config_json_len) };
    let config = match parse_config(cfg_bytes) {
        Ok(c) => c,
        Err(_) => {
            write_err(
                out_err_ptr,
                out_err_len,
                "drop_logs_by_service: invalid instance config",
            );
            return ptr::null_mut();
        }
    };
    let inst = Box::new(Instance {
        config,
        last_error: Mutex::new(String::new()),
    });
    Box::into_raw(inst).cast::<c_void>()
}

unsafe extern "C" fn instance_drop_export(instance: OtapPluginInstance) {
    if instance.is_null() {
        return;
    }
    // SAFETY: instance was produced by instance_new_export via Box::into_raw.
    let _ = unsafe { Box::from_raw(instance.cast::<Instance>()) };
}

unsafe extern "C" fn process_export(
    instance: OtapPluginInstance,
    pdata: OtapPdataHandle,
    host: *const OtapHostVTable,
    out_err_ptr: *mut *const u8,
    out_err_len: *mut usize,
) -> u32 {
    if instance.is_null() || host.is_null() {
        write_err(out_err_ptr, out_err_len, "null instance or host");
        return OtapPluginVerb::Error as u32;
    }
    // SAFETY: instance is owned by the host as an OtapPluginInstance
    // produced by instance_new_export; host vtable lifetime documented.
    let inst = unsafe { &*instance.cast::<Instance>() };
    let host = unsafe { &*host };

    // Look up service.name on the resource. This call returns a borrow
    // valid only until process_export returns.
    let key = b"service.name";
    let mut value_ptr: *const u8 = ptr::null();
    let mut value_len: usize = 0;
    // Host accessor slots are `Option<unsafe extern "C" fn(...)>` per
    // the v1 ABI. Phase-1 hosts populate every slot; null is a host
    // bug that we surface as HOST_UNSUPPORTED.
    let rc = match host.get_resource_attr_str {
        Some(f) => unsafe {
            f(
                pdata,
                key.as_ptr(),
                key.len(),
                &mut value_ptr,
                &mut value_len,
            )
        },
        None => HOST_UNSUPPORTED,
    };
    let service_name: Option<&str> = match rc {
        c if c == HOST_OK => {
            if value_ptr.is_null() {
                None
            } else {
                let bytes = unsafe { slice::from_raw_parts(value_ptr, value_len) };
                std::str::from_utf8(bytes).ok()
            }
        }
        c if c == HOST_NOT_FOUND => None,
        c if c == HOST_UNSUPPORTED => None,
        _ => None,
    };

    let matched = service_name == Some(inst.config.drop_when_service_name_eq.as_str());
    if matched {
        if inst.config.error_on_match {
            // Stash a per-instance message so the host pointer remains
            // valid until the next process call.
            let mut guard = inst.last_error.lock().unwrap();
            *guard = format!(
                "drop_logs_by_service: error_on_match=true matched service.name={}",
                inst.config.drop_when_service_name_eq
            );
            unsafe {
                ptr::write(out_err_ptr, guard.as_ptr());
                ptr::write(out_err_len, guard.len());
            }
            return OtapPluginVerb::Error as u32;
        }
        return OtapPluginVerb::Drop as u32;
    }
    OtapPluginVerb::ForwardSame as u32
}

static PLUGIN_VTABLE: OtapPluginVTable = OtapPluginVTable {
    abi_version: OTAP_PLUGIN_ABI_VERSION_V1,
    descriptor: Some(descriptor_export),
    validate_config: Some(validate_config_export),
    instance_new: Some(instance_new_export),
    instance_drop: Some(instance_drop_export),
    process: Some(process_export),
    _reserved: [0; 8],
};

/// Exported entry point looked up by the host via `dlsym`.
///
/// # Safety
///
/// Returns a pointer to a `'static` vtable that lives for the lifetime
/// of the loaded library. The host treats the pointer as borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otap_plugin_register_v1() -> *const OtapPluginVTable {
    &PLUGIN_VTABLE
}
