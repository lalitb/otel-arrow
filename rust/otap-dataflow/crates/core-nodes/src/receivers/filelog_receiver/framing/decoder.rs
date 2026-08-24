// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded streaming decoding for filelog framing.

use super::super::{Encoding, OnDecodeError};
use std::{fmt, str};
use thiserror::Error;

/// A half-open range of offsets in the source byte stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceRange {
    /// The first source byte in the range.
    pub start: u64,
    /// The first source byte after the range.
    pub end: u64,
}

/// The exact source bytes for one decoded or malformed source unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceBytes {
    bytes: [u8; 4],
    len: u8,
}

impl SourceBytes {
    fn from_slice(source: &[u8]) -> Self {
        debug_assert!(source.len() <= 4);
        let mut bytes = [0; 4];
        bytes[..source.len()].copy_from_slice(source);
        Self {
            bytes,
            len: source.len() as u8,
        }
    }

    fn from_pair(first: &[u8], second: &[u8]) -> Self {
        debug_assert!(first.len() + second.len() <= 4);
        let mut bytes = [0; 4];
        bytes[..first.len()].copy_from_slice(first);
        bytes[first.len()..first.len() + second.len()].copy_from_slice(second);
        Self {
            bytes,
            len: (first.len() + second.len()) as u8,
        }
    }

    /// Returns the exact source bytes represented by this value.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl fmt::Display for SourceBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.as_slice() {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The decoded shadow value for one source unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(variant_size_differences)]
pub enum DecodedValue {
    /// A decoded Unicode scalar.
    Scalar(char),
    /// One byte from raw mode.
    RawByte(u8),
}

/// One source-ordered decoder event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeEvent {
    /// A decoded source unit, including its exact source evidence.
    Unit {
        /// The source range occupied by the unit.
        range: SourceRange,
        /// The exact bytes occupied by the unit.
        source: SourceBytes,
        /// The decoded value, or U+FFFD for a malformed text unit.
        value: DecodedValue,
        /// Whether the source unit was malformed.
        malformed: bool,
    },
    /// A matching byte-order mark stripped at the start of a new stream.
    StrippedBom {
        /// The source range occupied by the stripped byte-order mark.
        range: SourceRange,
    },
}

impl DecodeEvent {
    const fn range(self) -> SourceRange {
        match self {
            Self::Unit { range, .. } | Self::StrippedBom { range } => range,
        }
    }
}

/// Progress and at most one event produced by a decoder call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeStep {
    /// Bytes consumed from the input slice supplied to this call.
    pub consumed: usize,
    /// The next source-ordered event, when one became available.
    pub event: Option<DecodeEvent>,
}

/// A fatal streaming decoder error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DecodeError {
    /// The caller did not supply the next expected source offset.
    #[error("decoder input offset discontinuity: expected {expected}, got {actual}")]
    OffsetDiscontinuity {
        /// The next offset expected by the decoder.
        expected: u64,
        /// The offset supplied by the caller.
        actual: u64,
    },
    /// A source range could not be represented by `u64` offsets.
    #[error("source offset overflow")]
    SourceOffsetOverflow,
    /// The configured fail policy rejected an exact malformed source unit.
    #[error("malformed source unit at {range:?}: {source_bytes}")]
    FatalMalformed {
        /// The exact range of the malformed source unit.
        range: SourceRange,
        /// The exact bytes of the malformed source unit.
        source_bytes: SourceBytes,
    },
}

/// A constant-state, one-event-at-a-time decoder for a single source stream.
#[derive(Debug)]
pub struct StreamDecoder {
    policy: OnDecodeError,
    state: DecoderState,
    bom_probe: Option<BomProbe>,
    replay: ReplayBytes,
    next_input_offset: u64,
    delivered_boundary: u64,
}

impl StreamDecoder {
    /// Creates a decoder at `source_offset`.
    ///
    /// Byte-order-mark probing is enabled only when `new_stream_start` is
    /// true and `source_offset` is zero.
    #[must_use]
    pub fn new(
        encoding: Encoding,
        policy: OnDecodeError,
        source_offset: u64,
        new_stream_start: bool,
    ) -> Self {
        let state = match encoding {
            Encoding::Utf8 => DecoderState::Utf8(Utf8State::default()),
            Encoding::Ascii => DecoderState::Ascii,
            Encoding::Utf16Le => DecoderState::Utf16(Utf16State::new(Endian::Little)),
            Encoding::Utf16Be => DecoderState::Utf16(Utf16State::new(Endian::Big)),
            Encoding::Raw => DecoderState::Raw,
        };
        let bom_probe = (new_stream_start && source_offset == 0 && encoding != Encoding::Raw)
            .then(|| BomProbe::new(source_offset, encoding));

        Self {
            policy,
            state,
            bom_probe,
            replay: ReplayBytes::default(),
            next_input_offset: source_offset,
            delivered_boundary: source_offset,
        }
    }

    /// Decodes until one event is available or the supplied input is consumed.
    ///
    /// The caller must advance `input_offset` and its input slice by
    /// [`DecodeStep::consumed`] before calling this method again.
    pub fn next(&mut self, input_offset: u64, input: &[u8]) -> Result<DecodeStep, DecodeError> {
        if input_offset != self.next_input_offset {
            return Err(DecodeError::OffsetDiscontinuity {
                expected: self.next_input_offset,
                actual: input_offset,
            });
        }

        let mut consumed = 0;
        match self.next_inner(input, &mut consumed) {
            Ok(event) => {
                if let Some(event) = event {
                    self.delivered_boundary = event.range().end;
                }
                Ok(DecodeStep { consumed, event })
            }
            Err(DecodeError::SourceOffsetOverflow) if consumed != 0 => Ok(DecodeStep {
                consumed,
                event: None,
            }),
            Err(error) => Err(error),
        }
    }

    /// Returns the source offset expected for the next caller-owned input byte.
    #[must_use]
    pub const fn next_expected_input_offset(&self) -> u64 {
        self.next_input_offset
    }

    /// Returns the highest source boundary represented by an event already
    /// returned to the caller.
    ///
    /// The decoder can consume bytes before returning all events they
    /// produce. Call [`Self::next`] with empty input until it returns no event
    /// before treating this as an encoding-complete boundary.
    #[must_use]
    pub const fn highest_delivered_source_boundary(&self) -> u64 {
        self.delivered_boundary
    }

    /// Returns the start of source bytes consumed but not yet represented by
    /// a returned event.
    ///
    /// Pending bytes can be either an incomplete encoding unit or a complete
    /// internally buffered event. Drain empty-input events before deciding
    /// that more source bytes are required.
    #[must_use]
    pub const fn pending_source_start(&self) -> Option<u64> {
        if self.delivered_boundary < self.next_input_offset {
            Some(self.delivered_boundary)
        } else {
            None
        }
    }

    /// Returns the earliest source offset that must remain uncommitted.
    #[must_use]
    pub const fn earliest_uncommittable_offset(&self) -> Option<u64> {
        self.pending_source_start()
    }

    fn next_inner(
        &mut self,
        input: &[u8],
        consumed: &mut usize,
    ) -> Result<Option<DecodeEvent>, DecodeError> {
        if self.bom_probe.is_some() {
            let progress = {
                let mut cursor = ByteCursor {
                    replay: &mut self.replay,
                    next_input_offset: &mut self.next_input_offset,
                    input,
                    consumed,
                };
                process_bom_probe(&mut self.bom_probe, self.policy, &mut cursor)?
            };
            match progress {
                BomProgress::Pending => return Ok(None),
                BomProgress::Event(event) => return Ok(Some(event)),
                BomProgress::Continue => {}
            }
        }

        let mut cursor = ByteCursor {
            replay: &mut self.replay,
            next_input_offset: &mut self.next_input_offset,
            input,
            consumed,
        };
        match &mut self.state {
            DecoderState::Utf8(state) => decode_utf8(state, self.policy, &mut cursor),
            DecoderState::Ascii => decode_ascii(self.policy, &mut cursor),
            DecoderState::Utf16(state) => decode_utf16(state, self.policy, &mut cursor),
            DecoderState::Raw => decode_raw(&mut cursor),
        }
    }
}

#[derive(Debug)]
enum DecoderState {
    Utf8(Utf8State),
    Ascii,
    Utf16(Utf16State),
    Raw,
}

#[derive(Debug, Default)]
struct Utf8State {
    bytes: [u8; 4],
    len: u8,
    start: u64,
}

#[derive(Clone, Copy, Debug)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    const fn decode(self, bytes: [u8; 2]) -> u16 {
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Utf16Unit {
    start: u64,
    end: u64,
    bytes: [u8; 2],
    value: u16,
}

#[derive(Debug)]
struct Utf16State {
    endian: Endian,
    odd: Option<(u64, u8)>,
    high: Option<Utf16Unit>,
    queued: Option<Utf16Unit>,
}

impl Utf16State {
    const fn new(endian: Endian) -> Self {
        Self {
            endian,
            odd: None,
            high: None,
            queued: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ByteOrigin {
    Replay,
    Input,
}

#[derive(Clone, Copy, Debug)]
struct NextByte {
    byte: u8,
    offset: u64,
    origin: ByteOrigin,
}

struct ByteCursor<'a> {
    replay: &'a mut ReplayBytes,
    next_input_offset: &'a mut u64,
    input: &'a [u8],
    consumed: &'a mut usize,
}

impl ByteCursor<'_> {
    fn peek(&self) -> Result<Option<NextByte>, DecodeError> {
        if let Some((byte, offset)) = self.replay.peek()? {
            return Ok(Some(NextByte {
                byte,
                offset,
                origin: ByteOrigin::Replay,
            }));
        }
        Ok(self
            .input
            .get(*self.consumed)
            .copied()
            .map(|byte| NextByte {
                byte,
                offset: *self.next_input_offset,
                origin: ByteOrigin::Input,
            }))
    }

    fn consume(&mut self, next: NextByte) -> Result<(), DecodeError> {
        match next.origin {
            ByteOrigin::Replay => self.replay.consume(),
            ByteOrigin::Input => {
                let next_offset = self
                    .next_input_offset
                    .checked_add(1)
                    .ok_or(DecodeError::SourceOffsetOverflow)?;
                *self.next_input_offset = next_offset;
                *self.consumed += 1;
            }
        }
        Ok(())
    }

    fn take(&mut self) -> Result<Option<NextByte>, DecodeError> {
        let Some(next) = self.peek()? else {
            return Ok(None);
        };
        self.consume(next)?;
        Ok(Some(next))
    }
}

#[derive(Debug, Default)]
struct ReplayBytes {
    bytes: [u8; 3],
    start: u64,
    len: u8,
    pos: u8,
}

impl ReplayBytes {
    fn load(&mut self, start: u64, bytes: &[u8]) {
        debug_assert!(bytes.len() <= self.bytes.len());
        debug_assert!(self.pos == self.len);
        self.bytes[..bytes.len()].copy_from_slice(bytes);
        self.start = start;
        self.len = bytes.len() as u8;
        self.pos = 0;
    }

    fn peek(&self) -> Result<Option<(u8, u64)>, DecodeError> {
        if self.pos == self.len {
            return Ok(None);
        }
        let offset = self
            .start
            .checked_add(u64::from(self.pos))
            .ok_or(DecodeError::SourceOffsetOverflow)?;
        Ok(Some((self.bytes[usize::from(self.pos)], offset)))
    }

    fn consume(&mut self) {
        debug_assert!(self.pos < self.len);
        self.pos += 1;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BomKind {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl BomKind {
    const ALL: [Self; 3] = [Self::Utf8, Self::Utf16Le, Self::Utf16Be];

    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Utf8 => &[0xef, 0xbb, 0xbf],
            Self::Utf16Le => &[0xff, 0xfe],
            Self::Utf16Be => &[0xfe, 0xff],
        }
    }

    const fn matches(self, encoding: Encoding) -> bool {
        matches!(
            (self, encoding),
            (Self::Utf8, Encoding::Utf8)
                | (Self::Utf16Le, Encoding::Utf16Le)
                | (Self::Utf16Be, Encoding::Utf16Be)
        )
    }
}

#[derive(Debug)]
struct BomProbe {
    bytes: [u8; 3],
    len: u8,
    start: u64,
    encoding: Encoding,
}

impl BomProbe {
    const fn new(start: u64, encoding: Encoding) -> Self {
        Self {
            bytes: [0; 3],
            len: 0,
            start,
            encoding,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    fn push(&mut self, byte: u8) {
        debug_assert!(usize::from(self.len) < self.bytes.len());
        self.bytes[usize::from(self.len)] = byte;
        self.len += 1;
    }
}

enum BomProgress {
    Pending,
    Continue,
    Event(DecodeEvent),
}

fn process_bom_probe(
    probe: &mut Option<BomProbe>,
    policy: OnDecodeError,
    cursor: &mut ByteCursor<'_>,
) -> Result<BomProgress, DecodeError> {
    loop {
        let Some(next) = cursor.take()? else {
            return Ok(BomProgress::Pending);
        };
        let active = probe
            .as_mut()
            .expect("BOM processing requires an active probe");
        active.push(next.byte);

        if let Some(kind) = BomKind::ALL
            .into_iter()
            .find(|kind| kind.bytes() == active.as_slice())
        {
            let active = probe.take().expect("the active BOM probe is present");
            let range = SourceRange {
                start: active.start,
                end: *cursor.next_input_offset,
            };
            if kind.matches(active.encoding) {
                return Ok(BomProgress::Event(DecodeEvent::StrippedBom { range }));
            }
            let source = SourceBytes::from_slice(active.as_slice());
            return malformed_event(policy, range, source).map(BomProgress::Event);
        }

        if BomKind::ALL
            .into_iter()
            .any(|kind| kind.bytes().starts_with(active.as_slice()))
        {
            continue;
        }

        let active = probe.take().expect("the active BOM probe is present");
        cursor.replay.load(active.start, active.as_slice());
        return Ok(BomProgress::Continue);
    }
}

fn decode_utf8(
    state: &mut Utf8State,
    policy: OnDecodeError,
    cursor: &mut ByteCursor<'_>,
) -> Result<Option<DecodeEvent>, DecodeError> {
    loop {
        let Some(next) = cursor.peek()? else {
            return Ok(None);
        };
        let pending_len = usize::from(state.len);
        let mut candidate = state.bytes;
        candidate[pending_len] = next.byte;
        let candidate_len = pending_len + 1;

        enum Decision {
            Scalar(char),
            Incomplete,
            Malformed(usize),
        }

        let decision = match str::from_utf8(&candidate[..candidate_len]) {
            Ok(valid) => Decision::Scalar(
                valid
                    .chars()
                    .next()
                    .expect("a UTF-8 candidate always contains one byte"),
            ),
            Err(error) => match error.error_len() {
                Some(len) => Decision::Malformed(len),
                None => Decision::Incomplete,
            },
        };

        match decision {
            Decision::Scalar(value) => {
                cursor.consume(next)?;
                let start = if state.len == 0 {
                    next.offset
                } else {
                    state.start
                };
                let source = SourceBytes::from_slice(&candidate[..candidate_len]);
                state.len = 0;
                let range = SourceRange {
                    start,
                    end: next
                        .offset
                        .checked_add(1)
                        .ok_or(DecodeError::SourceOffsetOverflow)?,
                };
                return Ok(Some(DecodeEvent::Unit {
                    range,
                    source,
                    value: DecodedValue::Scalar(value),
                    malformed: false,
                }));
            }
            Decision::Incomplete => {
                cursor.consume(next)?;
                if state.len == 0 {
                    state.start = next.offset;
                }
                state.bytes[pending_len] = next.byte;
                state.len += 1;
            }
            Decision::Malformed(error_len) if error_len == candidate_len => {
                cursor.consume(next)?;
                let start = if state.len == 0 {
                    next.offset
                } else {
                    state.start
                };
                state.len = 0;
                let range = SourceRange {
                    start,
                    end: next
                        .offset
                        .checked_add(1)
                        .ok_or(DecodeError::SourceOffsetOverflow)?,
                };
                let source = SourceBytes::from_slice(&candidate[..error_len]);
                return malformed_event(policy, range, source).map(Some);
            }
            Decision::Malformed(error_len) => {
                debug_assert_eq!(error_len, pending_len);
                let start = state.start;
                state.len = 0;
                let end = start
                    .checked_add(error_len as u64)
                    .ok_or(DecodeError::SourceOffsetOverflow)?;
                let range = SourceRange { start, end };
                let source = SourceBytes::from_slice(&candidate[..error_len]);
                return malformed_event(policy, range, source).map(Some);
            }
        }
    }
}

fn decode_ascii(
    policy: OnDecodeError,
    cursor: &mut ByteCursor<'_>,
) -> Result<Option<DecodeEvent>, DecodeError> {
    let Some(next) = cursor.take()? else {
        return Ok(None);
    };
    let range = one_byte_range(next.offset)?;
    let source = SourceBytes::from_slice(&[next.byte]);
    if next.byte <= 0x7f {
        Ok(Some(DecodeEvent::Unit {
            range,
            source,
            value: DecodedValue::Scalar(char::from(next.byte)),
            malformed: false,
        }))
    } else {
        malformed_event(policy, range, source).map(Some)
    }
}

fn decode_utf16(
    state: &mut Utf16State,
    policy: OnDecodeError,
    cursor: &mut ByteCursor<'_>,
) -> Result<Option<DecodeEvent>, DecodeError> {
    loop {
        let Some(unit) = next_utf16_unit(state, cursor)? else {
            return Ok(None);
        };

        if let Some(high) = state.high.take() {
            if is_low_surrogate(unit.value) {
                let high_value = u32::from(high.value - 0xd800);
                let low_value = u32::from(unit.value - 0xdc00);
                let scalar = 0x1_0000 + (high_value << 10) + low_value;
                let value =
                    char::from_u32(scalar).expect("a UTF-16 surrogate pair is a Unicode scalar");
                return Ok(Some(DecodeEvent::Unit {
                    range: SourceRange {
                        start: high.start,
                        end: unit.end,
                    },
                    source: SourceBytes::from_pair(&high.bytes, &unit.bytes),
                    value: DecodedValue::Scalar(value),
                    malformed: false,
                }));
            }

            state.queued = Some(unit);
            let range = SourceRange {
                start: high.start,
                end: high.end,
            };
            let source = SourceBytes::from_slice(&high.bytes);
            return malformed_event(policy, range, source).map(Some);
        }

        if is_high_surrogate(unit.value) {
            state.high = Some(unit);
            continue;
        }
        if is_low_surrogate(unit.value) {
            let range = SourceRange {
                start: unit.start,
                end: unit.end,
            };
            let source = SourceBytes::from_slice(&unit.bytes);
            return malformed_event(policy, range, source).map(Some);
        }

        let value =
            char::from_u32(u32::from(unit.value)).expect("a non-surrogate u16 is a Unicode scalar");
        return Ok(Some(DecodeEvent::Unit {
            range: SourceRange {
                start: unit.start,
                end: unit.end,
            },
            source: SourceBytes::from_slice(&unit.bytes),
            value: DecodedValue::Scalar(value),
            malformed: false,
        }));
    }
}

fn next_utf16_unit(
    state: &mut Utf16State,
    cursor: &mut ByteCursor<'_>,
) -> Result<Option<Utf16Unit>, DecodeError> {
    if let Some(unit) = state.queued.take() {
        return Ok(Some(unit));
    }

    if state.odd.is_none() {
        let Some(first) = cursor.take()? else {
            return Ok(None);
        };
        state.odd = Some((first.offset, first.byte));
    }

    let Some(second) = cursor.take()? else {
        return Ok(None);
    };
    let (start, first) = state
        .odd
        .take()
        .expect("a UTF-16 second byte requires a first byte");
    let bytes = [first, second.byte];
    let end = second
        .offset
        .checked_add(1)
        .ok_or(DecodeError::SourceOffsetOverflow)?;
    Ok(Some(Utf16Unit {
        start,
        end,
        bytes,
        value: state.endian.decode(bytes),
    }))
}

fn decode_raw(cursor: &mut ByteCursor<'_>) -> Result<Option<DecodeEvent>, DecodeError> {
    let Some(next) = cursor.take()? else {
        return Ok(None);
    };
    Ok(Some(DecodeEvent::Unit {
        range: one_byte_range(next.offset)?,
        source: SourceBytes::from_slice(&[next.byte]),
        value: DecodedValue::RawByte(next.byte),
        malformed: false,
    }))
}

const fn is_high_surrogate(value: u16) -> bool {
    value >= 0xd800 && value <= 0xdbff
}

const fn is_low_surrogate(value: u16) -> bool {
    value >= 0xdc00 && value <= 0xdfff
}

fn one_byte_range(start: u64) -> Result<SourceRange, DecodeError> {
    let end = start
        .checked_add(1)
        .ok_or(DecodeError::SourceOffsetOverflow)?;
    Ok(SourceRange { start, end })
}

fn malformed_event(
    policy: OnDecodeError,
    range: SourceRange,
    source: SourceBytes,
) -> Result<DecodeEvent, DecodeError> {
    match policy {
        OnDecodeError::PreserveRaw | OnDecodeError::Replace => Ok(DecodeEvent::Unit {
            range,
            source,
            value: DecodedValue::Scalar('\u{fffd}'),
            malformed: true,
        }),
        OnDecodeError::Fail => Err(DecodeError::FatalMalformed {
            range,
            source_bytes: source,
        }),
    }
}

#[cfg(test)]
#[path = "decoder/tests.rs"]
mod tests;
