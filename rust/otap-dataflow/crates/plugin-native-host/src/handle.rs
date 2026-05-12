// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Host-side implementation of the [`OtapHostVTable`] accessors.
//!
//! Plugins receive an opaque [`OtapPdataHandle`] together with a pointer
//! to a static [`OtapHostVTable`]. Behind the handle is a borrowed view
//! over the host's `OtapPdata`; behind the vtable are the functions in
//! this module.
//!
//! All accessor return values are borrowed for the duration of one
//! `process` call. The plugin must not retain them. The host enforces
//! this by re-decoding the protobuf on each lookup and storing the
//! decoded buffers inside the [`HostHandleState`] that lives only as
//! long as the dispatch.
//!
//! Phase 1 implements lookups against OTLP-bytes payloads only. Calls
//! against OTAP Arrow records return [`HOST_UNSUPPORTED`]; future
//! phases will add Arrow-aware accessors.

use core::ffi::c_void;
use std::cell::RefCell;
use std::ptr;

use otap_df_otap::pdata::OtapPdata;
use otap_df_pdata::OtapPayload;
use otap_df_pdata::OtlpProtoBytes;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceRequest;
use otap_df_pdata::proto::opentelemetry::collector::metrics::v1::ExportMetricsServiceRequest;
use otap_df_pdata::proto::opentelemetry::collector::trace::v1::ExportTraceServiceRequest;
use otap_df_pdata::proto::opentelemetry::common::v1::any_value::Value as AnyValueKind;
use otap_df_pdata::proto::opentelemetry::resource::v1::Resource;
use prost::Message;

use otap_df_plugin_abi::{
    HOST_INTERNAL, HOST_INVALID, HOST_NOT_FOUND, HOST_OK, HOST_UNSUPPORTED,
    OTAP_PLUGIN_ABI_VERSION_V1, OtapHostVTable, OtapPdataHandle, signal_tag,
};

/// Per-call host state lifted across the FFI boundary. The plugin sees
/// this only as an [`OtapPdataHandle`] (`*const c_void`).
///
/// The state owns:
///   * a borrow of the host's `OtapPdata` (so accessors can read it);
///   * a small interior-mutable scratch arena that owns any decoded
///     bytes/strings the plugin asked for during this call.
///
/// The arena is dropped when `process` returns, invalidating every
/// borrowed pointer the plugin received — exactly the lifetime contract
/// documented on [`OtapHostVTable`].
pub struct HostHandleState<'p> {
    pdata: &'p OtapPdata,
    /// Cached decoded proto requests so repeated lookups don't re-parse.
    /// Wrapped in [`RefCell`] because the C ABI hands us a `*const`
    /// handle but accessor calls need to mutate the cache.
    cache: RefCell<DecodeCache>,
}

#[derive(Default)]
struct DecodeCache {
    logs: Option<Result<ExportLogsServiceRequest, prost::DecodeError>>,
    metrics: Option<Result<ExportMetricsServiceRequest, prost::DecodeError>>,
    traces: Option<Result<ExportTraceServiceRequest, prost::DecodeError>>,
    /// Arena of strings the host has handed out as borrowed pointers
    /// during this call. Kept alive until the state drops so the plugin
    /// can safely read the bytes during `process`.
    strings: Vec<String>,
}

impl<'p> HostHandleState<'p> {
    /// Construct a fresh per-call state borrowing the host's pdata.
    #[must_use]
    pub fn new(pdata: &'p OtapPdata) -> Self {
        Self {
            pdata,
            cache: RefCell::new(DecodeCache::default()),
        }
    }

    /// Cast the state to the opaque handle the plugin sees.
    #[must_use]
    pub fn as_handle(&self) -> OtapPdataHandle {
        ptr::from_ref(self).cast::<c_void>()
    }
}

/// Decode `handle` back into a `&HostHandleState`. Returns `None` if the
/// pointer is null — guards against plugin misuse.
///
/// # Safety
///
/// The caller asserts that `handle` was produced by
/// [`HostHandleState::as_handle`] and is still alive for the duration of
/// the call. The host always passes a live pointer; misbehaving plugins
/// that fabricate handles trigger UB, which is the same trust posture as
/// any other native-plugin operation.
unsafe fn from_handle<'a>(handle: OtapPdataHandle) -> Option<&'a HostHandleState<'a>> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: see function-level docs.
    Some(unsafe { &*handle.cast::<HostHandleState<'a>>() })
}

unsafe extern "C" fn host_signal_type(handle: OtapPdataHandle) -> u32 {
    // SAFETY: handle is documented to be a valid HostHandleState pointer.
    let Some(state) = (unsafe { from_handle(handle) }) else {
        // No live state — fail-closed by reporting Logs (callers should
        // already null-check the handle before calling).
        return signal_tag::LOGS;
    };
    match state.pdata.signal_type() {
        otap_df_config::SignalType::Logs => signal_tag::LOGS,
        otap_df_config::SignalType::Metrics => signal_tag::METRICS,
        otap_df_config::SignalType::Traces => signal_tag::TRACES,
    }
}

fn first_resource<'a>(
    cache: &'a mut DecodeCache,
    payload: &OtlpProtoBytes,
) -> Result<Option<&'a Resource>, i32> {
    match payload {
        OtlpProtoBytes::ExportLogsRequest(b) => {
            let entry = cache
                .logs
                .get_or_insert_with(|| ExportLogsServiceRequest::decode(b.clone()));
            match entry {
                Ok(req) => Ok(req
                    .resource_logs
                    .first()
                    .and_then(|rl| rl.resource.as_ref())),
                Err(_) => Err(HOST_INTERNAL),
            }
        }
        OtlpProtoBytes::ExportMetricsRequest(b) => {
            let entry = cache
                .metrics
                .get_or_insert_with(|| ExportMetricsServiceRequest::decode(b.clone()));
            match entry {
                Ok(req) => Ok(req
                    .resource_metrics
                    .first()
                    .and_then(|rm| rm.resource.as_ref())),
                Err(_) => Err(HOST_INTERNAL),
            }
        }
        OtlpProtoBytes::ExportTracesRequest(b) => {
            let entry = cache
                .traces
                .get_or_insert_with(|| ExportTraceServiceRequest::decode(b.clone()));
            match entry {
                Ok(req) => Ok(req
                    .resource_spans
                    .first()
                    .and_then(|rs| rs.resource.as_ref())),
                Err(_) => Err(HOST_INTERNAL),
            }
        }
    }
}

unsafe extern "C" fn host_get_resource_attr_str(
    handle: OtapPdataHandle,
    key_ptr: *const u8,
    key_len: usize,
    out_ptr: *mut *const u8,
    out_len: *mut usize,
) -> i32 {
    if key_ptr.is_null() || out_ptr.is_null() || out_len.is_null() {
        return HOST_INVALID;
    }
    // SAFETY: handle invariants documented; key bytes valid for `key_len`.
    let Some(state) = (unsafe { from_handle(handle) }) else {
        return HOST_INVALID;
    };
    let key_bytes = unsafe { std::slice::from_raw_parts(key_ptr, key_len) };
    let Ok(key) = std::str::from_utf8(key_bytes) else {
        return HOST_INVALID;
    };

    let payload = state.pdata.payload_view();
    let proto = match payload {
        OtapPayload::OtlpBytes(p) => p,
        // Phase-1: arrow records lookups not implemented.
        OtapPayload::OtapArrowRecords(_) => return HOST_UNSUPPORTED,
    };

    let mut cache = state.cache.borrow_mut();
    // Extract the matching string value (if any) in a scoped block so the
    // borrow on `cache` from `first_resource` is released before we push
    // into `cache.strings`.
    let lookup: Result<Option<String>, i32> = {
        let resource = match first_resource(&mut cache, proto) {
            Ok(Some(r)) => r,
            Ok(None) => return HOST_NOT_FOUND,
            Err(code) => return code,
        };
        let mut found: Result<Option<String>, i32> = Ok(None);
        for attr in &resource.attributes {
            if attr.key == key {
                match attr.value.as_ref().and_then(|v| v.value.as_ref()) {
                    Some(AnyValueKind::StringValue(s)) => {
                        found = Ok(Some(s.clone()));
                    }
                    Some(_) => found = Err(HOST_UNSUPPORTED),
                    None => found = Err(HOST_NOT_FOUND),
                }
                break;
            }
        }
        found
    };
    match lookup {
        Ok(Some(s)) => {
            cache.strings.push(s);
            let stored = cache.strings.last().expect("just pushed");
            unsafe {
                ptr::write(out_ptr, stored.as_ptr());
                ptr::write(out_len, stored.len());
            }
            HOST_OK
        }
        Ok(None) => HOST_NOT_FOUND,
        Err(code) => code,
    }
}

/// Marker trait for types that can supply a `&'static OtapHostVTable`.
///
/// Provided so callers can write generic code that doesn't depend on
/// the precise vtable singleton path. Phase 1 has only one
/// implementation: [`host_vtable`] returns the static singleton.
pub trait HostVTableProvider {
    /// Borrow the static host vtable.
    fn host_vtable(&self) -> &'static OtapHostVTable;
}

/// The single static host vtable shared across all native plugin calls.
#[must_use]
pub fn host_vtable() -> &'static OtapHostVTable {
    &HOST_VTABLE
}

static HOST_VTABLE: OtapHostVTable = OtapHostVTable {
    abi_version: OTAP_PLUGIN_ABI_VERSION_V1,
    signal_type: Some(host_signal_type),
    get_resource_attr_str: Some(host_get_resource_attr_str),
    _reserved: [0; 16],
};
