// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Versioned framing for canonical OTLP protobuf file output.

use otel_arrow_dfe_config::SignalType;

/// Versioned frame magic (`OTLP Dataflow`, version 1).
pub const OTLP_PROTO_FRAME_MAGIC: [u8; 8] = *b"OTLPDF01";
/// Fixed frame header length.
pub const OTLP_PROTO_FRAME_HEADER_BYTES: usize = 20;

/// Decoded OTLP protobuf frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtlpProtoFrameHeader {
    /// Signal encoded by the payload.
    pub signal: SignalType,
    /// Protobuf payload length, excluding the frame header.
    pub payload_len: usize,
    /// Expected CRC32 checksum of the protobuf payload.
    pub checksum: u32,
}

/// Invalid or unsupported protobuf file framing.
#[derive(Debug, thiserror::Error)]
#[allow(variant_size_differences)]
pub enum OtlpProtoFrameError {
    /// Header magic or reserved bytes do not match version 1.
    #[error("invalid OTLP protobuf frame header")]
    InvalidHeader,
    /// Signal identifier is not supported by this framing version.
    #[error("unsupported OTLP protobuf frame signal code {0}")]
    UnsupportedSignal(u8),
    /// Payload length cannot fit the configured frame bound.
    #[error("OTLP protobuf frame exceeds the configured {max_frame_bytes} byte limit")]
    FrameTooLarge {
        /// Maximum complete frame length.
        max_frame_bytes: usize,
    },
    /// Payload bytes do not match the checksum in the header.
    #[error("OTLP protobuf frame checksum mismatch")]
    ChecksumMismatch,
}

/// Encode one self-identifying, checksummed OTLP protobuf frame.
pub fn encode_otlp_proto_frame(
    signal: SignalType,
    payload: &[u8],
    output: &mut Vec<u8>,
    max_frame_bytes: usize,
) -> Result<(), OtlpProtoFrameError> {
    output.clear();
    let Some(frame_len) = payload.len().checked_add(OTLP_PROTO_FRAME_HEADER_BYTES) else {
        return Err(OtlpProtoFrameError::FrameTooLarge { max_frame_bytes });
    };
    let Ok(payload_len) = u32::try_from(payload.len()) else {
        return Err(OtlpProtoFrameError::FrameTooLarge { max_frame_bytes });
    };
    if frame_len > max_frame_bytes {
        return Err(OtlpProtoFrameError::FrameTooLarge { max_frame_bytes });
    }

    output.extend_from_slice(&OTLP_PROTO_FRAME_MAGIC);
    output.push(signal_code(signal));
    output.extend_from_slice(&[0; 3]);
    output.extend_from_slice(&payload_len.to_be_bytes());
    output.extend_from_slice(&crc32fast::hash(payload).to_be_bytes());
    output.extend_from_slice(payload);
    Ok(())
}

/// Decode and validate a version 1 frame header.
pub fn decode_otlp_proto_frame_header(
    header: &[u8; OTLP_PROTO_FRAME_HEADER_BYTES],
) -> Result<OtlpProtoFrameHeader, OtlpProtoFrameError> {
    if header[..8] != OTLP_PROTO_FRAME_MAGIC || header[9..12] != [0; 3] {
        return Err(OtlpProtoFrameError::InvalidHeader);
    }
    let signal = signal_from_code(header[8])?;
    let payload_len = u32::from_be_bytes([header[12], header[13], header[14], header[15]]) as usize;
    let checksum = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
    Ok(OtlpProtoFrameHeader {
        signal,
        payload_len,
        checksum,
    })
}

/// Validate payload integrity against a decoded frame header.
pub fn validate_otlp_proto_frame_payload(
    header: OtlpProtoFrameHeader,
    payload: &[u8],
) -> Result<(), OtlpProtoFrameError> {
    if payload.len() != header.payload_len {
        return Err(OtlpProtoFrameError::InvalidHeader);
    }
    if crc32fast::hash(payload) != header.checksum {
        return Err(OtlpProtoFrameError::ChecksumMismatch);
    }
    Ok(())
}

const fn signal_code(signal: SignalType) -> u8 {
    match signal {
        SignalType::Logs => 1,
        SignalType::Metrics => 2,
        SignalType::Traces => 3,
        SignalType::Profiles => 4,
    }
}

fn signal_from_code(code: u8) -> Result<SignalType, OtlpProtoFrameError> {
    match code {
        1 => Ok(SignalType::Logs),
        2 => Ok(SignalType::Metrics),
        3 => Ok(SignalType::Traces),
        4 => Ok(SignalType::Profiles),
        _ => Err(OtlpProtoFrameError::UnsupportedSignal(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A Profiles payload is encoded and decoded with versioned framing.
    /// Guarantees: Signal, length, checksum, and payload bytes survive an exact frame round trip.
    #[test]
    fn profiles_frame_round_trip() {
        let mut frame = Vec::new();
        encode_otlp_proto_frame(SignalType::Profiles, b"profiles", &mut frame, 1024).unwrap();
        let header: [u8; OTLP_PROTO_FRAME_HEADER_BYTES] =
            frame[..OTLP_PROTO_FRAME_HEADER_BYTES].try_into().unwrap();
        let header = decode_otlp_proto_frame_header(&header).unwrap();
        let payload = &frame[OTLP_PROTO_FRAME_HEADER_BYTES..];

        assert_eq!(header.signal, SignalType::Profiles);
        assert_eq!(header.payload_len, payload.len());
        validate_otlp_proto_frame_payload(header, payload).unwrap();
        assert_eq!(payload, b"profiles");
    }

    /// Scenario: One payload byte changes after a frame is encoded.
    /// Guarantees: Integrity validation rejects the corrupted frame.
    #[test]
    fn corrupted_payload_is_rejected() {
        let mut frame = Vec::new();
        encode_otlp_proto_frame(SignalType::Profiles, b"profiles", &mut frame, 1024).unwrap();
        *frame.last_mut().unwrap() ^= 0xff;
        let header: [u8; OTLP_PROTO_FRAME_HEADER_BYTES] =
            frame[..OTLP_PROTO_FRAME_HEADER_BYTES].try_into().unwrap();
        let header = decode_otlp_proto_frame_header(&header).unwrap();
        assert!(matches!(
            validate_otlp_proto_frame_payload(header, &frame[OTLP_PROTO_FRAME_HEADER_BYTES..]),
            Err(OtlpProtoFrameError::ChecksumMismatch)
        ));
    }
}
