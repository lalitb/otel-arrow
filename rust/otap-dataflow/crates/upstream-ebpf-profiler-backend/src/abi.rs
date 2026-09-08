// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Checked decoding for the upstream kernel/userspace ABI.

use serde::{Deserialize, Serialize};
use std::mem::size_of;

/// Maximum number of 64-bit frame words in one upstream trace.
pub const MAX_FRAME_WORDS: usize = 3072;
/// Fixed bytes before the variable frame-data suffix.
pub const TRACE_PREFIX_SIZE: usize = 728;
/// Maximum encoded trace size.
pub const TRACE_MAX_SIZE: usize = TRACE_PREFIX_SIZE + MAX_FRAME_WORDS * 8;
/// Maximum number of upstream custom labels.
pub const MAX_CUSTOM_LABELS: usize = 10;

const PID_OFFSET: usize = 0;
const TID_OFFSET: usize = 4;
const KTIME_OFFSET: usize = 8;
const COMM_OFFSET: usize = 16;
const APM_TRANSACTION_ID_OFFSET: usize = 32;
const APM_TRACE_ID_OFFSET: usize = 40;
const CUSTOM_LABEL_COUNT_OFFSET: usize = 56;
const CUSTOM_LABELS_OFFSET: usize = 60;
const CUSTOM_LABEL_SIZE: usize = 64;
const FRAME_DATA_LEN_OFFSET: usize = 700;
const NUM_FRAMES_OFFSET: usize = 702;
const NUM_KERNEL_FRAMES_OFFSET: usize = 704;
const ORIGIN_OFFSET: usize = 706;
const VALUE_OFFSET: usize = 712;
const CPU_ID_OFFSET: usize = 720;

/// A decoded report-events notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationEvent {
    /// The PID map contains work for userspace.
    GenericPid,
    /// A future event type not understood by this revision.
    Unknown(u32),
}

/// Frame type decoded from the high nibble of a frame header.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// Unknown or critical-error frame.
    Unknown,
    /// Python frame.
    Python,
    /// PHP frame.
    Php,
    /// Native ELF frame.
    Native,
    /// Kernel address frame.
    Kernel,
    /// Java HotSpot frame.
    Hotspot,
    /// Ruby frame.
    Ruby,
    /// Perl frame.
    Perl,
    /// V8 frame.
    V8,
    /// PHP JIT frame.
    PhpJit,
    /// .NET frame.
    Dotnet,
    /// Go frame.
    Go,
    /// Erlang BEAM frame.
    Beam,
    /// LuaJIT frame.
    LuaJit,
    /// A future marker value.
    Other(u8),
}

impl From<u8> for FrameKind {
    fn from(value: u8) -> Self {
        match value {
            0x0 => Self::Unknown,
            0x1 => Self::Python,
            0x2 => Self::Php,
            0x3 => Self::Native,
            0x4 => Self::Kernel,
            0x5 => Self::Hotspot,
            0x6 => Self::Ruby,
            0x7 => Self::Perl,
            0x8 => Self::V8,
            0x9 => Self::PhpJit,
            0xa => Self::Dotnet,
            0xb => Self::Go,
            0xc => Self::Beam,
            0xd => Self::LuaJit,
            other => Self::Other(other),
        }
    }
}

/// Typed frame flags from the upstream header.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FrameFlags(u8);

impl FrameFlags {
    /// Error-frame bit.
    pub const ERROR: u8 = 1 << 0;
    /// Return-address bit.
    pub const RETURN_ADDRESS: u8 = 1 << 1;
    /// PID-specific cache-key bit.
    pub const PID_SPECIFIC: u8 = 1 << 2;

    /// Creates flags while retaining unknown future bits.
    #[must_use]
    pub const fn from_bits_retain(bits: u8) -> Self {
        Self(bits & 0x0f)
    }

    /// Returns all four ABI flag bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns whether this is an error frame.
    #[must_use]
    pub const fn is_error(self) -> bool {
        self.0 & Self::ERROR != 0
    }

    /// Returns whether the frame PC is a return address.
    #[must_use]
    pub const fn is_return_address(self) -> bool {
        self.0 & Self::RETURN_ADDRESS != 0
    }
}

/// One checked variable-length frame.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct DecodedFrame {
    /// Frame marker.
    pub kind: FrameKind,
    /// Frame flags, including unknown future bits.
    pub flags: FrameFlags,
    /// 52-bit type-specific header data.
    pub data: u64,
    /// Owned words following the header.
    pub variables: Vec<u64>,
}

/// One bounded custom label from the fixed upstream label array.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CustomLabel {
    /// Label key with trailing NUL bytes removed.
    pub key: Vec<u8>,
    /// Label value with trailing NUL bytes removed.
    pub value: Vec<u8>,
}

/// One owned trace record decoded from ring-buffer memory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RawTrace {
    /// Process ID.
    pub pid: u32,
    /// Thread ID.
    pub tid: u32,
    /// Kernel monotonic timestamp.
    pub ktime_ns: u64,
    /// Linux task command with trailing NUL bytes removed.
    pub comm: Vec<u8>,
    /// Upstream APM transaction/span ID bytes.
    pub apm_transaction_id: [u8; 8],
    /// Upstream APM trace ID bytes.
    pub apm_trace_id: [u8; 16],
    /// Bounded custom labels.
    pub custom_labels: Vec<CustomLabel>,
    /// Origin ID configured for the sampling entrypoint.
    pub origin: u16,
    /// Probe-defined sample value.
    pub value: u64,
    /// CPU that completed the trace.
    pub cpu_id: u32,
    /// Raw leading kernel instruction pointers.
    pub kernel_frames: Vec<u64>,
    /// Checked variable-length userspace frames.
    pub user_frames: Vec<DecodedFrame>,
}

/// ABI decoder failures.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AbiError {
    /// A fixed header was truncated.
    #[error("record length {actual} is smaller than required prefix {minimum}")]
    TruncatedRecord {
        /// Actual byte count.
        actual: usize,
        /// Minimum byte count.
        minimum: usize,
    },
    /// The record exceeds the upstream fixed maximum.
    #[error("record length {actual} exceeds maximum {maximum}")]
    OversizedRecord {
        /// Actual byte count.
        actual: usize,
        /// Maximum byte count.
        maximum: usize,
    },
    /// Frame-data words exceed the bytes supplied.
    #[error("frame words {words} require {required} bytes, record has {actual}")]
    TruncatedFrames {
        /// Claimed frame words.
        words: usize,
        /// Required total record length.
        required: usize,
        /// Actual record length.
        actual: usize,
    },
    /// A custom-label count exceeded its fixed array.
    #[error("custom label count {actual} exceeds {maximum}")]
    CustomLabelCount {
        /// Claimed count.
        actual: usize,
        /// Fixed maximum.
        maximum: usize,
    },
    /// A frame header declared an invalid word count.
    #[error("frame at word {word} has invalid length {length} with {remaining} words remaining")]
    InvalidFrameLength {
        /// Header word index in the user-frame suffix.
        word: usize,
        /// Declared frame word count.
        length: usize,
        /// Words remaining at this header.
        remaining: usize,
    },
    /// More frames were decoded than the configured bound.
    #[error("decoded frame count exceeds configured maximum {maximum}")]
    FrameCapacity {
        /// Configured maximum.
        maximum: usize,
    },
    /// Kernel-frame count exceeded the entire frame suffix.
    #[error("kernel frame count {kernel} exceeds frame word count {total}")]
    KernelFrameCount {
        /// Claimed kernel words.
        kernel: usize,
        /// Total frame words.
        total: usize,
    },
    /// Header user-frame count disagreed with checked frame decoding.
    #[error("header reports {declared} user frames but decoded {decoded}")]
    FrameCountMismatch {
        /// Header count.
        declared: usize,
        /// Checked count.
        decoded: usize,
    },
}

/// Decodes a four-byte report-events record without alignment assumptions.
pub fn decode_notification(bytes: &[u8]) -> Result<NotificationEvent, AbiError> {
    if bytes.len() < 4 {
        return Err(AbiError::TruncatedRecord {
            actual: bytes.len(),
            minimum: 4,
        });
    }
    Ok(match read_u32(bytes, 0) {
        1 => NotificationEvent::GenericPid,
        other => NotificationEvent::Unknown(other),
    })
}

/// Decodes one upstream trace from arbitrary byte alignment.
pub fn decode_trace(bytes: &[u8], max_frames: usize) -> Result<RawTrace, AbiError> {
    if bytes.len() < TRACE_PREFIX_SIZE {
        return Err(AbiError::TruncatedRecord {
            actual: bytes.len(),
            minimum: TRACE_PREFIX_SIZE,
        });
    }
    if bytes.len() > TRACE_MAX_SIZE + 7 {
        return Err(AbiError::OversizedRecord {
            actual: bytes.len(),
            maximum: TRACE_MAX_SIZE + 7,
        });
    }

    let frame_words = usize::from(read_u16(bytes, FRAME_DATA_LEN_OFFSET));
    if frame_words > MAX_FRAME_WORDS {
        return Err(AbiError::OversizedRecord {
            actual: frame_words,
            maximum: MAX_FRAME_WORDS,
        });
    }
    let required = TRACE_PREFIX_SIZE + frame_words * 8;
    if bytes.len() < required {
        return Err(AbiError::TruncatedFrames {
            words: frame_words,
            required,
            actual: bytes.len(),
        });
    }
    let kernel_count = usize::from(read_u16(bytes, NUM_KERNEL_FRAMES_OFFSET));
    if kernel_count > frame_words {
        return Err(AbiError::KernelFrameCount {
            kernel: kernel_count,
            total: frame_words,
        });
    }
    let declared = usize::from(read_u16(bytes, NUM_FRAMES_OFFSET));
    if kernel_count.saturating_add(declared) > max_frames {
        return Err(AbiError::FrameCapacity {
            maximum: max_frames,
        });
    }

    let custom_label_count = read_u32(bytes, CUSTOM_LABEL_COUNT_OFFSET) as usize;
    if custom_label_count > MAX_CUSTOM_LABELS {
        return Err(AbiError::CustomLabelCount {
            actual: custom_label_count,
            maximum: MAX_CUSTOM_LABELS,
        });
    }
    let mut custom_labels = Vec::with_capacity(custom_label_count);
    for index in 0..custom_label_count {
        let offset = CUSTOM_LABELS_OFFSET + index * CUSTOM_LABEL_SIZE;
        custom_labels.push(CustomLabel {
            key: trim_nul(&bytes[offset..offset + 16]).to_vec(),
            value: trim_nul(&bytes[offset + 16..offset + 64]).to_vec(),
        });
    }

    let mut words = Vec::with_capacity(frame_words);
    for index in 0..frame_words {
        words.push(read_u64(bytes, TRACE_PREFIX_SIZE + index * 8));
    }
    let kernel_frames = words[..kernel_count].to_vec();
    let user_words = &words[kernel_count..];
    let mut user_frames = Vec::new();
    let mut index = 0;
    while index < user_words.len() {
        if kernel_count + user_frames.len() >= max_frames {
            return Err(AbiError::FrameCapacity {
                maximum: max_frames,
            });
        }
        let header = user_words[index];
        let length = ((header >> 52) & 0x0f) as usize;
        let remaining = user_words.len() - index;
        if length == 0 || length > remaining {
            return Err(AbiError::InvalidFrameLength {
                word: index,
                length,
                remaining,
            });
        }
        user_frames.push(DecodedFrame {
            kind: FrameKind::from((header >> 60) as u8),
            flags: FrameFlags::from_bits_retain(((header >> 56) & 0x0f) as u8),
            data: header & ((1_u64 << 52) - 1),
            variables: user_words[index + 1..index + length].to_vec(),
        });
        index += length;
    }
    if declared != user_frames.len() {
        return Err(AbiError::FrameCountMismatch {
            declared,
            decoded: user_frames.len(),
        });
    }

    Ok(RawTrace {
        pid: read_u32(bytes, PID_OFFSET),
        tid: read_u32(bytes, TID_OFFSET),
        ktime_ns: read_u64(bytes, KTIME_OFFSET),
        comm: trim_nul(&bytes[COMM_OFFSET..COMM_OFFSET + 16]).to_vec(),
        apm_transaction_id: read_array(bytes, APM_TRANSACTION_ID_OFFSET),
        apm_trace_id: read_array(bytes, APM_TRACE_ID_OFFSET),
        custom_labels,
        origin: read_u16(bytes, ORIGIN_OFFSET),
        value: read_u64(bytes, VALUE_OFFSET),
        cpu_id: read_u32(bytes, CPU_ID_OFFSET),
        kernel_frames,
        user_frames,
    })
}

fn trim_nul(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    let mut result = [0; N];
    result.copy_from_slice(&bytes[offset..offset + N]);
    result
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(read_array(bytes, offset))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(read_array(bytes, offset))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(read_array(bytes, offset))
}

#[repr(C)]
struct CustomLabelLayout {
    key: [u8; 16],
    value: [u8; 48],
}

#[repr(C)]
struct CustomLabelsLayout {
    len: u32,
    labels: [CustomLabelLayout; MAX_CUSTOM_LABELS],
}

#[repr(C)]
struct TraceLayout {
    pid: u32,
    tid: u32,
    ktime: u64,
    comm: [u8; 16],
    apm_transaction_id: [u8; 8],
    apm_trace_id: [u8; 16],
    custom_labels: CustomLabelsLayout,
    frame_data_len: u16,
    num_frames: u16,
    num_kernel_frames: u16,
    origin: u16,
    value: u64,
    cpu_id: u32,
    frame_data: [u64; MAX_FRAME_WORDS],
}

const _: () = {
    assert!(size_of::<CustomLabelLayout>() == 64);
    assert!(size_of::<CustomLabelsLayout>() == 644);
    assert!(std::mem::offset_of!(TraceLayout, frame_data_len) == FRAME_DATA_LEN_OFFSET);
    assert!(std::mem::offset_of!(TraceLayout, num_frames) == NUM_FRAMES_OFFSET);
    assert!(std::mem::offset_of!(TraceLayout, num_kernel_frames) == NUM_KERNEL_FRAMES_OFFSET);
    assert!(std::mem::offset_of!(TraceLayout, origin) == ORIGIN_OFFSET);
    assert!(std::mem::offset_of!(TraceLayout, value) == VALUE_OFFSET);
    assert!(std::mem::offset_of!(TraceLayout, cpu_id) == CPU_ID_OFFSET);
    assert!(std::mem::offset_of!(TraceLayout, frame_data) == TRACE_PREFIX_SIZE);
    assert!(size_of::<TraceLayout>() == TRACE_MAX_SIZE);
};

#[cfg(test)]
mod tests {
    use super::*;

    fn trace_bytes(header: u64, variables: &[u64], marker_count: u16) -> Vec<u8> {
        let words = 1 + variables.len();
        let mut bytes = vec![0_u8; TRACE_PREFIX_SIZE + words * 8];
        bytes[PID_OFFSET..PID_OFFSET + 4].copy_from_slice(&7_u32.to_le_bytes());
        bytes[TID_OFFSET..TID_OFFSET + 4].copy_from_slice(&8_u32.to_le_bytes());
        bytes[FRAME_DATA_LEN_OFFSET..FRAME_DATA_LEN_OFFSET + 2]
            .copy_from_slice(&(words as u16).to_le_bytes());
        bytes[NUM_FRAMES_OFFSET..NUM_FRAMES_OFFSET + 2]
            .copy_from_slice(&marker_count.to_le_bytes());
        bytes[TRACE_PREFIX_SIZE..TRACE_PREFIX_SIZE + 8].copy_from_slice(&header.to_le_bytes());
        for (index, value) in variables.iter().enumerate() {
            let offset = TRACE_PREFIX_SIZE + (index + 1) * 8;
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    /// Scenario: A native frame arrives at an address that is not aligned for
    /// direct Rust struct access.
    /// Guarantees: Byte-wise little-endian decoding produces the owned frame
    /// without an unaligned cast or borrowed ring-buffer memory.
    #[test]
    fn unaligned_native_record_decodes() {
        let header = (0x3_u64 << 60) | (0x2_u64 << 52) | 0x1234;
        let trace = trace_bytes(header, &[0xfeed], 1);
        let mut unaligned = vec![0xff];
        unaligned.extend_from_slice(&trace);
        let decoded = decode_trace(&unaligned[1..], 4).expect("valid trace");
        assert_eq!(decoded.pid, 7);
        assert_eq!(decoded.user_frames[0].kind, FrameKind::Native);
        assert_eq!(decoded.user_frames[0].variables, [0xfeed]);
    }

    /// Scenario: A ring-buffer record ends before the fixed trace prefix.
    /// Guarantees: The decoder returns a typed truncation error and never
    /// indexes or casts beyond supplied memory.
    #[test]
    fn truncated_record_is_rejected() {
        assert!(matches!(
            decode_trace(&[0; 32], 4),
            Err(AbiError::TruncatedRecord { .. })
        ));
    }

    /// Scenario: A user frame declares a zero-word length.
    /// Guarantees: Malformed variable-length encoding is rejected without a
    /// non-progressing loop.
    #[test]
    fn zero_length_frame_is_rejected() {
        let bytes = trace_bytes(0x3_u64 << 60, &[], 1);
        assert!(matches!(
            decode_trace(&bytes, 4),
            Err(AbiError::InvalidFrameLength { length: 0, .. })
        ));
    }

    /// Scenario: A future frame marker is received with otherwise valid data.
    /// Guarantees: Unknown frame kinds remain representable and do not panic or
    /// corrupt following frames.
    #[test]
    fn unknown_frame_type_is_retained() {
        let header = (0xf_u64 << 60) | (0x1_u64 << 52);
        let decoded = decode_trace(&trace_bytes(header, &[], 1), 4).expect("valid trace");
        assert_eq!(decoded.user_frames[0].kind, FrameKind::Other(0xf));
    }

    /// Scenario: The perf notification map emits both the known PID event and a
    /// future selector.
    /// Guarantees: Known and unknown event types are decoded without panic.
    #[test]
    fn notification_types_are_total() {
        assert_eq!(
            decode_notification(&1_u32.to_le_bytes()).expect("known event"),
            NotificationEvent::GenericPid
        );
        assert_eq!(
            decode_notification(&99_u32.to_le_bytes()).expect("future event"),
            NotificationEvent::Unknown(99)
        );
    }

    /// Scenario: A trace contains only kernel frames and exceeds the configured frame limit.
    /// Guarantees: The limit is checked before allocating or copying the frame vectors.
    #[test]
    fn kernel_frames_are_part_of_frame_capacity() {
        let mut bytes = trace_bytes(1, &[2], 0);
        bytes[NUM_KERNEL_FRAMES_OFFSET..NUM_KERNEL_FRAMES_OFFSET + 2]
            .copy_from_slice(&2_u16.to_le_bytes());
        assert!(matches!(
            decode_trace(&bytes, 1),
            Err(AbiError::FrameCapacity { maximum: 1 })
        ));
        assert_eq!(
            decode_trace(&bytes, 2)
                .expect("two kernel frames")
                .kernel_frames,
            [1, 2]
        );
    }

    /// Scenario: Every truncation and each one-byte corruption of a small valid record is decoded.
    /// Guarantees: External lengths and markers cannot panic, loop forever, or escape frame bounds.
    #[test]
    fn truncated_and_corrupted_records_do_not_panic() {
        let bytes = trace_bytes((3_u64 << 60) | (2_u64 << 52) | 0x1234, &[0xfeed], 1);
        for end in 0..bytes.len() {
            assert!(decode_trace(&bytes[..end], 4).is_err());
        }
        for index in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[index] = 0xff;
            if let Ok(trace) = decode_trace(&changed, 4) {
                assert!(trace.user_frames.len() + trace.kernel_frames.len() <= 4);
            }
        }
    }
}
