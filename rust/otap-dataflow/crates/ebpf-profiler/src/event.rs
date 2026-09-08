// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Stable kernel/user event ABI decoding.

use std::mem::{align_of, offset_of, size_of};

use bitflags::bitflags;

use crate::{ProfilerError, Result};

/// Kernel/user ABI version implemented by this crate.
pub const ABI_VERSION: u16 = 1;
/// Fixed byte size of the version 1 event header.
pub const ABI_HEADER_SIZE: usize = 48;
/// Maximum user or kernel frames carried by one version 1 event.
pub const MAX_ABI_STACK_DEPTH: usize = 64;
/// Fixed byte size of one version 1 event including both frame arrays.
pub const ABI_EVENT_SIZE: usize = ABI_HEADER_SIZE + (MAX_ABI_STACK_DEPTH * 2 * size_of::<u64>());
const EVENT_KIND_SAMPLE: u16 = 1;

// Wire layout is asserted independently of RawSample, which is a public model.
#[repr(C, align(8))]
struct WireEvent {
    version: u16,
    header_size: u16,
    event_kind: u16,
    flags: u16,
    pid: u32,
    tid: u32,
    cpu: u32,
    reserved0: u32,
    timestamp_ns: u64,
    user_depth: u16,
    kernel_depth: u16,
    user_stack_error: i32,
    kernel_stack_error: i32,
    reserved1: u32,
    frames: [u64; MAX_ABI_STACK_DEPTH * 2],
}

const _: () = assert!(size_of::<WireEvent>() == ABI_EVENT_SIZE);
const _: () = assert!(align_of::<WireEvent>() == 8);
const _: () = assert!(offset_of!(WireEvent, frames) == ABI_HEADER_SIZE);
const _: () = assert!(offset_of!(WireEvent, timestamp_ns) == 24);

bitflags! {
    /// Stable flags emitted by the kernel sampler.
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub struct EventFlags: u16 {
        /// The user stack filled the ABI capacity and may be truncated.
        const USER_TRUNCATED = 1 << 0;
        /// The kernel stack filled the ABI capacity and may be truncated.
        const KERNEL_TRUNCATED = 1 << 1;
        /// User-stack collection returned an error.
        const USER_STACK_ERROR = 1 << 2;
        /// Kernel-stack collection returned an error.
        const KERNEL_STACK_ERROR = 1 << 3;
    }
}

/// A decoded, fixed-capacity on-CPU sample.
#[derive(Clone, Eq, PartialEq)]
pub struct RawSample {
    /// Process identifier in the profiler's procfs namespace.
    pub pid: u32,
    /// Thread identifier in the profiler's procfs namespace.
    pub tid: u32,
    /// Logical CPU on which the sample was captured.
    pub cpu: u32,
    /// Kernel CLOCK_BOOTTIME timestamp in nanoseconds, including suspend time.
    pub timestamp_ns: u64,
    /// Stable collection and truncation flags.
    pub flags: EventFlags,
    /// User-stack helper return code.
    pub user_stack_error: i32,
    /// Kernel-stack helper return code.
    pub kernel_stack_error: i32,
    /// Valid user-frame count.
    pub user_depth: u16,
    /// Valid kernel-frame count.
    pub kernel_depth: u16,
    /// Fixed-capacity user instruction addresses.
    pub user_frames: [u64; MAX_ABI_STACK_DEPTH],
    /// Fixed-capacity kernel instruction addresses.
    pub kernel_frames: [u64; MAX_ABI_STACK_DEPTH],
}

impl std::fmt::Debug for RawSample {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawSample")
            .field("pid", &self.pid)
            .field("tid", &self.tid)
            .field("cpu", &self.cpu)
            .field("timestamp_ns", &self.timestamp_ns)
            .field("flags", &self.flags)
            .field("user_stack_error", &self.user_stack_error)
            .field("kernel_stack_error", &self.kernel_stack_error)
            .field("user_frames", &self.user_stack())
            .field("kernel_frames", &self.kernel_stack())
            .finish()
    }
}

impl RawSample {
    /// Returns the valid user instruction addresses.
    #[must_use]
    pub fn user_stack(&self) -> &[u64] {
        &self.user_frames[..usize::from(self.user_depth).min(MAX_ABI_STACK_DEPTH)]
    }

    /// Returns the valid kernel instruction addresses.
    #[must_use]
    pub fn kernel_stack(&self) -> &[u64] {
        &self.kernel_frames[..usize::from(self.kernel_depth).min(MAX_ABI_STACK_DEPTH)]
    }

    /// Checks public fields before accepting a sample into aggregation.
    pub fn validate(&self) -> Result<()> {
        if EventFlags::from_bits(self.flags.bits()).is_none() {
            return Err(ProfilerError::AbiMismatch(
                "unknown sample flags".to_owned(),
            ));
        }
        for (depth, error, failed, truncated, frames) in [
            (
                self.user_depth,
                self.user_stack_error,
                self.flags.contains(EventFlags::USER_STACK_ERROR),
                self.flags.contains(EventFlags::USER_TRUNCATED),
                &self.user_frames,
            ),
            (
                self.kernel_depth,
                self.kernel_stack_error,
                self.flags.contains(EventFlags::KERNEL_STACK_ERROR),
                self.flags.contains(EventFlags::KERNEL_TRUNCATED),
                &self.kernel_frames,
            ),
        ] {
            let depth = usize::from(depth);
            if depth > MAX_ABI_STACK_DEPTH
                || failed != (error < 0)
                || error > 0
                || (failed && (depth != 0 || truncated))
                || (truncated && depth != MAX_ABI_STACK_DEPTH)
            {
                return Err(ProfilerError::AbiMismatch(
                    "inconsistent stack flags, depth, or helper error".to_owned(),
                ));
            }
            if frames[depth..].iter().any(|frame| *frame != 0) {
                return Err(ProfilerError::AbiMismatch(
                    "non-zero unused stack slots".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Decodes one versioned sample without retaining references to the input.
pub fn decode_event(bytes: &[u8]) -> Result<RawSample> {
    if bytes.len() < ABI_HEADER_SIZE {
        return Err(ProfilerError::AbiMismatch(format!(
            "truncated header: {} bytes",
            bytes.len()
        )));
    }
    let version = read_u16(bytes, 0)?;
    if version != ABI_VERSION {
        return Err(ProfilerError::AbiMismatch(format!(
            "expected version {ABI_VERSION}, got {version}"
        )));
    }
    let header_size = usize::from(read_u16(bytes, 2)?);
    if header_size != ABI_HEADER_SIZE {
        return Err(ProfilerError::AbiMismatch(format!(
            "expected header size {ABI_HEADER_SIZE}, got {header_size}"
        )));
    }
    let event_kind = read_u16(bytes, 4)?;
    if event_kind != EVENT_KIND_SAMPLE {
        return Err(ProfilerError::AbiMismatch(format!(
            "unsupported event kind {event_kind}"
        )));
    }
    let raw_flags = read_u16(bytes, 6)?;
    let flags = EventFlags::from_bits(raw_flags).ok_or_else(|| {
        ProfilerError::AbiMismatch(format!("unknown event flags 0x{raw_flags:04x}"))
    })?;
    let user_depth = usize::from(read_u16(bytes, 32)?);
    let kernel_depth = usize::from(read_u16(bytes, 34)?);
    if user_depth > MAX_ABI_STACK_DEPTH || kernel_depth > MAX_ABI_STACK_DEPTH {
        return Err(ProfilerError::AbiMismatch(format!(
            "stack depth exceeds {MAX_ABI_STACK_DEPTH}"
        )));
    }
    if bytes.len() != ABI_EVENT_SIZE {
        return Err(ProfilerError::AbiMismatch(format!(
            "expected {ABI_EVENT_SIZE} bytes, got {}",
            bytes.len()
        )));
    }
    if read_u32(bytes, 20)? != 0 || read_u32(bytes, 44)? != 0 {
        return Err(ProfilerError::AbiMismatch(
            "non-zero reserved fields".to_owned(),
        ));
    }

    let mut user_frames = [0_u64; MAX_ABI_STACK_DEPTH];
    let mut kernel_frames = [0_u64; MAX_ABI_STACK_DEPTH];
    let mut offset = ABI_HEADER_SIZE;
    for frame in &mut user_frames {
        *frame = read_u64(bytes, offset)?;
        offset += 8;
    }
    offset = ABI_HEADER_SIZE + (MAX_ABI_STACK_DEPTH * size_of::<u64>());
    for frame in &mut kernel_frames {
        *frame = read_u64(bytes, offset)?;
        offset += 8;
    }

    let sample = RawSample {
        pid: read_u32(bytes, 8)?,
        tid: read_u32(bytes, 12)?,
        cpu: read_u32(bytes, 16)?,
        timestamp_ns: read_u64(bytes, 24)?,
        flags,
        user_stack_error: read_i32(bytes, 36)?,
        kernel_stack_error: read_i32(bytes, 40)?,
        user_depth: u16::try_from(user_depth)
            .map_err(|_| ProfilerError::AbiMismatch("invalid user depth".to_owned()))?,
        kernel_depth: u16::try_from(kernel_depth)
            .map_err(|_| ProfilerError::AbiMismatch("invalid kernel depth".to_owned()))?,
        user_frames,
        kernel_frames,
    };
    sample.validate()?;
    Ok(sample)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| ProfilerError::AbiMismatch("truncated u16".to_owned()))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| ProfilerError::AbiMismatch("truncated u32".to_owned()))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| ProfilerError::AbiMismatch("truncated i32".to_owned()))?;
    Ok(i32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| ProfilerError::AbiMismatch("truncated u64".to_owned()))?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

/// Encodes one sample using the stable version 1 ABI.
///
/// This is primarily used by deterministic event sources, ABI fixtures, and
/// comparison tooling. The kernel program writes the same layout directly.
#[must_use]
pub fn encode_event(sample: &RawSample) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ABI_EVENT_SIZE);
    bytes.extend_from_slice(&ABI_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(ABI_HEADER_SIZE as u16).to_le_bytes());
    bytes.extend_from_slice(&EVENT_KIND_SAMPLE.to_le_bytes());
    bytes.extend_from_slice(&sample.flags.bits().to_le_bytes());
    bytes.extend_from_slice(&sample.pid.to_le_bytes());
    bytes.extend_from_slice(&sample.tid.to_le_bytes());
    bytes.extend_from_slice(&sample.cpu.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&sample.timestamp_ns.to_le_bytes());
    bytes.extend_from_slice(&sample.user_depth.to_le_bytes());
    bytes.extend_from_slice(&sample.kernel_depth.to_le_bytes());
    bytes.extend_from_slice(&sample.user_stack_error.to_le_bytes());
    bytes.extend_from_slice(&sample.kernel_stack_error.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    for frame in &sample.user_frames {
        bytes.extend_from_slice(&frame.to_le_bytes());
    }
    for frame in &sample.kernel_frames {
        bytes.extend_from_slice(&frame.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RawSample {
        let mut user_frames = [0; MAX_ABI_STACK_DEPTH];
        user_frames[..2].copy_from_slice(&[0x1000, 0x2000]);
        RawSample {
            pid: 10,
            tid: 11,
            cpu: 2,
            timestamp_ns: 123,
            flags: EventFlags::KERNEL_STACK_ERROR,
            user_stack_error: 0,
            kernel_stack_error: -14,
            user_depth: 2,
            kernel_depth: 0,
            user_frames,
            kernel_frames: [0; MAX_ABI_STACK_DEPTH],
        }
    }

    /// Scenario: A valid little-endian version 1 event is decoded.
    /// Guarantees: Fixed-width fields and frame ordering cross the ABI intact.
    #[test]
    fn valid_event_decodes() {
        let expected = sample();
        let decoded = decode_event(&encode_event(&expected)).expect("event should decode");
        assert_eq!(decoded, expected);
    }

    /// Scenario: A kernel event advertises an unknown ABI version.
    /// Guarantees: Userspace rejects incompatible layouts before reading frames.
    #[test]
    fn wrong_version_is_rejected() {
        let mut bytes = encode_event(&sample());
        bytes[0] = 2;
        assert!(decode_event(&bytes).is_err());
    }

    /// Scenario: A perf record is shorter than the fixed ABI header.
    /// Guarantees: Malformed kernel input returns a typed error without panic.
    #[test]
    fn truncated_event_is_rejected() {
        assert!(decode_event(&[0; ABI_HEADER_SIZE - 1]).is_err());
    }

    /// Scenario: A perf record contains bytes beyond its advertised frame data.
    /// Guarantees: Ambiguous or mismatched event sizes are rejected.
    #[test]
    fn oversized_event_is_rejected() {
        let mut bytes = encode_event(&sample());
        bytes.push(0);
        assert!(decode_event(&bytes).is_err());
    }

    /// Scenario: A kernel event sets an unassigned flag bit.
    /// Guarantees: New semantics cannot be silently misinterpreted by old code.
    #[test]
    fn invalid_flags_are_rejected() {
        let mut bytes = encode_event(&sample());
        bytes[6..8].copy_from_slice(&0x8000_u16.to_le_bytes());
        assert!(decode_event(&bytes).is_err());
    }

    /// Scenario: Event bytes begin at an unaligned address in a larger buffer.
    /// Guarantees: Decoding uses checked byte operations rather than aligned casts.
    #[test]
    fn unaligned_bytes_decode() {
        let encoded = encode_event(&sample());
        let mut outer = vec![0];
        outer.extend_from_slice(&encoded);
        assert!(decode_event(&outer[1..]).is_ok());
    }

    /// Scenario: Callers construct a sample with an out-of-range public depth.
    /// Guarantees: Accessors and Debug cannot panic, and validation rejects it.
    #[test]
    fn malformed_public_fields_are_safe() {
        let mut value = sample();
        value.user_depth = u16::MAX;
        value.kernel_depth = u16::MAX;
        assert_eq!(value.user_stack().len(), MAX_ABI_STACK_DEPTH);
        assert_eq!(value.kernel_stack().len(), MAX_ABI_STACK_DEPTH);
        assert!(!format!("{value:?}").is_empty());
        assert!(value.validate().is_err());
    }

    /// Scenario: Reserved fields, ignored frames, and stack error flags disagree.
    /// Guarantees: Malformed wire data is rejected rather than silently normalized.
    #[test]
    fn invalid_semantics_are_rejected() {
        for offset in [20, 44, ABI_HEADER_SIZE + 16] {
            let mut bytes = encode_event(&sample());
            bytes[offset] = 1;
            assert!(decode_event(&bytes).is_err());
        }
        let mut value = sample();
        value.flags = EventFlags::empty();
        assert!(value.validate().is_err());
        value.kernel_stack_error = 0;
        value.flags = EventFlags::USER_TRUNCATED;
        assert!(value.validate().is_err());
    }

    /// Scenario: A native C producer emits the shared C layout independently.
    /// Guarantees: Generated fixture bytes decode without relying on Rust encoding.
    #[test]
    fn independently_generated_c_golden() {
        let Some(path) = std::env::var_os("OTEL_EBPF_PROFILER_GOLDEN") else {
            return;
        };
        let bytes = std::fs::read(path).expect("C golden fixture should exist");
        let value = decode_event(&bytes).expect("C ABI should match the Rust decoder");
        assert_eq!(
            (value.pid, value.tid, value.cpu),
            (0x12345678, 0x23456789, 37)
        );
        assert_eq!(value.timestamp_ns, 0x0102030405060708);
        assert_eq!(
            value.user_stack(),
            &[0x1122334455667788, 0x8877665544332211]
        );
        assert_eq!(value.kernel_stack_error, -14);
        assert_eq!(value.flags, EventFlags::KERNEL_STACK_ERROR);
    }
}
