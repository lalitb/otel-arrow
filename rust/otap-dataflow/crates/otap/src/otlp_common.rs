// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Common utilities shared between OTLP/gRPC and OTLP/HTTP receivers.
//!
//! This module provides shared functionality to avoid code duplication:
//! - Precomputed empty response encoding for each signal type
//! - OtlpProtoBytes creation from raw bytes
//! - OtapPdata construction with proper context setup

use bytes::Bytes;
use otap_df_config::SignalType;
use otap_df_pdata::OtlpProtoBytes;
use otap_df_pdata::proto::opentelemetry::collector::logs::v1::ExportLogsServiceResponse;
use otap_df_pdata::proto::opentelemetry::collector::metrics::v1::ExportMetricsServiceResponse;
use otap_df_pdata::proto::opentelemetry::collector::trace::v1::ExportTraceServiceResponse;
use prost::Message;
use std::sync::OnceLock;

use crate::pdata::{Context, OtapPdata};

/// Encodes a default protobuf response for repeated use.
fn encode_response<T: Message + Default>() -> Bytes {
    let mut buf = Vec::with_capacity(T::default().encoded_len());
    T::default().encode(&mut buf).expect("encode response");
    Bytes::from(buf)
}

/// Returns a precomputed empty response for the given signal type.
///
/// This function uses `OnceLock` to lazily compute and cache the response bytes,
/// avoiding repeated protobuf encoding on each request.
///
/// # Arguments
/// * `signal` - The signal type (Logs, Metrics, or Traces)
///
/// # Returns
/// A static byte slice containing the encoded empty response
#[must_use]
pub fn precomputed_response(signal: SignalType) -> &'static [u8] {
    static LOGS: OnceLock<Bytes> = OnceLock::new();
    static METRICS: OnceLock<Bytes> = OnceLock::new();
    static TRACES: OnceLock<Bytes> = OnceLock::new();

    match signal {
        SignalType::Logs => LOGS
            .get_or_init(encode_response::<ExportLogsServiceResponse>)
            .as_ref(),
        SignalType::Metrics => METRICS
            .get_or_init(encode_response::<ExportMetricsServiceResponse>)
            .as_ref(),
        SignalType::Traces => TRACES
            .get_or_init(encode_response::<ExportTraceServiceResponse>)
            .as_ref(),
    }
}

/// Converts raw bytes into an `OtlpProtoBytes` enum variant based on signal type.
///
/// # Arguments
/// * `signal` - The signal type determining which variant to create
/// * `bytes` - The raw protobuf-encoded request bytes
///
/// # Returns
/// The appropriate `OtlpProtoBytes` variant containing the bytes
#[must_use]
pub fn bytes_to_otlp_proto(signal: SignalType, bytes: Bytes) -> OtlpProtoBytes {
    match signal {
        SignalType::Logs => OtlpProtoBytes::ExportLogsRequest(bytes),
        SignalType::Metrics => OtlpProtoBytes::ExportMetricsRequest(bytes),
        SignalType::Traces => OtlpProtoBytes::ExportTracesRequest(bytes),
    }
}

/// Creates an `OtapPdata` from raw bytes with the appropriate signal type.
///
/// # Arguments
/// * `signal` - The signal type (Logs, Metrics, or Traces)
/// * `bytes` - The raw protobuf-encoded request bytes
/// * `preallocate_frame` - Whether to pre-reserve a context frame for wait_for_result
///
/// # Returns
/// A new `OtapPdata` ready to be sent through the pipeline
#[must_use]
pub fn create_otlp_pdata(signal: SignalType, bytes: Bytes, preallocate_frame: bool) -> OtapPdata {
    let otlp_proto = bytes_to_otlp_proto(signal, bytes);
    let context = if preallocate_frame {
        // Pre-reserve a single frame since wait_for_result uses one slot.
        Context::with_capacity(1)
    } else {
        Context::default()
    };
    OtapPdata::new(context, otlp_proto.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_precomputed_response_logs() {
        let response = precomputed_response(SignalType::Logs);
        // Should return the same reference on subsequent calls (cached)
        let response2 = precomputed_response(SignalType::Logs);
        assert!(std::ptr::eq(response.as_ptr(), response2.as_ptr()));
    }

    #[test]
    fn test_precomputed_response_metrics() {
        let response = precomputed_response(SignalType::Metrics);
        let response2 = precomputed_response(SignalType::Metrics);
        assert!(std::ptr::eq(response.as_ptr(), response2.as_ptr()));
    }

    #[test]
    fn test_precomputed_response_traces() {
        let response = precomputed_response(SignalType::Traces);
        let response2 = precomputed_response(SignalType::Traces);
        assert!(std::ptr::eq(response.as_ptr(), response2.as_ptr()));
    }

    #[test]
    fn test_bytes_to_otlp_proto() {
        let bytes = Bytes::from_static(b"test");

        let logs = bytes_to_otlp_proto(SignalType::Logs, bytes.clone());
        assert!(matches!(logs, OtlpProtoBytes::ExportLogsRequest(_)));

        let metrics = bytes_to_otlp_proto(SignalType::Metrics, bytes.clone());
        assert!(matches!(metrics, OtlpProtoBytes::ExportMetricsRequest(_)));

        let traces = bytes_to_otlp_proto(SignalType::Traces, bytes);
        assert!(matches!(traces, OtlpProtoBytes::ExportTracesRequest(_)));
    }

    #[test]
    fn test_create_otlp_pdata() {
        let bytes = Bytes::from_static(b"test");

        // Without preallocated frame
        let pdata = create_otlp_pdata(SignalType::Logs, bytes.clone(), false);
        // Just verify pdata was created successfully - the payload method
        // returns OtapPayload directly, not Option
        let _payload = pdata.into_parts();

        // With preallocated frame
        let pdata = create_otlp_pdata(SignalType::Metrics, bytes, true);
        let _payload = pdata.into_parts();
    }
}
